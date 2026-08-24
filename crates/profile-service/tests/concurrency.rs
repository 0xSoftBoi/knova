//! Requirement 1: concurrent updates to the same profile must not corrupt state.
//!
//! Three distinct properties, tested separately because they fail differently.
//!
//! 1. [`stale_update_is_rejected`] — the lost-update problem. A mutex does not
//!    solve this, so it is the test that justifies the version field existing.
//! 2. [`concurrent_increments_never_lose_an_update`] — under real parallel
//!    load, every writer's contribution survives.
//! 3. [`concurrent_writes_are_never_torn`] — no reader ever observes one
//!    writer's address paired with another's phone number.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{Version, headers};
use profile_service::{AppState, IdempotencyStore, InMemoryProfiles};
use tower::ServiceExt;

const INTERNAL_TOKEN: &str = "test-internal-token";
const USER: &str = "user-under-test";

fn app() -> Router {
    profile_service::router(AppState {
        profiles: Arc::new(InMemoryProfiles::new()),
        internal_token: Arc::from(INTERNAL_TOKEN),
        idempotency: Arc::new(IdempotencyStore::default()),
    })
}

/// Sends one request as the gateway would.
async fn call(
    app: &Router,
    method: &str,
    if_match: Option<Version>,
    body: Option<String>,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri("/profile")
        .header(headers::INTERNAL_TOKEN, INTERNAL_TOKEN)
        .header(headers::USER_ID, USER)
        .header(header::CONTENT_TYPE, "application/json");

    if let Some(version) = if_match {
        builder = builder.header(header::IF_MATCH, version.to_string());
    }

    let request = builder
        .body(body.map_or_else(Body::empty, Body::from))
        .expect("request is well-formed");

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router is infallible");

    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body is finite");

    (
        status,
        response_headers,
        String::from_utf8(bytes.to_vec()).expect("body is UTF-8"),
    )
}

/// Both fields carry the same marker, in forms that pass validation. A reader
/// that ever sees the two disagree has observed a torn write.
fn body_for(marker: usize) -> String {
    format!(r#"{{"address":"address-{marker}","phone_number":"555{marker:04}"}}"#)
}

fn markers_of(body: &str) -> (usize, usize) {
    let profile: serde_json::Value = serde_json::from_str(body).expect("body is JSON");

    let address = profile["address"]
        .as_str()
        .expect("address is a string")
        .trim_start_matches("address-")
        .parse()
        .expect("address carries a marker");
    let phone = profile["phone_number"].as_str().expect("phone is a string")[3..]
        .parse()
        .expect("phone carries a marker");

    (address, phone)
}

/// Reads the version out of an `ETag` response header.
fn version_of(headers: &HeaderMap) -> Version {
    headers
        .get(header::ETAG)
        .expect("every 2xx and 412 carries an ETag")
        .to_str()
        .expect("ETag is ASCII")
        .parse()
        .expect("ETag holds a version")
}

#[tokio::test]
async fn stale_update_is_rejected() {
    let app = app();

    let (status, headers, _) = call(&app, "POST", None, Some(body_for(0))).await;
    assert_eq!(status, StatusCode::CREATED);

    // Both clients read the same version, as two browser tabs would.
    let observed = version_of(&headers);

    // First writer wins.
    let (status, headers, _) = call(&app, "PUT", Some(observed), Some(body_for(1))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        version_of(&headers),
        observed.checked_next().expect("no overflow")
    );

    // Second writer is holding a version that is now stale. Without the
    // precondition this would return 200 and erase writer one's change, with
    // neither client ever learning that anything was lost.
    let (status, headers, _) = call(&app, "PUT", Some(observed), Some(body_for(2))).await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "a write against a stale version must be refused, not silently applied"
    );
    assert_eq!(
        version_of(&headers),
        observed.checked_next().expect("no overflow"),
        "the 412 must report the current version so the client can retry without a re-read"
    );

    // The winner's data survived intact.
    let (_, _, body) = call(&app, "GET", None, None).await;
    assert!(
        body.contains("address-1"),
        "writer one's update was lost: {body}"
    );
    assert!(
        !body.contains("address-2"),
        "the stale write was applied anyway: {body}"
    );
}

#[tokio::test]
async fn unconditional_update_is_refused() {
    let app = app();
    call(&app, "POST", None, Some(body_for(0))).await;

    let (status, ..) = call(&app, "PUT", None, Some(body_for(1))).await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_REQUIRED,
        "a PUT with no If-Match cannot express which version it intends to replace, \
         so it must be refused rather than treated as last-write-wins"
    );
}

/// Stores `counter` in `address`, so a write's content depends on the read that
/// preceded it.
fn counter_body(counter: usize) -> String {
    format!(r#"{{"address":"counter-{counter}","phone_number":"5550100"}}"#)
}

fn counter_of(body: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(body).expect("body is JSON")["address"]
        .as_str()
        .expect("address is a string")
        .trim_start_matches("counter-")
        .parse()
        .expect("address holds a counter")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_increments_never_lose_an_update() {
    const WRITERS: usize = 64;

    let app = app();
    call(&app, "POST", None, Some(counter_body(0))).await;

    // Each writer reads the counter and writes back one more. This is what
    // makes a lost update *observable*: counting accepted writes cannot detect
    // one, because last-write-wins also accepts every write and also bumps the
    // version every time. Only a read-modify-write whose result depends on what
    // was read can tell the two designs apart — with last-write-wins, two
    // writers that read the same value both store value + 1, and one increment
    // silently disappears.
    let writers = (1..=WRITERS).map(|_| {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let (_, headers, body) = call(&app, "GET", None, None).await;
                let next = counter_of(&body) + 1;

                let (status, ..) = call(
                    &app,
                    "PUT",
                    Some(version_of(&headers)),
                    Some(counter_body(next)),
                )
                .await;
                if status == StatusCode::OK {
                    break;
                }
                // The only other outcome a contended writer may see is losing
                // the race; anything else means the store misbehaved.
                assert_eq!(status, StatusCode::PRECONDITION_FAILED);
            }
        })
    });

    for writer in writers.collect::<Vec<_>>() {
        writer.await.expect("no writer panicked");
    }

    let (_, headers, body) = call(&app, "GET", None, None).await;

    assert_eq!(
        counter_of(&body),
        WRITERS,
        "{WRITERS} writers each incremented once, so every increment must be present; \
         a lower value means updates were silently overwritten"
    );
    assert_eq!(
        version_of(&headers),
        (0..WRITERS).fold(Version::INITIAL, |version, _| {
            version.checked_next().expect("no overflow")
        }),
        "one create plus exactly one accepted update per writer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_writes_are_never_torn() {
    const WRITERS: usize = 64;

    let app = app();
    call(&app, "POST", None, Some(body_for(0))).await;

    let writers = (1..=WRITERS).map(|marker| {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let (_, headers, _) = call(&app, "GET", None, None).await;
                let (status, ..) = call(
                    &app,
                    "PUT",
                    Some(version_of(&headers)),
                    Some(body_for(marker)),
                )
                .await;
                if status == StatusCode::OK {
                    break;
                }
            }
        })
    });

    for writer in writers.collect::<Vec<_>>() {
        writer.await.expect("no writer panicked");
    }

    // Each writer submits both fields carrying its own marker. If any reader
    // could observe a half-applied write, the two would disagree.
    let (_, _, body) = call(&app, "GET", None, None).await;
    let (address_marker, phone_marker) = markers_of(&body);

    assert_eq!(
        address_marker, phone_marker,
        "torn write: address carried {address_marker} while phone carried {phone_marker}"
    );
}

#[tokio::test]
async fn direct_access_without_the_internal_token_is_refused() {
    let request = Request::builder()
        .method("GET")
        .uri("/profile")
        .header(headers::USER_ID, USER)
        .body(Body::empty())
        .expect("request is well-formed");

    let response = app().oneshot(request).await.expect("router is infallible");

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a caller that reaches the profile port directly must not be served, \
         even though it supplied a plausible user id"
    );
}

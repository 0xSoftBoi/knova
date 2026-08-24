//! Requirement 2: a failed login must not reveal whether the username exists.
//!
//! Two things have to hold, and only one of them is obvious.
//!
//! The obvious one is that the *responses* match — same status, same body, same
//! headers. The subtle one is that the *timing* matches. Argon2 takes tens of
//! milliseconds by design; an implementation that returns early on an unknown
//! username is fast on a miss and slow on a hit, and that gap is measurable
//! across a network. It turns the login endpoint into a "does this account
//! exist" oracle even though every response is byte-identical.
//!
//! [`indistinguishable_responses`] covers the first. [`no_early_return_for_unknown_user`]
//! covers the second without a flaky A-vs-B timing comparison: it asserts the
//! unknown-user path spends real Argon2 time, which is only possible if it
//! actually hashed something.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use auth_service::{AppState, KeyMaterial, LoginThrottle, TokenService, Upstream, UserDirectory};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

const USERNAME: &str = "alice";
const PASSWORD: &str = "correct-horse-battery-staple";

/// Built once: seeding hashes the password, which is deliberately expensive.
fn state() -> AppState {
    static STATE: OnceLock<AppState> = OnceLock::new();
    STATE
        .get_or_init(|| AppState {
            users: Arc::new(UserDirectory::with_seed_user(USERNAME, PASSWORD)),
            tokens: Arc::new(TokenService::new(
                &KeyMaterial::new("test", "test-secret"),
                &[],
                Duration::from_secs(900),
            )),
            throttle: Arc::new(LoginThrottle::default()),
            upstream: Upstream::new("http://127.0.0.1:0", "test-internal-token"),
            hash_permits: Arc::new(tokio::sync::Semaphore::new(
                auth_service::default_hash_permits(),
            )),
        })
        .clone()
}

/// One login attempt, returning everything a caller could observe.
async fn attempt(username: &str, password: &str) -> (StatusCode, Option<String>, Vec<u8>) {
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"username":"{username}","password":"{password}"}}"#
        )))
        .expect("request is well-formed");

    let response = auth_service::router(state())
        .oneshot(request)
        .await
        .expect("router is infallible");

    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body is finite");

    (status, content_type, body.to_vec())
}

#[tokio::test]
async fn indistinguishable_responses() {
    let unknown_user = attempt("no-such-account", "irrelevant").await;
    let wrong_password = attempt(USERNAME, "not-the-password").await;

    assert_eq!(unknown_user.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        unknown_user, wrong_password,
        "an unknown username and a wrong password must be observationally identical; \
         any difference in status, content type, or body is a user-enumeration oracle"
    );

    // Pin the exact bytes, so a future edit that adds a helpful detail such as
    // "user not found" to one branch fails here rather than shipping.
    assert_eq!(
        String::from_utf8_lossy(&unknown_user.2),
        r#"{"error":"invalid_credentials","message":"Invalid username or password."}"#
    );
}

#[tokio::test]
async fn no_early_return_for_unknown_user() {
    // Argon2id at m=19456 KiB cannot complete in under a millisecond on any
    // machine that exists. Spending real time here proves the unknown-user path
    // verified against the decoy hash instead of short-circuiting on the lookup.
    const FLOOR: Duration = Duration::from_millis(5);

    let started = Instant::now();
    let (status, ..) = attempt("no-such-account", "irrelevant").await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        elapsed >= FLOOR,
        "unknown-user login returned in {elapsed:?}, faster than one Argon2 verification; \
         the lookup miss is short-circuiting and leaks account existence by timing"
    );
}

#[tokio::test]
async fn valid_credentials_are_still_accepted() {
    let (status, _, body) = attempt(USERNAME, PASSWORD).await;

    assert_eq!(status, StatusCode::OK, "the control case must still pass");
    assert!(
        String::from_utf8_lossy(&body).contains("access_token"),
        "a successful login must return a token"
    );
}

#[tokio::test]
async fn profile_routes_require_a_token() {
    let request = Request::builder()
        .method("GET")
        .uri("/profile")
        .body(Body::empty())
        .expect("request is well-formed");

    let response = auth_service::router(state())
        .oneshot(request)
        .await
        .expect("router is infallible");

    // Rejected at the gateway, so the profile service is never contacted —
    // note this passes with nothing listening on the upstream port.
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

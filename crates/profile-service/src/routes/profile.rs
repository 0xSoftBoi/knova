//! Handlers for `/profile`.
//!
//! The concurrency contract lives in the status codes:
//!
//! | Request                     | Response                          |
//! |-----------------------------|-----------------------------------|
//! | `POST`, no profile yet      | `201` + `ETag: "1"`               |
//! | `POST`, profile exists      | `409`                             |
//! | `GET`                       | `200` + `ETag: "n"`, or `404`     |
//! | `PUT`, no `If-Match`        | `428`                             |
//! | `PUT`, `If-Match` stale     | `412` + `ETag: "n"` (the winner)  |
//! | `PUT`, `If-Match` current   | `200` + `ETag: "n+1"`             |

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::dto::{Profile, ProfileInput};
use std::sync::Arc;

use common::{UserId, Version};

use crate::AppState;
use crate::auth::Caller;
use crate::error::ProfileError;
use crate::idempotency::{Lookup, Recorded};
use crate::store::{Revision, Update};

/// Header carrying a client-chosen key that makes a retried create safe.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// `POST /profile` — create the calling user's profile.
///
/// Honours `Idempotency-Key`. Without one a timed-out client that retries
/// cannot tell "my create succeeded" from "someone else created this", because
/// both are `409`. With one, the retry replays the original response.
///
/// # Errors
///
/// [`ProfileError::Invalid`] if the payload fails validation,
/// [`ProfileError::AlreadyExists`] if the user already has a profile,
/// [`ProfileError::IdempotencyInFlight`] if an earlier request holds the same
/// key, and [`ProfileError::IdempotencyReused`] if the key was used before with
/// a different body.
pub async fn create(
    State(state): State<AppState>,
    Caller(user_id): Caller,
    request_headers: HeaderMap,
    body: String,
) -> Result<Response, ProfileError> {
    let key = request_headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .filter(|key| !key.is_empty())
        .map(str::to_owned);

    // Reserve before doing anything, so two concurrent retries cannot both run.
    if let Some(key) = &key {
        match state.idempotency.begin(user_id.as_str(), key, &body) {
            Lookup::Reserved => {}
            Lookup::InFlight => return Err(ProfileError::IdempotencyInFlight),
            Lookup::BodyMismatch => return Err(ProfileError::IdempotencyReused),
            Lookup::Replay(recorded) => return Ok(replay(&recorded)),
        }
    }

    let outcome = create_once(&state, &user_id, &body).await;

    match (&outcome, &key) {
        // A terminal outcome is recorded so the retry sees what the first
        // caller saw. Everything else releases the key: a transient fault must
        // not permanently consume it.
        (Ok(recorded), Some(key)) => {
            state
                .idempotency
                .finish(user_id.as_str(), key, &body, recorded.clone());
        }
        (Err(_), Some(key)) => state.idempotency.abandon(user_id.as_str(), key),
        _ => {}
    }

    outcome.map(|recorded| replay(&recorded))
}

/// Performs one create, rendering the outcome in replayable form.
async fn create_once(
    state: &AppState,
    user_id: &UserId,
    body: &str,
) -> Result<Recorded, ProfileError> {
    let input: ProfileInput =
        serde_json::from_str(body).map_err(|_| ProfileError::MalformedBody)?;
    input.validate()?;

    let created = state
        .profiles
        .create(user_id, input)
        .await?
        .ok_or(ProfileError::AlreadyExists)?;

    tracing::info!(user_id = %user_id, "profile created");

    Ok(Recorded {
        status: StatusCode::CREATED.as_u16(),
        etag: Some(created.version.to_string()),
        body: serde_json::to_string(&created).map_err(|_| ProfileError::MalformedBody)?,
    })
}

/// Renders a recorded outcome, whether it is being served for the first time or
/// replayed for a retry. One function, so the two cannot drift.
fn replay(recorded: &Recorded) -> Response {
    let status = StatusCode::from_u16(recorded.status).unwrap_or(StatusCode::CREATED);
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");

    if let Some(etag) = &recorded.etag {
        response = response.header(header::ETAG, etag);
    }

    response
        .body(recorded.body.clone().into())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// `GET /profile` — read the calling user's profile.
///
/// # Errors
///
/// [`ProfileError::NotFound`] if the user has no profile.
pub async fn read(
    State(state): State<AppState>,
    Caller(user_id): Caller,
) -> Result<impl IntoResponse, ProfileError> {
    let profile = state
        .profiles
        .get(&user_id)
        .await?
        .ok_or(ProfileError::NotFound)?;
    Ok(with_etag(StatusCode::OK, profile))
}

/// `PUT /profile` — replace the calling user's profile, if unchanged.
///
/// Requires `If-Match` carrying the version the caller believes it is
/// replacing. Without it a client cannot express "I intend to overwrite exactly
/// what I read", and a lost update becomes indistinguishable from a successful
/// one, so the header is mandatory rather than optional.
///
/// # Errors
///
/// [`ProfileError::PreconditionRequired`] without `If-Match`,
/// [`ProfileError::MalformedPrecondition`] if it does not parse,
/// [`ProfileError::PreconditionFailed`] if the version is stale, and
/// [`ProfileError::NotFound`] if there is no profile to replace.
pub async fn update(
    State(state): State<AppState>,
    Caller(user_id): Caller,
    request_headers: HeaderMap,
    Json(input): Json<ProfileInput>,
) -> Result<impl IntoResponse, ProfileError> {
    input.validate()?;
    let expected = parse_if_match(&request_headers)?;

    match state.profiles.update(&user_id, expected, input).await? {
        Update::Applied(profile) => {
            tracing::info!(user_id = %user_id, version = %profile.version, "profile updated");
            Ok(with_etag(StatusCode::OK, profile))
        }
        Update::Stale { current_version } => {
            tracing::info!(user_id = %user_id, expected = %expected, current = %current_version, "update rejected as stale");
            Err(ProfileError::PreconditionFailed { current_version })
        }
        Update::Missing => Err(ProfileError::NotFound),
    }
}

/// Attaches the profile's version as an `ETag`, so the client's next `PUT` can
/// quote it back without guessing.
///
/// Takes the shared projection rather than an owned `Profile`: the store hands
/// out an [`Arc`] so a read costs one refcount bump, and cloning it here to
/// serialise would give that saving straight back.
fn with_etag(status: StatusCode, profile: Arc<Profile>) -> impl IntoResponse {
    (
        status,
        [(header::ETAG, profile.version.to_string())],
        Json(profile),
    )
}

/// Reads the version out of an `If-Match` header.
///
/// The accepted syntax lives on [`Version`]'s [`FromStr`] impl, so this and the
/// `ETag` it round-trips from cannot disagree.
///
/// [`FromStr`]: std::str::FromStr
fn parse_if_match(headers: &HeaderMap) -> Result<Version, ProfileError> {
    headers
        .get(header::IF_MATCH)
        .ok_or(ProfileError::PreconditionRequired)?
        .to_str()
        .ok()
        .and_then(|raw| raw.parse().ok())
        .ok_or(ProfileError::MalformedPrecondition)
}

/// `GET /profile/history` — every accepted revision, oldest first.
///
/// The audit view. A profile is a projection of its revisions, so this is the
/// underlying record rather than a derived report: an address change is a fraud
/// and KYC signal, and the prior value is evidence.
///
/// # Errors
///
/// [`ProfileError::NotFound`] if the user has no profile.
pub async fn history(
    State(state): State<AppState>,
    Caller(user_id): Caller,
) -> Result<Json<Vec<Revision>>, ProfileError> {
    let revisions = state.profiles.history(&user_id).await?;

    if revisions.is_empty() {
        return Err(ProfileError::NotFound);
    }

    Ok(Json(revisions))
}

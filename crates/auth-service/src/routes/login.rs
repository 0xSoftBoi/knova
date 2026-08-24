//! The `POST /login` and `POST /logout` handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use common::dto::{LoginRequest, LoginResponse};

use crate::AppState;
use crate::error::AppError;
use crate::extract::{Authenticated, PeerAddress};
use crate::throttle::Verdict;
use crate::users::Authentication;

/// Password used to force a rejection that costs exactly what a real one costs.
///
/// A throttled account must not answer faster than an unthrottled one, or the
/// throttle becomes the user-enumeration oracle the login path is built to
/// avoid. Verifying this instead of the caller's password does the same Argon2
/// work and cannot succeed: no account holds it, because it is not a legal
/// password to register.
const UNMATCHABLE: &str = "\0throttled\0";

/// Exchanges a username and password for a bearer token.
///
/// # Errors
///
/// Returns [`AppError::InvalidCredentials`] if the account does not exist, the
/// password is wrong, *or* the account is throttled — all three are one
/// response. [`AppError::AddressThrottled`] if the caller's address has failed
/// too often, [`AppError::Overloaded`] if verification capacity is exhausted,
/// and [`AppError::BlockingTask`] or [`AppError::TokenSigning`] on faults.
pub async fn login(
    State(state): State<AppState>,
    PeerAddress(address): PeerAddress,
    Json(LoginRequest { username, password }): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    // Shed rather than queue. Waiting for a permit would trade a bounded memory
    // problem for an unbounded latency one, and a caller who is going to be
    // told "try again" is better told immediately. Acquired before the lookup,
    // so saturation is refused identically for every username.
    let permit = Arc::clone(&state.hash_permits)
        .try_acquire_owned()
        .map_err(|_| AppError::Overloaded)?;

    let verdict = state.throttle.check(&username, address);
    if verdict == Verdict::AddressThrottled {
        return Err(AppError::AddressThrottled);
    }

    // A throttled account still pays for a full verification, against a
    // password nothing can match. Same work, same elapsed time, same `401`.
    let offered = if verdict == Verdict::AccountThrottled {
        UNMATCHABLE.to_owned()
    } else {
        password
    };

    let users = Arc::clone(&state.users);
    let subject = username.clone();

    // Argon2 is deliberately expensive and CPU-bound. Running it on a runtime
    // worker would stall every other connection that worker is driving.
    let outcome = tokio::task::spawn_blocking(move || {
        let outcome = users.authenticate(&subject, &offered);
        drop(permit);
        outcome
    })
    .await?;

    let Authentication::Authenticated(user) = outcome else {
        state.throttle.record_failure(&username, address);
        return Err(AppError::InvalidCredentials);
    };

    state.throttle.record_success(&username);
    tracing::info!(username = user.username(), "login succeeded");

    Ok(Json(LoginResponse {
        access_token: state.tokens.issue(user.id(), user.username())?,
        token_type: std::borrow::Cow::Borrowed("Bearer"),
        expires_in: state.tokens.ttl_secs(),
    }))
}

/// Revokes the presented token immediately.
///
/// Expiry alone leaves a stolen token usable for its full lifetime. Idempotent:
/// revoking an already-revoked token would need a valid token to authenticate
/// with, and a revoked one no longer is, so the second call simply fails
/// authentication like any other unusable token.
///
/// # Errors
///
/// Returns [`AppError::MissingBearer`] or [`AppError::InvalidToken`] if the
/// caller is not authenticated.
pub async fn logout(
    State(state): State<AppState>,
    Authenticated(claims): Authenticated,
) -> StatusCode {
    tracing::info!(username = claims.username, "token revoked");
    state.tokens.revoke(&claims);

    StatusCode::NO_CONTENT
}

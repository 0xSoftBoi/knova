//! The service-wide error type and its single HTTP mapping.

use std::borrow::Cow;

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::dto::ErrorResponse;
use common::{InvalidProfile, Version};

use crate::store::StoreError;

/// Anything a handler can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// The internal token was absent or wrong: the caller is not the gateway.
    #[error("caller is not the gateway")]
    NotInternal,

    /// The gateway forwarded a request with no user id header. This is a bug in
    /// the gateway, never something a client can cause, so it is a `500`.
    #[error("internal request carried no user id")]
    MissingCaller,

    /// No profile exists for the calling user.
    #[error("profile not found")]
    NotFound,

    /// A profile already exists and `POST` will not overwrite it.
    #[error("profile already exists")]
    AlreadyExists,

    /// `PUT` arrived without an `If-Match` header.
    #[error("If-Match is required")]
    PreconditionRequired,

    /// The `If-Match` version did not match the stored one.
    #[error("If-Match version is stale")]
    PreconditionFailed {
        /// The version actually stored, returned so the client can retry.
        current_version: Version,
    },

    /// An `If-Match` header was present but not a valid version.
    #[error("If-Match is malformed")]
    MalformedPrecondition,

    /// The request body was not the JSON this endpoint expects.
    #[error("request body is not valid JSON for this endpoint")]
    MalformedBody,

    /// Another request is currently holding this idempotency key.
    #[error("a request with this idempotency key is in progress")]
    IdempotencyInFlight,

    /// This idempotency key was used before with a different body.
    #[error("idempotency key reused with a different request body")]
    IdempotencyReused,

    /// The payload failed the invariants on [`ProfileInput`].
    ///
    /// [`ProfileInput`]: common::dto::ProfileInput
    #[error(transparent)]
    Invalid(#[from] InvalidProfile),

    /// The storage backend failed.
    #[error("storage fault")]
    Store(#[from] StoreError),
}

impl ProfileError {
    /// Maps the error to `(status, machine-readable code, client-safe message)`.
    fn parts(&self) -> (StatusCode, &'static str, Cow<'static, str>) {
        match self {
            Self::NotInternal => (
                StatusCode::FORBIDDEN,
                "forbidden",
                Cow::Borrowed("This service is not directly reachable."),
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                Cow::Borrowed("No profile exists for this user."),
            ),
            Self::AlreadyExists => (
                StatusCode::CONFLICT,
                "already_exists",
                Cow::Borrowed("A profile already exists; use PUT to update it."),
            ),
            Self::PreconditionRequired => (
                StatusCode::PRECONDITION_REQUIRED,
                "precondition_required",
                Cow::Borrowed("If-Match is required. Read the profile and retry with its ETag."),
            ),
            Self::PreconditionFailed { .. } => (
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
                Cow::Borrowed("The profile changed since you read it. Re-read and retry."),
            ),
            Self::MalformedPrecondition => (
                StatusCode::BAD_REQUEST,
                "malformed_precondition",
                Cow::Borrowed("If-Match must be an ETag such as \"3\"."),
            ),
            Self::MalformedBody => (
                StatusCode::BAD_REQUEST,
                "malformed_body",
                Cow::Borrowed("The request body must be a JSON profile."),
            ),
            Self::IdempotencyInFlight => (
                StatusCode::CONFLICT,
                "idempotency_in_flight",
                Cow::Borrowed(
                    "An earlier request with this Idempotency-Key is still running. Retry shortly.",
                ),
            ),
            Self::IdempotencyReused => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                Cow::Borrowed(
                    "This Idempotency-Key was already used with a different request body.",
                ),
            ),
            // The only variant whose message is not static: it names the field
            // the caller got wrong, which is information the caller supplied.
            Self::Invalid(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_profile",
                Cow::Owned(reason.to_string()),
            ),
            Self::MissingCaller | Self::Store(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                Cow::Borrowed("An unexpected error occurred."),
            ),
        }
    }
}

impl IntoResponse for ProfileError {
    fn into_response(self) -> Response {
        let (status, error, message) = self.parts();

        if status.is_server_error() {
            tracing::error!(error = ?self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }

        let body = Json(ErrorResponse {
            error: Cow::Borrowed(error),
            message,
        });

        // A 412 carries the current ETag so a client can retry without a
        // separate round-trip to discover the version it lost to.
        match self {
            Self::PreconditionFailed { current_version } => {
                (status, [(header::ETAG, current_version.to_string())], body).into_response()
            }
            _ => (status, body).into_response(),
        }
    }
}

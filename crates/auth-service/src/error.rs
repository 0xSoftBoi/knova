//! The service-wide error type and its single HTTP mapping.

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::dto::ErrorResponse;

/// Anything a handler can fail with.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The username was unknown, or the password was wrong. Deliberately one
    /// variant: see [`Authentication`](crate::Authentication).
    #[error("invalid credentials")]
    InvalidCredentials,

    /// No usable `Authorization: Bearer` header was present.
    #[error("missing bearer token")]
    MissingBearer,

    /// A bearer token was present but did not validate.
    #[error("invalid token")]
    InvalidToken,

    /// Claims could not be serialized or signed.
    #[error("token signing failed")]
    TokenSigning(#[source] jsonwebtoken::errors::Error),

    /// A `spawn_blocking` task panicked or was cancelled.
    #[error("blocking task failed")]
    BlockingTask(#[from] tokio::task::JoinError),

    /// The profile service could not be reached, or its response was unreadable.
    #[error("upstream request failed")]
    Upstream(#[source] Box<reqwest::Error>),

    /// Every password-verification permit is in use.
    #[error("password verification capacity exhausted")]
    Overloaded,

    /// The calling address has failed too many logins recently.
    ///
    /// Safe to report honestly: it depends on the caller's address, not on
    /// whether any account exists.
    #[error("too many failed logins from this address")]
    AddressThrottled,
}

impl AppError {
    /// Maps the error to `(status, machine-readable code, client-safe message)`.
    ///
    /// The only place that decides how an error appears on the wire, which is
    /// what makes the indistinguishability of the two credential failures
    /// auditable by reading a single function.
    fn parts(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "Invalid username or password.",
            ),
            Self::MissingBearer | Self::InvalidToken => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid bearer token is required.",
            ),
            Self::AddressThrottled => (
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_requests",
                "Too many failed attempts from this address. Retry later.",
            ),
            Self::Overloaded => (
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "The service is at capacity. Retry shortly.",
            ),
            Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream_unavailable",
                "The profile service is unavailable.",
            ),
            Self::TokenSigning(_) | Self::BlockingTask(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "An unexpected error occurred.",
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error, message) = self.parts();

        // Full detail to the log, sanitised detail to the client.
        if status.is_server_error() {
            tracing::error!(error = ?self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }

        let body = Json(ErrorResponse::new(error, message));

        // Retryable failures say when; the rest make the client guess.
        let retry_after = match self {
            Self::Overloaded => Some("1"),
            Self::AddressThrottled => Some("300"),
            _ => None,
        };

        if let Some(seconds) = retry_after {
            return (status, [(header::RETRY_AFTER, seconds)], body).into_response();
        }

        (status, body).into_response()
    }
}

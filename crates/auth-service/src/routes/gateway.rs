//! The API gateway hop.
//!
//! Every profile request enters here, is authenticated, and is replayed against
//! the profile service with the caller's identity attached out-of-band.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use common::headers;

use crate::AppState;
use crate::error::AppError;
use crate::extract::Authenticated;

/// Headers copied from the client request to the upstream request.
///
/// An allowlist, not a denylist. Everything else is dropped, which is what
/// stops a client from supplying its own `x-user-id` or `x-internal-token` and
/// impersonating either the gateway or another user — the spoofed values are
/// never copied, and the genuine ones are set afterwards.
const FORWARDED_TO_UPSTREAM: [&str; 3] = [
    "content-type",
    "if-match",
    // Client-chosen key that makes a retried create safe.
    "idempotency-key",
];

/// Headers copied from the upstream response back to the client.
const FORWARDED_TO_CLIENT: [HeaderName; 2] = [header::CONTENT_TYPE, header::ETAG];

/// Authenticates the caller and forwards the request to the profile service.
///
/// The upstream never validates a token. It is told *who* the caller is by a
/// header, and believes it only because the request also carries the shared
/// internal secret. That keeps token verification in exactly one service.
///
/// # Errors
///
/// Returns [`AppError::MissingBearer`] or [`AppError::InvalidToken`] if the
/// caller is not authenticated, and [`AppError::Upstream`] if the profile
/// service cannot be reached.
pub async fn proxy(
    State(state): State<AppState>,
    Authenticated(claims): Authenticated,
    method: Method,
    uri: Uri,
    client_headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let url = format!("{}{}", state.upstream.base_url, uri.path());

    let mut upstream = state
        .upstream
        .http
        .request(method, &url)
        .header(
            headers::INTERNAL_TOKEN,
            state.upstream.internal_token.as_ref(),
        )
        .header(headers::USER_ID, claims.sub.as_str());

    for name in FORWARDED_TO_UPSTREAM {
        if let Some(value) = client_headers.get(name) {
            upstream = upstream.header(name, value);
        }
    }

    // Correlation crosses the hop, so one id spans both services' logs. Set
    // after the allowlist because this value is the gateway's, not the
    // client's — `tracing_id::propagate` has already normalised it.
    if let Some(value) = client_headers.get(headers::REQUEST_ID) {
        upstream = upstream.header(headers::REQUEST_ID, value);
    }

    let response = upstream
        .body(body)
        .send()
        .await
        .map_err(|source| AppError::Upstream(Box::new(source)))?;

    let status = response.status();
    let mut builder = Response::builder().status(status);

    for name in FORWARDED_TO_CLIENT {
        if let Some(value) = response.headers().get(&name) {
            builder = builder.header(name, value);
        }
    }

    let payload = response
        .bytes()
        .await
        .map_err(|source| AppError::Upstream(Box::new(source)))?;

    // `body` only fails if a header copied above was malformed, which cannot
    // happen: every value came from a already-parsed `HeaderMap`.
    Ok(builder
        .body(payload.into())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

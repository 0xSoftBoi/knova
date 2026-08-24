//! Trust boundary for the internal hop.
//!
//! The profile service never sees a bearer token and never validates one. It
//! trusts exactly two headers, and only in this order: a shared secret proving
//! the caller is the gateway, then the user id the gateway derived from a token
//! it already verified. Collapsing those two checks into one place means no
//! handler can accidentally skip the first and still read the second.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use common::UserId;
use common::headers;
use subtle::ConstantTimeEq;
use tracing::Instrument;

use crate::AppState;
use crate::error::ProfileError;

/// The authenticated user, as asserted by the gateway.
///
/// Only meaningful because [`require_internal_token`] runs first; on its own
/// this header is client-controlled and worthless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller(pub UserId);

impl<S> FromRequestParts<S> for Caller
where
    S: Send + Sync,
{
    type Rejection = ProfileError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .headers
            .get(headers::USER_ID)
            .and_then(|value| value.to_str().ok())
            .filter(|id| !id.is_empty())
            .map(|id| Self(UserId::from(id)))
            .ok_or(ProfileError::MissingCaller)
    }
}

/// Establishes correlation, then rejects anything that is not the gateway.
///
/// Two concerns in one layer, deliberately. Each `from_fn` layer costs roughly
/// 700 ns of machinery — a boxed future and a state clone per request — which
/// measured as more than a third of the handler it wraps. Two layers therefore
/// cost more than the work they guard. Combining them keeps both guarantees and
/// removes one layer's worth of that overhead.
///
/// It stays a *layer* rather than folding into an extractor: a layer protects
/// every route beneath it, whereas an extractor only protects routes that
/// remember to ask for it. That difference is worth 700 ns — a route added
/// later must not be able to skip the check by omission.
///
/// The token comparison is constant-time. A byte-by-byte `==` short-circuits on
/// the first mismatch, which leaks the secret one character at a time to anyone
/// who can measure response latency across enough requests.
///
/// Correlation is established *before* the token check, so a rejected request
/// still appears in the logs under the id the caller quoted.
///
/// # Errors
///
/// Returns [`ProfileError::NotInternal`] if the token is absent or wrong.
pub async fn gateway_guard(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ProfileError> {
    let correlation = crate::tracing_id::inbound(request.headers());
    let span = crate::tracing_id::span(correlation.as_ref());

    let presented = request
        .headers()
        .get(headers::INTERNAL_TOKEN)
        .map_or(&b""[..], |value| value.as_bytes());

    if presented.ct_eq(state.internal_token.as_bytes()).unwrap_u8() != 1 {
        let _entered = span.enter();
        return Err(ProfileError::NotInternal);
    }

    // `Instrument`, not `span.enter()`. Entering a span makes it current for the
    // enclosing scope only; a future *built* inside that scope is polled later,
    // with the span long since exited, so the handler's own events would escape
    // it entirely. This is easy to write and invisible until you go looking for
    // a log line that should have been correlated.
    let mut response = next.run(request).instrument(span).await;
    crate::tracing_id::echo(response.headers_mut(), correlation);

    Ok(response)
}

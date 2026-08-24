//! Request correlation across the gateway hop.
//!
//! With two services, "grep both logs around the same timestamp" stops working
//! the moment there is more than one request in flight. Every request gets an
//! id — accepted from the caller when present, so a trace can start at the
//! client, and minted here otherwise — which is attached to the tracing span,
//! forwarded upstream, and echoed to the caller so a user reporting a failure
//! can quote it.

use axum::extract::Request;
use axum::http::{HeaderValue, header::HeaderName};
use axum::middleware::Next;
use axum::response::Response;
use common::headers;

/// Longest accepted inbound id.
///
/// Client-supplied values end up in log lines, so they are bounded and
/// restricted to characters that cannot break a log parser or smuggle a line
/// ending. Anything failing that is replaced rather than rejected: a malformed
/// correlation id is not worth failing a request over.
const MAX_LEN: usize = 64;

/// Attaches a request id to the span, the upstream call, and the response.
pub async fn propagate(mut request: Request, next: Next) -> Response {
    let header = HeaderName::from_static(headers::REQUEST_ID);

    let id = request
        .headers()
        .get(&header)
        .and_then(|value| value.to_str().ok())
        .filter(|id| is_acceptable(id))
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);

    // Put it back in normalised form so the gateway forwards exactly what it
    // logged, even when the caller supplied nothing or supplied junk.
    if let Ok(value) = HeaderValue::from_str(&id) {
        request.headers_mut().insert(header.clone(), value.clone());

        let span = tracing::info_span!("request", request_id = %id);
        let mut response = {
            let _entered = span.enter();
            next.run(request)
        }
        .await;

        response.headers_mut().insert(header, value);
        return response;
    }

    next.run(request).await
}

/// Whether a client-supplied id is safe to log and forward verbatim.
fn is_acceptable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausible_ids_are_kept() {
        assert!(is_acceptable("0199f2a1-3c4d-7e8f-9a0b-1c2d3e4f5a6b"));
        assert!(is_acceptable("req_12345"));
    }

    #[test]
    fn log_breaking_or_oversized_ids_are_rejected() {
        assert!(!is_acceptable(""));
        assert!(!is_acceptable("has space"));
        assert!(!is_acceptable("line\nbreak"));
        assert!(!is_acceptable(&"x".repeat(MAX_LEN + 1)));
    }
}

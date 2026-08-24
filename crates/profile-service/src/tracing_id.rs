//! Adopting the request id the gateway assigned.
//!
//! This service never mints one. If a request arrives without an id it did not
//! come through the gateway, and the internal-token check will refuse it a
//! moment later — so an absent id is a signal, not something to paper over with
//! a fresh value that correlates with nothing.
//!
//! These are helpers rather than a middleware of their own: see
//! [`auth::gateway_guard`](crate::auth::gateway_guard) for why.

use axum::http::{HeaderMap, HeaderValue, header::HeaderName};
use common::headers;

/// The correlation id the gateway attached, if any.
pub(crate) fn inbound(headers: &HeaderMap) -> Option<HeaderValue> {
    headers
        .get(HeaderName::from_static(headers::REQUEST_ID))
        .cloned()
}

/// A span carrying the correlation id, or marking its absence.
pub(crate) fn span(id: Option<&HeaderValue>) -> tracing::Span {
    // Recording an absence is more useful than inventing an id that correlates
    // with nothing.
    let id = id.map_or_else(
        || "none".into(),
        |value| String::from_utf8_lossy(value.as_bytes()),
    );
    tracing::info_span!("request", request_id = %id)
}

/// Echoes the correlation id back to the caller.
pub(crate) fn echo(headers: &mut HeaderMap, id: Option<HeaderValue>) {
    if let Some(value) = id {
        headers.insert(HeaderName::from_static(headers::REQUEST_ID), value);
    }
}

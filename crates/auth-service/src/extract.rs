//! Request extractors.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header;
use axum::http::request::Parts;

use crate::AppState;
use crate::error::AppError;
use crate::token::Claims;

/// A caller who presented a valid bearer token.
///
/// Implemented as an extractor rather than a helper function so that a handler
/// which needs an authenticated user says so *in its signature*. A route that
/// forgets to authenticate cannot compile against a `Claims` it never received.
#[derive(Debug, Clone)]
pub struct Authenticated(pub Claims);

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let value = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(AppError::MissingBearer)?;

        // RFC 9110 makes the auth scheme case-insensitive, so `bearer x` is as
        // valid as `Bearer x`. Matching with `strip_prefix("Bearer ")` would
        // reject conforming clients.
        let (scheme, token) = value.split_once(' ').ok_or(AppError::MissingBearer)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(AppError::MissingBearer);
        }

        Ok(Self(state.tokens.verify(token.trim())?))
    }
}

/// The source address of the request, when one is known.
///
/// An infallible extractor rather than `ConnectInfo<SocketAddr>` directly,
/// because the address is genuinely optional: tests drive the router in-process
/// with no connection behind it, and a handler that 500s in that case would
/// make the throttle untestable.
///
/// Deliberately *not* read from `X-Forwarded-For`. Behind a trusted proxy that
/// header is the real client and this one is the proxy, but trusting it
/// unconditionally lets any caller pick their own rate-limit bucket by
/// inventing a value. Honouring it is a deployment decision that belongs with
/// the proxy configuration that makes it safe.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddress(pub Option<IpAddr>);

impl<S> FromRequestParts<S> for PeerAddress
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(peer)| peer.ip()),
        ))
    }
}

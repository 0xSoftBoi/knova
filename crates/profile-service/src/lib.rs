//! Profile service.
//!
//! Stores one profile per user, behind the authorization service. It never sees
//! a bearer token: it trusts the shared internal secret checked by
//! [`auth::require_internal_token`], and the user id header that check gates.
//!
//! The concurrency design is documented on [`store`], which is the part of this
//! crate worth reading first.

pub mod auth;
pub mod config;
pub mod error;
pub mod idempotency;
pub mod routes;
pub mod store;
pub mod tracing_id;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

pub use config::{Backend, Config};
pub use error::ProfileError;
pub use idempotency::IdempotencyStore;
pub use store::{InMemoryProfiles, ProfileRepository, Revision, SqliteProfiles, Update};

/// State shared by every handler.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The profile store.
    ///
    /// A trait object so the backend is a deployment choice rather than a
    /// compile-time one: the in-memory implementation for a single process, the
    /// SQLite one wherever the version must be shared across replicas.
    pub profiles: Arc<dyn ProfileRepository>,
    /// Shared secret the gateway must present.
    pub internal_token: Arc<str>,
    /// Deduplicates retried creates.
    pub idempotency: Arc<IdempotencyStore>,
}

/// Builds the service's HTTP router.
///
/// Every route sits behind [`auth::require_internal_token`], applied as a layer
/// rather than called from each handler so a newly added route is protected by
/// default instead of by remembering.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route(
            "/profile",
            post(routes::profile::create)
                .get(routes::profile::read)
                .put(routes::profile::update),
        )
        .route("/profile/history", get(routes::profile::history))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::gateway_guard,
        ));

    Router::new()
        // Merged after the layer above, so the probe is reachable without the
        // shared secret while everything else stays behind it.
        .merge(protected)
        .route("/health", get(routes::health::health))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Resolves when the process is asked to stop.
///
/// Without this, `SIGTERM` from an orchestrator kills in-flight requests
/// mid-write during every rolling deploy.
///
/// # Panics
///
/// Panics if the process cannot install a signal handler, which indicates the
/// process lacks a capability it needs to run at all.
pub async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Ctrl+C handler must install");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler must install")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received, draining in-flight requests");
}

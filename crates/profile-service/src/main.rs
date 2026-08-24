//! Binary entry point. All behaviour lives in the library target.

use std::error::Error;
use std::sync::Arc;

use profile_service::{
    AppState, Backend, Config, IdempotencyStore, InMemoryProfiles, ProfileRepository,
    SqliteProfiles,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        // `info`, not `info,tower_http=debug`. The debug default emitted two
        // formatted lines per request — 320,000 lines across a 160,000-request
        // run — and cost 25% of gateway throughput plus 0.6 ms of p50. It also
        // fills a disk and puts request metadata in a log nobody meant to keep.
        // Operators opt in with RUST_LOG when they are debugging.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env();

    let profiles: Arc<dyn ProfileRepository> = match config.backend {
        Backend::Memory => Arc::new(InMemoryProfiles::new()),
        Backend::Sqlite => Arc::new(SqliteProfiles::in_memory()?),
    };

    let state = AppState {
        profiles,
        internal_token: Arc::from(config.internal_token.as_str()),
        idempotency: Arc::new(IdempotencyStore::default()),
    };

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = config.bind_addr, backend = ?config.backend, "profile-service listening");

    axum::serve(listener, profile_service::router(state))
        .with_graceful_shutdown(profile_service::shutdown_signal())
        .await?;

    Ok(())
}

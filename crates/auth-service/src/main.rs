//! Binary entry point. All behaviour lives in the library target.

use std::error::Error;
use std::sync::Arc;

use auth_service::{AppState, Config, TokenService, Upstream, UserDirectory, default_hash_permits};

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

    let config = Config::from_env()?;

    let state = AppState {
        users: Arc::new(UserDirectory::with_seed_user(
            &config.seed_username,
            &config.seed_password,
        )),
        tokens: Arc::new(TokenService::new(
            &config.active_key,
            &config.retired_keys,
            config.token_ttl,
        )),
        throttle: Arc::new(auth_service::LoginThrottle::default()),
        upstream: Upstream::new(&config.profile_service_url, &config.internal_token),
        hash_permits: Arc::new(tokio::sync::Semaphore::new(default_hash_permits())),
    };

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;

    tracing::info!(
        addr = config.bind_addr,
        seed_user = config.seed_username,
        active_kid = config.active_key.kid,
        "auth-service listening"
    );

    // `into_make_service_with_connect_info` is what makes the peer address
    // available to the throttle; without it `ConnectInfo` never extracts.
    axum::serve(
        listener,
        auth_service::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(auth_service::shutdown_signal())
    .await?;

    Ok(())
}

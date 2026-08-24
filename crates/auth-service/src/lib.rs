//! Authorization service.
//!
//! Issues bearer tokens for seeded credentials and, once the gateway routes are
//! wired, forwards authenticated requests to the profile service. Clients only
//! ever address this process.
//!
//! The library target exists so integration tests can drive [`router`] directly
//! with [`tower::ServiceExt::oneshot`] instead of binding a port:
//!
//! ```no_run
//! use auth_service::{AppState, Config, TokenService, Upstream, UserDirectory};
//! use std::sync::Arc;
//!
//! let config = Config::from_env()?;
//! let state = AppState {
//!     users: Arc::new(UserDirectory::with_seed_user(
//!         &config.seed_username,
//!         &config.seed_password,
//!     )),
//!     tokens: Arc::new(TokenService::new(
//!         &config.active_key,
//!         &config.retired_keys,
//!         config.token_ttl,
//!     )),
//!     throttle: Arc::new(auth_service::LoginThrottle::default()),
//!     upstream: Upstream::new(&config.profile_service_url, &config.internal_token),
//!     hash_permits: Arc::new(tokio::sync::Semaphore::new(
//!         auth_service::default_hash_permits(),
//!     )),
//! };
//! let app = auth_service::router(state);
//! # Ok::<(), auth_service::ConfigError>(())
//! ```

mod password;

pub mod config;
pub mod error;
pub mod extract;
pub mod routes;
pub mod throttle;
pub mod token;
pub mod tracing_id;
pub mod users;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

pub use config::{Config, ConfigError};
pub use error::AppError as Error;
pub use error::AppError;
pub use extract::{Authenticated, PeerAddress};
pub use throttle::{LoginThrottle, Verdict};
pub use token::{Claims, KeyMaterial, TokenService};
pub use users::{Authentication, UserDirectory, UserRecord};

/// State shared by every handler.
///
/// axum clones this once per request, so both fields are [`Arc`]s: cloning is
/// two refcount bumps rather than a copy of the user directory.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The credential store.
    pub users: Arc<UserDirectory>,
    /// Token issuer and verifier.
    pub tokens: Arc<TokenService>,
    /// Everything needed to reach the profile service.
    pub upstream: Upstream,
    /// Counts recent login failures per account and per source address.
    pub throttle: Arc<LoginThrottle>,
    /// Bounds how many password verifications run at once.
    ///
    /// Argon2id at the default parameters allocates 19 MiB per verification,
    /// and tokio's blocking pool grows to 512 threads. An unbounded login
    /// endpoint can therefore be driven to roughly 9.7 GiB of resident memory
    /// by one attacker with a loop — an OOM kill, not merely slow responses.
    /// Requests that cannot get a permit are shed with `503`, because queueing
    /// them converts a memory problem into an unbounded-latency problem.
    pub hash_permits: Arc<Semaphore>,
}

/// Default ceiling on concurrent password verifications.
///
/// Argon2 is CPU-bound, so allowing more concurrent hashes than cores only
/// thrashes; the floor keeps a single-core container usable.
#[must_use]
pub fn default_hash_permits() -> usize {
    std::thread::available_parallelism().map_or(2, std::num::NonZero::get)
}

/// The profile service, as seen from the gateway.
///
/// Grouped rather than three loose fields on [`AppState`] because they are only
/// ever used together, by one handler.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Reused across calls so connections are pooled rather than renegotiated
    /// per request. `reqwest::Client` is already an `Arc` internally, so
    /// cloning it shares the pool.
    pub http: reqwest::Client,
    /// Base URL of the profile service.
    pub base_url: Arc<str>,
    /// Shared secret presented on the internal hop.
    pub internal_token: Arc<str>,
}

impl Upstream {
    /// How long to wait for a TCP connection to the profile service.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
    /// How long to wait for a complete upstream response.
    ///
    /// `reqwest` applies *no* timeout by default. Without this, an upstream
    /// that accepts connections and then stops responding makes every gateway
    /// request hang forever, and the gateway dies of accumulated connections
    /// rather than of anything wrong with itself. Bounding it converts a
    /// cascading failure into a `502`.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Builds an upstream handle with a fresh connection pool.
    ///
    /// # Panics
    ///
    /// Panics if the TLS backend cannot be initialised, which can only happen
    /// at startup and indicates a broken build rather than a runtime condition.
    #[must_use]
    pub fn new(base_url: &str, internal_token: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .timeout(Self::REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(30))
            // HTTP/1.1 with a connection pool, deliberately. HTTP/2 prior
            // knowledge was measured here and was 25% *slower* — one multiplexed
            // socket means head-of-line blocking and framing overhead, and with
            // small payloads to a peer on the same host there is no connection
            // setup cost for multiplexing to amortise. It wins over a WAN with
            // TLS handshakes, which this is not.
            .build()
            .expect("TLS backend must initialise");

        Self {
            http,
            base_url: Arc::from(base_url),
            internal_token: Arc::from(internal_token),
        }
    }
}

/// Builds the service's HTTP router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/login", post(routes::login::login))
        .route("/logout", post(routes::login::logout))
        // Registered per method rather than with `any`, so an unsupported verb
        // is rejected here instead of making a round-trip to be refused there.
        .route(
            "/profile",
            post(routes::gateway::proxy)
                .get(routes::gateway::proxy)
                .put(routes::gateway::proxy),
        )
        .route("/profile/history", get(routes::gateway::proxy))
        .route("/health", get(routes::health::health))
        .layer(TraceLayer::new_for_http())
        // Outermost, so every request — including ones rejected by an
        // extractor before any handler runs — is correlated.
        .layer(axum::middleware::from_fn(tracing_id::propagate))
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

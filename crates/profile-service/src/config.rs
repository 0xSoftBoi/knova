//! Process configuration, read once at startup.

use std::fmt;

/// Values read from the environment at startup.
#[derive(Clone)]
pub struct Config {
    /// Address the HTTP listener binds to.
    pub bind_addr: String,
    /// Shared secret the gateway must present on every request.
    pub internal_token: String,
    /// Which storage backend to build.
    pub backend: Backend,
}

/// Storage backends selectable at startup.
///
/// A deployment choice rather than a compile-time one: a single instance can
/// run on the in-memory store, while anything replicated needs the version to
/// live somewhere every replica can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Process-local maps. Fastest, and correct only for a single instance.
    #[default]
    Memory,
    /// SQLite, opened in memory as the brief calls for. Demonstrates the same
    /// compare-and-swap as real SQL; point it at a file or a server for
    /// durability and shared state.
    Sqlite,
}

impl Config {
    /// Reads configuration from the environment, falling back to development
    /// defaults so `cargo run` works with no setup.
    #[must_use]
    pub fn from_env() -> Self {
        fn var(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_owned())
        }

        Self {
            bind_addr: var("PROFILE_BIND_ADDR", "127.0.0.1:8081"),
            internal_token: var("INTERNAL_TOKEN", "dev-only-internal-token-change-me"),
            backend: match var("PROFILE_BACKEND", "memory").as_str() {
                "sqlite" => Backend::Sqlite,
                _ => Backend::Memory,
            },
        }
    }
}

/// Redacts the shared secret.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("internal_token", &"<redacted>")
            .field("backend", &self.backend)
            .finish()
    }
}

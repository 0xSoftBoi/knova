//! Process configuration, read once at startup so a bad value fails the boot
//! rather than the first request.

use std::fmt;

use crate::token::KeyMaterial;
use std::num::ParseIntError;
use std::time::Duration;

/// Why [`Config::from_env`] could not produce a usable configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A duration variable was set to something that is not a number of seconds.
    #[error("{key} must be a whole number of seconds")]
    InvalidDuration {
        /// The offending environment variable.
        key: &'static str,
        /// The underlying parse failure.
        #[source]
        source: ParseIntError,
    },
}

/// Values read from the environment at startup.
#[derive(Clone)]
pub struct Config {
    /// Address the HTTP listener binds to.
    pub bind_addr: String,
    /// Key currently used to sign tokens.
    pub active_key: KeyMaterial,
    /// Keys no longer used for signing but still accepted for verification,
    /// so a rotation does not invalidate every live session.
    pub retired_keys: Vec<KeyMaterial>,
    /// How long an issued token remains valid.
    pub token_ttl: Duration,
    /// Username of the hard-coded account the exercise permits.
    pub seed_username: String,
    /// Password of that account, in plain text; hashed during seeding.
    pub seed_password: String,
    /// Base URL of the profile service the gateway forwards to.
    pub profile_service_url: String,
    /// Shared secret presented on the internal hop.
    pub internal_token: String,
}

impl Config {
    /// Reads configuration from the environment, falling back to development
    /// defaults so `cargo run` works with no setup.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidDuration`] if `TOKEN_TTL_SECS` is set to a
    /// value that is not a whole number.
    pub fn from_env() -> Result<Self, ConfigError> {
        fn var(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_owned())
        }

        const TTL_KEY: &str = "TOKEN_TTL_SECS";
        let token_ttl = var(TTL_KEY, "900")
            .parse()
            .map(Duration::from_secs)
            .map_err(|source| ConfigError::InvalidDuration {
                key: TTL_KEY,
                source,
            })?;

        Ok(Self {
            bind_addr: var("AUTH_BIND_ADDR", "127.0.0.1:8080"),
            // Development default. A production deployment would source this
            // from a secret manager and refuse to boot without it.
            active_key: KeyMaterial::new(
                &var("JWT_ACTIVE_KID", "k1"),
                &var("JWT_SECRET", "dev-only-insecure-jwt-secret-change-me"),
            ),
            retired_keys: parse_retired(&var("JWT_RETIRED_KEYS", "")),
            token_ttl,
            seed_username: var("SEED_USERNAME", "alice"),
            seed_password: var("SEED_PASSWORD", "correct-horse-battery-staple"),
            profile_service_url: var("PROFILE_SERVICE_URL", "http://127.0.0.1:8081"),
            internal_token: var("INTERNAL_TOKEN", "dev-only-internal-token-change-me"),
        })
    }
}

/// Parses `kid=secret;kid=secret` into retired verification keys.
///
/// Malformed entries are skipped rather than failing the boot: a typo in a
/// *retired* key must not take the service down, and the consequence is only
/// that tokens signed by it stop verifying, which rotation was going to do
/// anyway.
fn parse_retired(raw: &str) -> Vec<KeyMaterial> {
    raw.split(';')
        .filter_map(|entry| entry.split_once('='))
        .map(|(kid, secret)| KeyMaterial::new(kid.trim(), secret))
        .filter(|key| !key.kid.is_empty() && !key.secret.is_empty())
        .collect()
}

/// Redacts the secret-bearing fields.
///
/// A derived implementation would print the signing key and the seed password
/// the first time anything logged the configuration.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("active_kid", &self.active_key.kid)
            .field(
                "retired_kids",
                &self.retired_keys.iter().map(|k| &k.kid).collect::<Vec<_>>(),
            )
            .field("token_ttl", &self.token_ttl)
            .field("seed_username", &self.seed_username)
            .field("seed_password", &"<redacted>")
            .field("profile_service_url", &self.profile_service_url)
            .field("internal_token", &"<redacted>")
            .finish()
    }
}

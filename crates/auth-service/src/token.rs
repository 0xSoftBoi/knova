//! Bearer token issuing, verification, and revocation.
//!
//! # Key rotation
//!
//! Every token carries a `kid` naming the key that signed it, and the service
//! holds one active signing key plus any number of retired verification keys.
//! Rotation is then an overlap rather than an outage: deploy the new key as
//! active with the old one retired, wait out the token lifetime, drop the old
//! one. A service with a single hard-coded secret cannot rotate without
//! invalidating every live session, which in practice means it never rotates.
//!
//! The `kid` selects from *our* key set; it never selects an algorithm. The
//! accepted algorithm list is pinned separately, because trusting a token's own
//! header for that is how `alg: none` and HMAC/RSA confusion land.
//!
//! # Revocation
//!
//! Expiry alone means a stolen token is valid for its full lifetime. Each token
//! carries a `jti`, and [`TokenService::revoke`] records it until its natural
//! expiry — so the list is bounded by issuance rate times TTL, not by time.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::UserId;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// One signing or verification key, named by its `kid`.
#[derive(Debug, Clone)]
pub struct KeyMaterial {
    /// Identifier written into the token header.
    pub kid: String,
    /// The HMAC secret.
    pub secret: String,
}

impl KeyMaterial {
    /// Builds key material.
    #[must_use]
    pub fn new(kid: &str, secret: &str) -> Self {
        Self {
            kid: kid.to_owned(),
            secret: secret.to_owned(),
        }
    }
}

/// Claims carried by an issued token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the account's [`UserId`], not its username, because usernames
    /// may be reassigned while ids may not.
    pub sub: UserId,
    /// The account's login name at the time of issue, carried for logging.
    pub username: String,
    /// Unique token identifier, so one token can be revoked without touching
    /// the key that signed it.
    pub jti: String,
    /// Expiry, as seconds since the Unix epoch.
    pub exp: u64,
    /// Issued-at, as seconds since the Unix epoch.
    pub iat: u64,
}

/// Signs, validates and revokes [`Claims`].
pub struct TokenService {
    active_kid: String,
    encoding: EncodingKey,
    /// Every key whose tokens are still accepted, including the active one.
    decoding: HashMap<String, DecodingKey>,
    validation: Validation,
    ttl: Duration,
    /// Revoked `jti`s mapped to the expiry after which they can be forgotten.
    revoked: RwLock<HashMap<String, u64>>,
}

impl TokenService {
    /// Builds a service that signs with `active` and also accepts tokens signed
    /// by any key in `retired`.
    #[must_use]
    pub fn new(active: &KeyMaterial, retired: &[KeyMaterial], ttl: Duration) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        // Pin the accepted algorithm set. Leaving this to a library default is
        // how `alg: none` and HMAC/RSA confusion attacks land.
        validation.algorithms = vec![Algorithm::HS256];
        validation.set_required_spec_claims(&["exp", "sub"]);

        let mut decoding = HashMap::new();
        for key in std::iter::once(active).chain(retired) {
            decoding.insert(
                key.kid.clone(),
                DecodingKey::from_secret(key.secret.as_bytes()),
            );
        }

        Self {
            active_kid: active.kid.clone(),
            encoding: EncodingKey::from_secret(active.secret.as_bytes()),
            decoding,
            validation,
            ttl,
            revoked: RwLock::new(HashMap::new()),
        }
    }

    /// Lifetime of tokens issued by this service, in seconds.
    #[must_use]
    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// Issues a token for the given account.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::TokenSigning`] if the claims cannot be signed.
    ///
    /// # Panics
    ///
    /// Panics if the system clock is set before the Unix epoch.
    pub fn issue(&self, user_id: &UserId, username: &str) -> Result<String, AppError> {
        let now = unix_now();
        let claims = Claims {
            sub: user_id.clone(),
            username: username.to_owned(),
            jti: uuid::Uuid::new_v4().to_string(),
            iat: now,
            exp: now + self.ttl.as_secs(),
        };

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(self.active_kid.clone());

        encode(&header, &claims, &self.encoding).map_err(AppError::TokenSigning)
    }

    /// Validates a token's key, signature, algorithm, expiry and revocation.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::InvalidToken`] for every failure mode. The caller
    /// learns that the token is unusable, never why — distinguishing "expired"
    /// from "revoked" from "forged" tells an attacker how close they are.
    pub fn verify(&self, token: &str) -> Result<Claims, AppError> {
        let kid = decode_header(token)
            .map_err(|_| AppError::InvalidToken)?
            .kid
            .ok_or(AppError::InvalidToken)?;

        let key = self.decoding.get(&kid).ok_or(AppError::InvalidToken)?;

        let claims = decode::<Claims>(token, key, &self.validation)
            .map(|data| data.claims)
            .map_err(|_| AppError::InvalidToken)?;

        if self.revoked.read().contains_key(&claims.jti) {
            return Err(AppError::InvalidToken);
        }

        Ok(claims)
    }

    /// Revokes a single token, effective immediately.
    ///
    /// Recorded only until the token would have expired anyway, so the list is
    /// bounded by issuance rate times TTL rather than growing without limit.
    pub fn revoke(&self, claims: &Claims) {
        let now = unix_now();
        let mut revoked = self.revoked.write();

        revoked.retain(|_, expiry| *expiry > now);
        revoked.insert(claims.jti.clone(), claims.exp);
    }

    /// How many revocations are currently retained. Exposed for tests and
    /// metrics; the number is a queue depth, not a secret.
    #[must_use]
    pub fn revoked_count(&self) -> usize {
        self.revoked.read().len()
    }
}

/// Omits the key material.
impl fmt::Debug for TokenService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenService")
            .field("active_kid", &self.active_kid)
            .field("accepted_kids", &self.decoding.keys().collect::<Vec<_>>())
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_secs()
}

//! Argon2id hashing and the decoy hash that keeps login failures uniform.

use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use password_hash::rand_core::OsRng;

/// Default parameters (m = 19456 KiB, t = 2, p = 1) track the OWASP
/// recommendation for interactive logins.
fn argon2() -> &'static Argon2<'static> {
    static ARGON2: OnceLock<Argon2<'static>> = OnceLock::new();
    ARGON2.get_or_init(Argon2::default)
}

/// Hashes `password` into a PHC string with a fresh random salt.
pub(crate) fn hash(password: &str) -> Result<String, password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// A well-formed hash of a password no account holds.
///
/// Verifying against it costs exactly what verifying a real hash costs, which
/// is what keeps an unknown username indistinguishable from a wrong password.
/// Computed once on first use; it cannot be a `const` because the salt is
/// random.
pub(crate) fn decoy_hash() -> &'static str {
    static DECOY: OnceLock<String> = OnceLock::new();
    DECOY.get_or_init(|| hash("\0unassigned\0").expect("hashing a literal cannot fail"))
}

/// Checks `password` against the PHC string `phc`.
///
/// A malformed `phc` is a data-integrity fault rather than a client error, so
/// it is logged and reported as a plain mismatch instead of being propagated
/// where it could be probed.
pub(crate) fn verify(password: &str, phc: &str) -> bool {
    let Ok(expected) = PasswordHash::new(phc) else {
        tracing::error!("stored password hash is malformed");
        return false;
    };
    argon2()
        .verify_password(password.as_bytes(), &expected)
        .is_ok()
}

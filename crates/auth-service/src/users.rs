//! In-memory user directory.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use common::UserId;

use crate::password;

/// A stored account.
///
/// Fields are private: the password hash must not be readable by handler code,
/// and the id and username are only meaningful together.
pub struct UserRecord {
    id: UserId,
    username: String,
    /// PHC string; the salt and Argon2 parameters are embedded in it.
    password_hash: String,
}

impl UserRecord {
    /// The account's stable identifier.
    #[must_use]
    pub fn id(&self) -> &UserId {
        &self.id
    }

    /// The account's login name.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }
}

/// Redacts the password hash.
///
/// A hash is not a plaintext password, but it is offline-crackable material and
/// has no business appearing in a log line.
impl fmt::Debug for UserRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserRecord")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("password_hash", &"<redacted>")
            .finish()
    }
}

/// Outcome of a credential check.
///
/// An unknown username and a wrong password both produce [`Self::Rejected`].
/// The distinction is not merely unrendered: it never leaves this module, so no
/// caller is able to leak it.
#[derive(Debug)]
pub enum Authentication {
    /// The credentials matched the contained account.
    Authenticated(Arc<UserRecord>),
    /// The credentials did not match, for a reason the caller does not learn.
    Rejected,
}

/// Accounts held in memory, immutable after construction.
///
/// There is no lock because there are no writers. `Arc<UserDirectory>` is
/// already [`Sync`] since [`HashMap`] is [`Sync`] when its contents are. Adding
/// a [`RwLock`] here would cost an atomic per request and buy nothing; it would
/// be necessary the moment registration existed.
///
/// [`RwLock`]: std::sync::RwLock
#[derive(Debug, Default)]
pub struct UserDirectory {
    by_username: HashMap<String, Arc<UserRecord>>,
}

impl UserDirectory {
    /// Builds a directory containing the single hard-coded account the exercise
    /// permits, hashing `password` on the way in.
    ///
    /// # Panics
    ///
    /// Panics if Argon2 rejects the seed password. This can only happen for a
    /// password longer than `0xFFFF_FFFF` bytes, so it indicates a programming
    /// error rather than a runtime condition.
    #[must_use]
    pub fn with_seed_user(username: &str, password: &str) -> Self {
        // Force the decoy hash now. It lives behind a `OnceLock`, so leaving it
        // cold means the first unknown-username login computes it *and* then
        // verifies against it — two Argon2 passes where a known username costs
        // one. Measured at 0.94s versus 0.47s, which is a user-enumeration
        // oracle for exactly one request per process. Warming it here removes
        // the asymmetry at the only point where a directory can exist without
        // having served a request yet.
        let _ = password::decoy_hash();

        let record = UserRecord {
            id: UserId::random(),
            username: username.to_owned(),
            password_hash: password::hash(password).expect("seed password must be hashable"),
        };

        Self {
            by_username: HashMap::from([(username.to_owned(), Arc::new(record))]),
        }
    }

    /// Verifies `password` against the account named `username`.
    ///
    /// Performs a full Argon2 verification whether or not the account exists, so
    /// the two rejection paths are indistinguishable by elapsed time as well as
    /// by response body.
    ///
    /// Blocking and CPU-bound by design; call it from
    /// [`tokio::task::spawn_blocking`] rather than directly from a handler.
    #[must_use]
    pub fn authenticate(&self, username: &str, password: &str) -> Authentication {
        let user = self.by_username.get(username);

        let phc = match user {
            Some(user) => user.password_hash.as_str(),
            None => password::decoy_hash(),
        };

        match (user, password::verify(password, phc)) {
            (Some(user), true) => Authentication::Authenticated(Arc::clone(user)),
            _ => Authentication::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn constructing_the_directory_warms_the_decoy_hash() {
        let _directory = UserDirectory::with_seed_user("alice", "hunter2");

        // A cold `OnceLock` would run a full Argon2 here — tens of milliseconds
        // in release, hundreds in debug. Warm, this is a pointer read. The gap
        // is wide enough that the assertion is not a timing race.
        let started = Instant::now();
        let _ = password::decoy_hash();

        assert!(
            started.elapsed() < Duration::from_millis(1),
            "the decoy hash was still cold after construction, so the first \
             unknown-username login would cost two Argon2 passes instead of one \
             and leak account existence by timing"
        );
    }

    #[test]
    fn an_unknown_username_is_rejected_like_a_wrong_password() {
        let directory = UserDirectory::with_seed_user("alice", "hunter2");

        assert!(matches!(
            directory.authenticate("alice", "wrong"),
            Authentication::Rejected
        ));
        assert!(matches!(
            directory.authenticate("nobody", "wrong"),
            Authentication::Rejected
        ));
        assert!(matches!(
            directory.authenticate("alice", "hunter2"),
            Authentication::Authenticated(_)
        ));
    }
}

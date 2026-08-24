//! Idempotency keys for `POST /profile`.
//!
//! A client whose request times out cannot tell whether the server processed it.
//! Retrying is the only option, and without deduplication the retry gets `409`
//! — indistinguishable from "someone else created this profile", which is a
//! materially different fact. The client is left guessing about the state of
//! its own write.
//!
//! An `Idempotency-Key` fixes that: the first request's outcome is recorded
//! against the key and replayed verbatim for any retry carrying the same key.
//! The retry then receives the original `201`, not a confusing `409`.
//!
//! Three cases have to be distinguished, and only the first is obvious.
//!
//! - **Key unseen** — reserve it, run the request, record the outcome.
//! - **Key complete** — replay the recorded response.
//! - **Key in flight** — the first request has not finished. Answering `409`
//!   is correct and is *not* the same `409` as a duplicate profile, so it
//!   carries its own error code.
//!
//! A fourth case matters for safety: the same key presented with a *different*
//! body. That means the client reused a key it should not have, and replaying
//! the first response would silently discard the second request. Refused with
//! `422`.
//!
//! `PUT` needs none of this: `If-Match` already makes a retry idempotent,
//! because the second attempt fails its precondition.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// A recorded response, replayed for any retry of the same key.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// Status of the original response.
    pub status: u16,
    /// Body of the original response, verbatim.
    pub body: String,
    /// `ETag` of the original response, if it carried one.
    pub etag: Option<String>,
}

/// What the store says about a key.
#[derive(Debug, Clone)]
pub enum Lookup {
    /// The key is newly reserved; the caller should do the work.
    Reserved,
    /// The key completed; replay this.
    Replay(Recorded),
    /// Another request holds this key right now.
    InFlight,
    /// The key was used before with a different request body.
    BodyMismatch,
}

#[derive(Debug)]
enum Entry {
    InFlight {
        fingerprint: u64,
        since: Instant,
    },
    Complete {
        fingerprint: u64,
        at: Instant,
        recorded: Recorded,
    },
}

impl Entry {
    fn fingerprint(&self) -> u64 {
        match self {
            Self::InFlight { fingerprint, .. } | Self::Complete { fingerprint, .. } => *fingerprint,
        }
    }
}

/// Records the outcome of keyed requests for a bounded window.
#[derive(Debug)]
pub struct IdempotencyStore {
    entries: Mutex<HashMap<(String, String), Entry>>,
    retention: Duration,
    /// How long an in-flight reservation is honoured before it is assumed
    /// abandoned. Without this, a request that panics mid-flight would wedge
    /// its key until the process restarts.
    reservation_timeout: Duration,
}

impl IdempotencyStore {
    /// How long a completed outcome stays replayable.
    pub const DEFAULT_RETENTION: Duration = Duration::from_secs(86_400);
    /// How long an unfinished request holds its key.
    pub const DEFAULT_RESERVATION_TIMEOUT: Duration = Duration::from_secs(60);

    /// Builds a store with the given windows.
    #[must_use]
    pub fn new(retention: Duration, reservation_timeout: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            retention,
            reservation_timeout,
        }
    }

    /// Reserves `key` for `user`, or reports what is already known about it.
    #[must_use]
    pub fn begin(&self, user: &str, key: &str, body: &str) -> Lookup {
        let fingerprint = fingerprint(body);
        let mut entries = self.entries.lock();

        self.expire(&mut entries);

        let slot = (user.to_owned(), key.to_owned());

        match entries.get(&slot) {
            Some(entry) if entry.fingerprint() != fingerprint => Lookup::BodyMismatch,
            Some(Entry::InFlight { .. }) => Lookup::InFlight,
            Some(Entry::Complete { recorded, .. }) => Lookup::Replay(recorded.clone()),
            None => {
                entries.insert(
                    slot,
                    Entry::InFlight {
                        fingerprint,
                        since: Instant::now(),
                    },
                );
                Lookup::Reserved
            }
        }
    }

    /// Records the outcome of a reserved key.
    pub fn finish(&self, user: &str, key: &str, body: &str, recorded: Recorded) {
        self.entries.lock().insert(
            (user.to_owned(), key.to_owned()),
            Entry::Complete {
                fingerprint: fingerprint(body),
                at: Instant::now(),
                recorded,
            },
        );
    }

    /// Releases a reservation whose request failed, so a retry may proceed.
    ///
    /// Only failures the client can fix are released. A recorded terminal
    /// response is never released, or a retry would re-run the work.
    pub fn abandon(&self, user: &str, key: &str) {
        let mut entries = self.entries.lock();
        let slot = (user.to_owned(), key.to_owned());

        if matches!(entries.get(&slot), Some(Entry::InFlight { .. })) {
            entries.remove(&slot);
        }
    }

    fn expire(&self, entries: &mut HashMap<(String, String), Entry>) {
        entries.retain(|_, entry| match entry {
            Entry::InFlight { since, .. } => since.elapsed() < self.reservation_timeout,
            Entry::Complete { at, .. } => at.elapsed() < self.retention,
        });
    }
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::new(Self::DEFAULT_RETENTION, Self::DEFAULT_RESERVATION_TIMEOUT)
    }
}

/// Fingerprints a request body so key reuse with different content is caught.
///
/// A hash, not the body itself: the store must not retain payloads it has no
/// reason to keep. Collisions would let a mismatched retry replay the wrong
/// response, but at 64 bits against the handful of bodies one client sends
/// under one key, that is not a risk worth a cryptographic digest.
fn fingerprint(body: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"address":"1 Main St"}"#;

    fn recorded() -> Recorded {
        Recorded {
            status: 201,
            body: BODY.to_owned(),
            etag: Some("\"1\"".to_owned()),
        }
    }

    #[test]
    fn a_fresh_key_is_reserved_then_replayed() {
        let store = IdempotencyStore::default();

        assert!(matches!(store.begin("u", "k", BODY), Lookup::Reserved));
        assert!(matches!(store.begin("u", "k", BODY), Lookup::InFlight));

        store.finish("u", "k", BODY, recorded());

        let Lookup::Replay(replayed) = store.begin("u", "k", BODY) else {
            panic!("a completed key must replay");
        };
        assert_eq!(replayed.status, 201);
    }

    #[test]
    fn reusing_a_key_with_a_different_body_is_refused() {
        let store = IdempotencyStore::default();
        let _ = store.begin("u", "k", BODY);
        store.finish("u", "k", BODY, recorded());

        assert!(matches!(
            store.begin("u", "k", r#"{"address":"somewhere else"}"#),
            Lookup::BodyMismatch
        ));
    }

    #[test]
    fn keys_are_scoped_to_one_user() {
        let store = IdempotencyStore::default();
        let _ = store.begin("alice", "k", BODY);

        assert!(
            matches!(store.begin("bob", "k", BODY), Lookup::Reserved),
            "one caller's key must not collide with another's"
        );
    }

    #[test]
    fn an_abandoned_reservation_frees_the_key() {
        let store = IdempotencyStore::default();
        let _ = store.begin("u", "k", BODY);
        store.abandon("u", "k");

        assert!(matches!(store.begin("u", "k", BODY), Lookup::Reserved));
    }

    #[test]
    fn a_recorded_outcome_survives_abandon() {
        let store = IdempotencyStore::default();
        let _ = store.begin("u", "k", BODY);
        store.finish("u", "k", BODY, recorded());
        store.abandon("u", "k");

        assert!(
            matches!(store.begin("u", "k", BODY), Lookup::Replay(_)),
            "a terminal response must never be released for re-execution"
        );
    }

    #[test]
    fn a_stale_reservation_lapses() {
        let store = IdempotencyStore::new(Duration::from_secs(60), Duration::from_millis(30));
        let _ = store.begin("u", "k", BODY);

        std::thread::sleep(Duration::from_millis(60));

        assert!(
            matches!(store.begin("u", "k", BODY), Lookup::Reserved),
            "a request that died mid-flight must not wedge its key"
        );
    }
}

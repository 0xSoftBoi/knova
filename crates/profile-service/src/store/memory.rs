//! In-memory [`ProfileRepository`], the default backend.
//!
//! # Sharding
//!
//! ```text
//! [ RwLock<HashMap<UserId, Arc<RwLock<Record>>>> ; N ]
//!   ^^^^^^                 ^^^^^^^^^^^^^^^^^^^^
//!   one shard's structure  one profile's contents
//! ```
//!
//! A single map behind one lock is correct, and it is also a single cache line
//! that every core touches on every request. Even a *read* lock has to write
//! the reader count, so the line bounces between cores and throughput stops
//! scaling with them. Splitting the keyspace across shards means unrelated
//! profiles rarely touch the same line.
//!
//! The per-entry lock then guards one profile's contents, so writers to
//! different profiles never contend, and writers to the *same* profile
//! serialise — the granularity the invariant needs and no more. The shard guard
//! is released before the entry lock is taken (see [`InMemoryProfiles::slot`]);
//! holding both would collapse this into a global lock wearing a disguise.
//!
//! # Reads take no lock at all
//!
//! [`Record`] keeps the current [`Profile`] in an [`ArcSwap`], rebuilt only when
//! a write is accepted. A read is then a single atomic load — no lock, no
//! allocation, and no readers blocking each other. Before this the address, the
//! phone number and the id were each cloned to build a response, and every
//! reader of a hot profile queued behind the same `RwLock`; with four workers on
//! one key that lock, not the work, was the ceiling.
//!
//! Writers still serialise on the revisions mutex, which is what keeps the
//! compare-and-swap atomic: the version is read and the new projection is
//! published under one hold, so no two writers can both believe they won. A
//! reader concurrent with a write sees either the old projection or the new
//! one, never a mix.
//!
//! The cost is one duplicated copy of two short strings per profile — the
//! current projection and the newest revision hold the same values. That is the
//! right trade for a read-heavy store.
//!
//! # Why `parking_lot` and not `std::sync`
//!
//! `std` locks poison on panic, and a poisoned entry lock would make one
//! account permanently unreadable until the process restarts — a customer
//! locked out of their own record by an unrelated bug. `parking_lot` does not
//! poison, which removes the failure mode rather than arguing that the critical
//! sections cannot panic. Neither crate's guards may be held across an
//! `.await`; none here are.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::BuildHasher;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use common::dto::{Profile, ProfileInput};
use common::{UserId, Version};
use parking_lot::{Mutex, RwLock};
use rustc_hash::FxBuildHasher;

use super::{ProfileRepository, Revision, StoreError, Update, project, revision};

/// A profile's full history, plus the projection readers are handed.
#[derive(Debug)]
struct Record {
    /// Read without any lock; replaced wholesale by a writer.
    current: ArcSwap<Profile>,
    /// Held by writers for the whole compare-and-swap, and by the audit read.
    revisions: Mutex<Vec<Revision>>,
}

type Shard = RwLock<HashMap<UserId, Arc<Record>, FxBuildHasher>>;

/// Profiles held in memory, keyed by owner across a fixed set of shards.
#[derive(Debug)]
pub struct InMemoryProfiles {
    shards: Box<[Shard]>,
    /// `shards.len() - 1`; the length is a power of two so this masks instead
    /// of dividing.
    mask: usize,
}

impl InMemoryProfiles {
    /// Creates an empty store sized to the machine.
    #[must_use]
    pub fn new() -> Self {
        Self::with_shards(default_shards())
    }

    /// Creates an empty store with `shards` rounded up to a power of two.
    ///
    /// Exposed so a test can pin the count; production should use [`Self::new`].
    #[must_use]
    pub fn with_shards(shards: usize) -> Self {
        let count = shards.max(1).next_power_of_two();
        Self {
            shards: (0..count).map(|_| Shard::default()).collect(),
            mask: count - 1,
        }
    }

    /// The shard owning `user_id`.
    fn shard(&self, user_id: &UserId) -> &Shard {
        // Take the high bits: `FxHash`'s low bits are the least mixed, and the
        // mask would otherwise select shards on exactly those.
        #[allow(clippy::cast_possible_truncation)]
        let index = (FxBuildHasher.hash_one(user_id) >> 32) as usize & self.mask;
        &self.shards[index]
    }

    /// Reads the current projection without awaiting.
    ///
    /// The real implementation; the trait method is a thin wrapper. Nothing
    /// here can block, so the async signature the trait needs — for backends
    /// that do I/O — costs a boxed future per call that this path does not
    /// need. Exposed so the hot read can skip it where the concrete type is
    /// known, and so a benchmark can price that box.
    #[must_use]
    pub fn read(&self, user_id: &UserId) -> Option<Arc<Profile>> {
        self.slot(user_id).map(|slot| slot.current.load_full())
    }

    /// Looks up one profile's lock, releasing the shard guard before returning.
    ///
    /// The temporary scope is the point: the shard guard must not outlive this
    /// function, or callers would hold it while waiting on the per-profile lock.
    fn slot(&self, user_id: &UserId) -> Option<Arc<Record>> {
        self.shard(user_id).read().get(user_id).map(Arc::clone)
    }
}

impl Default for InMemoryProfiles {
    fn default() -> Self {
        Self::new()
    }
}

/// Four shards per core, which keeps unrelated keys off each other's cache
/// lines without spending a lock per profile.
fn default_shards() -> usize {
    std::thread::available_parallelism().map_or(8, std::num::NonZero::get) * 4
}

#[async_trait]
impl ProfileRepository for InMemoryProfiles {
    async fn create(
        &self,
        user_id: &UserId,
        input: ProfileInput,
    ) -> Result<Option<Arc<Profile>>, StoreError> {
        let mut shard = self.shard(user_id).write();

        match shard.entry(user_id.clone()) {
            Entry::Occupied(_) => Ok(None),
            Entry::Vacant(slot) => {
                let first = revision(Version::INITIAL, input);
                let current = Arc::new(project(user_id, &first));
                slot.insert(Arc::new(Record {
                    current: ArcSwap::new(Arc::clone(&current)),
                    revisions: Mutex::new(vec![first]),
                }));
                Ok(Some(current))
            }
        }
    }

    async fn get(&self, user_id: &UserId) -> Result<Option<Arc<Profile>>, StoreError> {
        Ok(self.read(user_id))
    }

    async fn update(
        &self,
        user_id: &UserId,
        expected_version: Version,
        input: ProfileInput,
    ) -> Result<Update, StoreError> {
        let Some(slot) = self.slot(user_id) else {
            return Ok(Update::Missing);
        };

        // Every writer takes this, so the version read below and the publish
        // that follows happen under one hold. That is what makes the version
        // test meaningful rather than decorative — no second writer can slip
        // between them and have both believe they won.
        let mut revisions = slot.revisions.lock();
        let current_version = slot.current.load().version;

        if current_version != expected_version {
            return Ok(Update::Stale { current_version });
        }

        let next = current_version
            .checked_next()
            .ok_or(StoreError::VersionExhausted)?;

        let applied = revision(next, input);
        let current = Arc::new(project(user_id, &applied));
        revisions.push(applied);
        slot.current.store(Arc::clone(&current));

        Ok(Update::Applied(current))
    }

    async fn history(&self, user_id: &UserId) -> Result<Vec<Revision>, StoreError> {
        Ok(self
            .slot(user_id)
            .map(|slot| slot.revisions.lock().clone())
            .unwrap_or_default())
    }
}

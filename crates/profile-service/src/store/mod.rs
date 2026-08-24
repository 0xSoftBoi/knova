//! Profile storage.
//!
//! # The three hazards, and which mechanism handles each
//!
//! **Data races.** Two threads mutating the same bytes without synchronisation.
//! Rust rules these out at compile time. This costs no design effort and is not
//! the interesting part.
//!
//! **Torn writes.** A reader observing a profile with the new address beside the
//! old phone number. Any lock around the whole record prevents this.
//!
//! **Lost updates.** The one that matters, and the one **no lock can fix**. Two
//! clients read version 4, both edit, both write. The second silently erases
//! the first and *both* receive `200`. The window spans two round-trips with
//! human think-time in between, so holding a lock across it is not an option.
//!
//! The answer is optimistic concurrency control: every record carries a
//! [`Version`], a writer states the version it believes it is replacing, and
//! the store refuses the write if that belief is stale.
//!
//! # Why this is a trait
//!
//! Because the guarantee has to outlive the `HashMap`. A single-process store
//! makes `If-Match: "4"` meaningful only to the instance that issued version 4;
//! behind a load balancer with three replicas there are three divergent
//! histories and the guarantee is void. Expressing storage as a trait forces
//! the concurrency contract to be stated independently of where the bytes live,
//! and [`sqlite`] demonstrates that the same contract holds when it is a real
//! `UPDATE ... WHERE version = ?` with a row-count check.
//!
//! Both implementations pass the same conformance suite
//! (`tests/repository_conformance.rs`), which is the actual evidence that the
//! design is not coupled to one backend.

pub mod memory;
pub mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;
use common::dto::{Profile, ProfileInput};
use common::{UserId, Version};
use serde::{Deserialize, Serialize};

pub use memory::InMemoryProfiles;
pub use sqlite::SqliteProfiles;

/// One accepted write, retained permanently.
///
/// A profile is a projection of its revisions, not a cell that gets
/// overwritten. An address change is a fraud and KYC signal, so discarding the
/// prior value destroys evidence a bank is required to keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    /// The version this write produced.
    pub version: Version,
    /// The address as of this revision.
    pub address: String,
    /// The phone number as of this revision.
    pub phone_number: String,
    /// When the write was accepted, in milliseconds since the Unix epoch.
    pub recorded_at_ms: u64,
}

/// Result of an attempted update.
///
/// A dedicated enum rather than `Result<Profile, _>`, because "your version was
/// stale" is an ordinary expected outcome the caller translates into `412`, not
/// an error condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The write was applied; contains the profile at its new version.
    Applied(Arc<Profile>),
    /// The caller's `If-Match` version did not match the stored one.
    Stale {
        /// The version actually stored, so the client can re-read and retry.
        current_version: Version,
    },
    /// No profile exists for this user.
    Missing,
}

/// Why a storage operation could not complete.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The version counter cannot be incremented further.
    ///
    /// Unreachable in any real deployment, but a hard failure rather than a
    /// saturating increment: at saturation every stale version would silently
    /// become valid again, which is a correctness failure rather than a
    /// capacity one.
    #[error("version counter exhausted")]
    VersionExhausted,

    /// The backend itself failed.
    #[error("storage backend failed")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Storage for profiles, with compare-and-swap update semantics.
///
/// Implementations must guarantee that [`Self::update`] compares the stored
/// version and writes atomically with respect to other updates of the same
/// profile. Everything the service promises rests on that.
#[async_trait]
pub trait ProfileRepository: std::fmt::Debug + Send + Sync + 'static {
    /// Creates a profile at [`Version::INITIAL`], or returns [`None`] if one
    /// already exists.
    ///
    /// The existence check and the insert must be atomic, so two concurrent
    /// creates cannot both succeed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the backend fails.
    async fn create(
        &self,
        user_id: &UserId,
        input: ProfileInput,
    ) -> Result<Option<Arc<Profile>>, StoreError>;

    /// Reads the current projection of a profile.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the backend fails.
    async fn get(&self, user_id: &UserId) -> Result<Option<Arc<Profile>>, StoreError>;

    /// Replaces a profile only if it is still at `expected_version`.
    ///
    /// # Errors
    ///
    /// [`StoreError::VersionExhausted`] if the counter cannot advance, or
    /// [`StoreError::Backend`] if the backend fails.
    async fn update(
        &self,
        user_id: &UserId,
        expected_version: Version,
        input: ProfileInput,
    ) -> Result<Update, StoreError>;

    /// Every accepted write for this profile, oldest first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the backend fails.
    async fn history(&self, user_id: &UserId) -> Result<Vec<Revision>, StoreError>;
}

/// Wall-clock milliseconds since the Unix epoch.
///
/// Audit timestamps are for humans reading a history, not for ordering: the
/// [`Version`] sequence is what orders revisions, and it does not depend on a
/// clock that can step backwards.
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Builds the projection a client sees from a revision.
///
/// Called once per accepted write, never per read: the result is retained as an
/// [`Arc`] so readers share it.
pub(crate) fn project(user_id: &UserId, revision: &Revision) -> Profile {
    Profile {
        user_id: user_id.clone(),
        address: revision.address.clone(),
        phone_number: revision.phone_number.clone(),
        version: revision.version,
    }
}

/// Builds the revision an accepted write produces.
pub(crate) fn revision(version: Version, input: ProfileInput) -> Revision {
    Revision {
        version,
        address: input.address,
        phone_number: input.phone_number,
        recorded_at_ms: now_ms(),
    }
}

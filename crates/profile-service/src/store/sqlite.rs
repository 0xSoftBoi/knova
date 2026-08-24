//! SQLite-backed [`ProfileRepository`].
//!
//! Exists to demonstrate that the concurrency contract is a property of the
//! *design*, not of the `HashMap`. The compare-and-swap that
//! [`memory`](super::memory) performs with a version check under a write lock
//! is here a single statement:
//!
//! ```sql
//! UPDATE profiles SET address = ?, phone_number = ?, version = ?
//!  WHERE user_id = ? AND version = ?
//! ```
//!
//! The row count decides the outcome. One row means the caller's version was
//! current and the write landed. Zero rows means either the profile is gone or
//! someone else got there first — indistinguishable from the count alone, which
//! is why the follow-up read happens inside the same transaction.
//!
//! That statement is what makes horizontal scaling possible: the version lives
//! in shared storage, so `If-Match: "4"` means the same thing to every replica.
//! The in-memory backend cannot say that, and swapping this in is the whole
//! remedy. Point it at Postgres and nothing above this module changes.
//!
//! # Blocking
//!
//! `rusqlite` is a blocking driver, so every method hops to
//! [`tokio::task::spawn_blocking`] — the same reasoning as the Argon2 path in
//! the authorization service. Putting that inside the implementation rather
//! than in the handlers means the in-memory backend, which never blocks, pays
//! nothing for it.
//!
//! The connection is behind one mutex because an in-memory SQLite database
//! belongs to a single connection. A file or server backend would take a pool
//! instead; that is a change to this module and nothing else.

use std::sync::Arc;

use async_trait::async_trait;
use common::dto::{Profile, ProfileInput};
use common::{UserId, Version};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use super::{ProfileRepository, Revision, StoreError, Update, now_ms};

const SCHEMA: &str = "
    PRAGMA foreign_keys = ON;

    CREATE TABLE IF NOT EXISTS profiles (
        user_id      TEXT PRIMARY KEY,
        address      TEXT    NOT NULL,
        phone_number TEXT    NOT NULL,
        version      INTEGER NOT NULL
    );

    -- Append-only. The row in `profiles` is a projection of these; nothing ever
    -- updates or deletes here, which is what makes the history admissible.
    CREATE TABLE IF NOT EXISTS profile_revisions (
        user_id        TEXT    NOT NULL,
        version        INTEGER NOT NULL,
        address        TEXT    NOT NULL,
        phone_number   TEXT    NOT NULL,
        recorded_at_ms INTEGER NOT NULL,
        PRIMARY KEY (user_id, version)
    );
";

/// Profiles stored in SQLite.
#[derive(Debug)]
pub struct SqliteProfiles {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteProfiles {
    /// Opens an in-memory database, as the brief calls for.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the database cannot be opened or migrated.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory().map_err(backend)?)
    }

    /// Opens a database file, which is the same store with durability.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the file cannot be opened or migrated.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path).map_err(backend)?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch(SCHEMA).map_err(backend)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Runs `work` on the blocking pool with exclusive use of the connection.
    async fn with_connection<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);

        tokio::task::spawn_blocking(move || work(&mut connection.lock()))
            .await
            .map_err(|join| StoreError::Backend(Box::new(join)))?
    }
}

#[async_trait]
impl ProfileRepository for SqliteProfiles {
    async fn create(
        &self,
        user_id: &UserId,
        input: ProfileInput,
    ) -> Result<Option<Arc<Profile>>, StoreError> {
        let owner = user_id.clone();

        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let version = Version::INITIAL;
            let recorded_at_ms = now_ms();

            // `INSERT OR IGNORE` on the primary key makes the existence check
            // and the insert one atomic step, so two concurrent creates cannot
            // both succeed.
            let inserted = transaction
                .execute(
                    "INSERT OR IGNORE INTO profiles (user_id, address, phone_number, version)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        owner.as_str(),
                        input.address,
                        input.phone_number,
                        to_sql(version)?
                    ],
                )
                .map_err(backend)?;

            if inserted == 0 {
                return Ok(None);
            }

            transaction
                .execute(
                    "INSERT INTO profile_revisions
                        (user_id, version, address, phone_number, recorded_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        owner.as_str(),
                        to_sql(version)?,
                        input.address,
                        input.phone_number,
                        recorded_at_ms
                    ],
                )
                .map_err(backend)?;

            transaction.commit().map_err(backend)?;

            Ok(Some(Arc::new(Profile {
                user_id: owner,
                address: input.address,
                phone_number: input.phone_number,
                version,
            })))
        })
        .await
    }

    async fn get(&self, user_id: &UserId) -> Result<Option<Arc<Profile>>, StoreError> {
        let owner = user_id.clone();

        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT address, phone_number, version FROM profiles WHERE user_id = ?1",
                    params![owner.as_str()],
                    |row| {
                        Ok(Profile {
                            user_id: owner.clone(),
                            address: row.get(0)?,
                            phone_number: row.get(1)?,
                            version: from_sql(row.get(2)?),
                        })
                    },
                )
                .optional()
                .map(|found| found.map(Arc::new))
                .map_err(backend)
        })
        .await
    }

    async fn update(
        &self,
        user_id: &UserId,
        expected_version: Version,
        input: ProfileInput,
    ) -> Result<Update, StoreError> {
        let owner = user_id.clone();

        self.with_connection(move |connection| {
            // IMMEDIATE takes the write lock up front, so the follow-up read
            // that distinguishes "stale" from "missing" cannot race another
            // writer between the UPDATE and the SELECT.
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(backend)?;

            let next = expected_version
                .checked_next()
                .ok_or(StoreError::VersionExhausted)?;

            let changed = transaction
                .execute(
                    "UPDATE profiles
                        SET address = ?1, phone_number = ?2, version = ?3
                      WHERE user_id = ?4 AND version = ?5",
                    params![
                        input.address,
                        input.phone_number,
                        to_sql(next)?,
                        owner.as_str(),
                        to_sql(expected_version)?
                    ],
                )
                .map_err(backend)?;

            if changed == 0 {
                // Zero rows means the version did not match, or there is no
                // such profile. Only a read tells them apart.
                let current: Option<i64> = transaction
                    .query_row(
                        "SELECT version FROM profiles WHERE user_id = ?1",
                        params![owner.as_str()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(backend)?;

                return Ok(match current {
                    Some(raw) => Update::Stale {
                        current_version: from_sql(raw),
                    },
                    None => Update::Missing,
                });
            }

            transaction
                .execute(
                    "INSERT INTO profile_revisions
                        (user_id, version, address, phone_number, recorded_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        owner.as_str(),
                        to_sql(next)?,
                        input.address,
                        input.phone_number,
                        now_ms()
                    ],
                )
                .map_err(backend)?;

            transaction.commit().map_err(backend)?;

            Ok(Update::Applied(Arc::new(Profile {
                user_id: owner,
                address: input.address,
                phone_number: input.phone_number,
                version: next,
            })))
        })
        .await
    }

    async fn history(&self, user_id: &UserId) -> Result<Vec<Revision>, StoreError> {
        let owner = user_id.clone();

        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT version, address, phone_number, recorded_at_ms
                       FROM profile_revisions
                      WHERE user_id = ?1
                   ORDER BY version ASC",
                )
                .map_err(backend)?;

            let revisions = statement
                .query_map(params![owner.as_str()], |row| {
                    Ok(Revision {
                        version: from_sql(row.get(0)?),
                        address: row.get(1)?,
                        phone_number: row.get(2)?,
                        recorded_at_ms: row.get(3)?,
                    })
                })
                .map_err(backend)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(backend)?;

            Ok(revisions)
        })
        .await
    }
}

/// Wraps a driver error, keeping the cause for logs and out of responses.
fn backend<E>(source: E) -> StoreError
where
    E: std::error::Error + Send + Sync + 'static,
{
    StoreError::Backend(Box::new(source))
}

/// SQLite's INTEGER is signed, so versions are stored as `i64`.
///
/// The conversion cannot fail below 2^63 writes to a single profile; treating
/// it as exhaustion rather than wrapping keeps the failure mode identical to
/// [`Version::checked_next`].
fn to_sql(version: Version) -> Result<i64, StoreError> {
    i64::try_from(version.get()).map_err(|_| StoreError::VersionExhausted)
}

/// Rebuilds a version from a stored counter, clamping a negative value to zero.
///
/// A negative version can only mean the row was written by something other than
/// this service; zero then fails every `If-Match`, which fails closed.
fn from_sql(raw: i64) -> Version {
    Version::from_counter(u64::try_from(raw).unwrap_or(0))
}

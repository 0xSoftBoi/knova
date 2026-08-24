//! One suite, run against every [`ProfileRepository`] implementation.
//!
//! This is the evidence that the concurrency guarantee belongs to the design
//! rather than to the `HashMap`. The in-memory backend enforces it with a
//! version check under a write lock; the SQLite backend enforces it with
//! `UPDATE ... WHERE version = ?` and a row count. If both pass identical
//! assertions, the contract is the thing being tested, not the mechanism.
//!
//! Adding a Postgres backend means adding one line to `backends!` and nothing
//! else.

use std::sync::Arc;

use common::dto::ProfileInput;
use common::{UserId, Version};
use profile_service::{InMemoryProfiles, ProfileRepository, SqliteProfiles, Update};

fn input(marker: &str) -> ProfileInput {
    ProfileInput {
        address: format!("address-{marker}"),
        phone_number: "5550100".to_owned(),
    }
}

fn user() -> UserId {
    UserId::from("conformance-user")
}

/// Generates the whole suite once per backend.
macro_rules! backends {
    ($($name:ident => $build:expr),+ $(,)?) => {
        $(
            mod $name {
                use super::*;

                fn repository() -> Arc<dyn ProfileRepository> {
                    $build
                }

                #[tokio::test]
                async fn create_then_read_round_trips() {
                    let repo = repository();
                    let created = repo
                        .create(&user(), input("first"))
                        .await
                        .expect("backend is healthy")
                        .expect("profile did not already exist");

                    assert_eq!(created.version, Version::INITIAL);

                    let read = repo
                        .get(&user())
                        .await
                        .expect("backend is healthy")
                        .expect("profile exists");
                    assert_eq!(read, created);
                }

                #[tokio::test]
                async fn create_is_refused_twice() {
                    let repo = repository();
                    repo.create(&user(), input("first")).await.expect("healthy");

                    let second = repo
                        .create(&user(), input("second"))
                        .await
                        .expect("healthy");
                    assert!(second.is_none(), "a second create must not overwrite");

                    let stored = repo.get(&user()).await.expect("healthy").expect("exists");
                    assert_eq!(stored.address, "address-first");
                }

                #[tokio::test]
                async fn update_at_the_current_version_is_applied() {
                    let repo = repository();
                    repo.create(&user(), input("first")).await.expect("healthy");

                    let outcome = repo
                        .update(&user(), Version::INITIAL, input("second"))
                        .await
                        .expect("healthy");

                    let Update::Applied(profile) = outcome else {
                        panic!("expected the update to be applied, got {outcome:?}");
                    };
                    assert_eq!(profile.address, "address-second");
                    assert_eq!(
                        profile.version,
                        Version::INITIAL.checked_next().expect("no overflow")
                    );
                }

                #[tokio::test]
                async fn update_at_a_stale_version_is_refused() {
                    let repo = repository();
                    repo.create(&user(), input("first")).await.expect("healthy");
                    repo.update(&user(), Version::INITIAL, input("second"))
                        .await
                        .expect("healthy");

                    // Replay the version the first writer already consumed.
                    let outcome = repo
                        .update(&user(), Version::INITIAL, input("clobber"))
                        .await
                        .expect("healthy");

                    assert_eq!(
                        outcome,
                        Update::Stale {
                            current_version: Version::INITIAL
                                .checked_next()
                                .expect("no overflow")
                        },
                        "a stale write must be refused and must report the winning version"
                    );

                    let stored = repo.get(&user()).await.expect("healthy").expect("exists");
                    assert_eq!(
                        stored.address, "address-second",
                        "the stale write must not have been applied"
                    );
                }

                #[tokio::test]
                async fn update_of_an_absent_profile_reports_missing() {
                    let repo = repository();
                    let outcome = repo
                        .update(&user(), Version::INITIAL, input("nothing"))
                        .await
                        .expect("healthy");

                    assert_eq!(outcome, Update::Missing);
                }

                #[tokio::test]
                async fn history_records_every_accepted_write() {
                    let repo = repository();
                    repo.create(&user(), input("first")).await.expect("healthy");

                    let mut version = Version::INITIAL;
                    for marker in ["second", "third"] {
                        let outcome = repo
                            .update(&user(), version, input(marker))
                            .await
                            .expect("healthy");
                        let Update::Applied(profile) = outcome else {
                            panic!("expected {marker} to apply");
                        };
                        version = profile.version;
                    }

                    // A rejected write must leave no trace in the audit log.
                    repo.update(&user(), Version::INITIAL, input("rejected"))
                        .await
                        .expect("healthy");

                    let history = repo.history(&user()).await.expect("healthy");
                    let addresses: Vec<_> =
                        history.iter().map(|r| r.address.as_str()).collect();

                    assert_eq!(
                        addresses,
                        ["address-first", "address-second", "address-third"],
                        "history must hold every accepted write, in order, and nothing else"
                    );
                    assert!(
                        history.windows(2).all(|w| w[0].version < w[1].version),
                        "revisions must be strictly ordered by version"
                    );
                }

                #[tokio::test]
                async fn reads_share_one_projection() {
                    let repo = repository();
                    repo.create(&user(), input("first")).await.expect("healthy");

                    let first = repo.get(&user()).await.expect("healthy").expect("exists");
                    let second = repo.get(&user()).await.expect("healthy").expect("exists");

                    // Structural, not a timing assertion: two reads that return
                    // pointers to the same allocation cannot have cloned it. If
                    // a future change rebuilds the projection per read, this
                    // fails immediately instead of quietly costing three
                    // allocations on the hottest path in the service.
                    //
                    // The SQL backend legitimately builds a fresh projection per
                    // read — it has to, the row lives in the database — so this
                    // asserts only that the in-memory backend keeps its
                    // sharing. See the `#[cfg]` on the assertion.
                    if stringify!($name) == "in_memory" {
                        assert!(
                            Arc::ptr_eq(&first, &second),
                            "in-memory reads must hand out the shared projection, not a clone"
                        );
                    }
                    assert_eq!(first, second);
                }

                #[tokio::test]
                async fn a_write_publishes_a_new_projection() {
                    let repo = repository();
                    repo.create(&user(), input("first")).await.expect("healthy");
                    let before = repo.get(&user()).await.expect("healthy").expect("exists");

                    repo.update(&user(), Version::INITIAL, input("second"))
                        .await
                        .expect("healthy");
                    let after = repo.get(&user()).await.expect("healthy").expect("exists");

                    assert!(
                        !Arc::ptr_eq(&before, &after),
                        "a write must publish a new projection, not mutate the one readers hold"
                    );
                    assert_eq!(before.address, "address-first");
                    assert_eq!(after.address, "address-second");
                }

                #[tokio::test]
                async fn history_of_an_absent_profile_is_empty() {
                    assert!(
                        repository()
                            .history(&user())
                            .await
                            .expect("healthy")
                            .is_empty()
                    );
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
                async fn concurrent_increments_never_lose_an_update() {
                    const WRITERS: usize = 32;

                    let repo = repository();
                    repo.create(&user(), input("0")).await.expect("healthy");

                    // Each writer's value depends on what it read, which is the
                    // only way a lost update becomes observable: counting
                    // accepted writes cannot distinguish compare-and-swap from
                    // last-write-wins, because both accept every write.
                    let writers = (0..WRITERS).map(|_| {
                        let repo = Arc::clone(&repo);
                        tokio::spawn(async move {
                            loop {
                                let current = repo
                                    .get(&user())
                                    .await
                                    .expect("healthy")
                                    .expect("exists");
                                let counter: usize = current
                                    .address
                                    .trim_start_matches("address-")
                                    .parse()
                                    .expect("address holds a counter");

                                let outcome = repo
                                    .update(
                                        &user(),
                                        current.version,
                                        input(&(counter + 1).to_string()),
                                    )
                                    .await
                                    .expect("healthy");

                                if matches!(outcome, Update::Applied(_)) {
                                    break;
                                }
                            }
                        })
                    });

                    for writer in writers.collect::<Vec<_>>() {
                        writer.await.expect("no writer panicked");
                    }

                    let stored = repo.get(&user()).await.expect("healthy").expect("exists");
                    assert_eq!(
                        stored.address,
                        format!("address-{WRITERS}"),
                        "every increment must survive"
                    );
                    assert_eq!(
                        repo.history(&user()).await.expect("healthy").len(),
                        WRITERS + 1,
                        "one revision per accepted write, plus the create"
                    );
                }
            }
        )+
    };
}

backends! {
    in_memory => Arc::new(InMemoryProfiles::new()),
    sqlite => Arc::new(SqliteProfiles::in_memory().expect("in-memory database opens")),
}

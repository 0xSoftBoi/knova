// Throughput arithmetic converts operation counts and nanosecond durations to
// `f64`. Both are far below 2^53 at any scale this runs at, so the precision
// clippy warns about cannot be lost here, and a per-site allow on every
// division would bury the measurements it is meant to guard.
#![allow(clippy::cast_precision_loss)]

//! Throughput measurements for the in-memory store. Not correctness checks, and
//! excluded from the normal run because timings are load-dependent.
//!
//! ```text
//! cargo test -p profile-service --release --test read_contention -- --ignored --nocapture
//! ```
//!
//! Every figure is a median of [`ROUNDS`] runs. A single run on a loaded box
//! varies by 20% or more, which is wider than most of the differences worth
//! measuring, so a lone number proves nothing.
//!
//! [`shard_scaling`] sweeps the shard count with everything else held constant,
//! which isolates that one variable instead of comparing two builds that differ
//! in three ways.

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::dto::ProfileInput;
use common::{UserId, Version};
use profile_service::{InMemoryProfiles, ProfileRepository, Update};

const ROUNDS: usize = 5;
const KEYS: usize = 10_000;

fn workers() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

fn input(marker: usize) -> ProfileInput {
    ProfileInput {
        address: format!("address-{marker}"),
        phone_number: "5550100".to_owned(),
    }
}

/// Seeds `keys` profiles and returns the store alongside the ids.
///
/// The ids are built once and shared: constructing a `UserId` per operation
/// would put a string allocation inside the measured loop and report the
/// benchmark's own cost as the store's.
async fn seeded(shards: usize, keys: usize) -> (Arc<InMemoryProfiles>, Arc<Vec<UserId>>) {
    let repository = Arc::new(InMemoryProfiles::with_shards(shards));
    let ids: Vec<UserId> = (0..keys)
        .map(|key| UserId::from(format!("user-{key}").as_str()))
        .collect();

    for id in &ids {
        repository.create(id, input(0)).await.expect("healthy");
    }

    (repository, Arc::new(ids))
}

fn per_sec(ops: usize, elapsed: Duration) -> f64 {
    ops as f64 / elapsed.as_secs_f64()
}

/// Median of `ROUNDS` runs, plus the spread, so a reader can see whether a
/// difference is signal.
fn median(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    (
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1],
    )
}

/// One round of `per_worker` reads each, spread across `ids`.
async fn spread_round(
    repository: &Arc<InMemoryProfiles>,
    ids: &Arc<Vec<UserId>>,
    per_worker: usize,
) -> Duration {
    let started = Instant::now();

    let tasks = (0..workers()).map(|w| {
        let repository = Arc::clone(repository);
        let ids = Arc::clone(ids);
        tokio::spawn(async move {
            // Stride by a prime so workers do not march in lockstep over the
            // same keys, which would understate contention.
            let mut key = w * 7919;
            for _ in 0..per_worker {
                key = (key + 7919) % ids.len();
                repository.get(&ids[key]).await.expect("healthy");
            }
        })
    });
    for task in tasks.collect::<Vec<_>>() {
        task.await.expect("no worker panicked");
    }

    started.elapsed()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement, not a correctness check"]
async fn shard_scaling() {
    const PER_WORKER: usize = 150_000;

    println!(
        "\n{} workers, {KEYS} keys, median of {ROUNDS} rounds\n",
        workers()
    );
    println!("{:>7}  {:>14}  {:>26}", "shards", "median ops/sec", "range");

    let mut single = 0.0;
    for shards in [1_usize, 2, 4, 16, 64, 256] {
        let (repository, ids) = seeded(shards, KEYS).await;

        let mut samples = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let elapsed = spread_round(&repository, &ids, PER_WORKER).await;
            samples.push(per_sec(workers() * PER_WORKER, elapsed));
        }

        let (mid, lo, hi) = median(samples);
        if shards == 1 {
            single = mid;
        }
        println!(
            "{shards:>7}  {mid:>14.0}  {:>10.0} .. {:<10.0}  {:+.0}%",
            lo,
            hi,
            (mid / single - 1.0) * 100.0
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement, not a correctness check"]
async fn hot_profile_reads() {
    const PER_WORKER: usize = 150_000;

    let (repository, ids) = seeded(64, 1).await;
    let mut samples = Vec::with_capacity(ROUNDS);

    for _ in 0..ROUNDS {
        let started = Instant::now();
        let tasks = (0..workers()).map(|_| {
            let repository = Arc::clone(&repository);
            let user = ids[0].clone();
            tokio::spawn(async move {
                for _ in 0..PER_WORKER {
                    repository.get(&user).await.expect("healthy");
                }
            })
        });
        for task in tasks.collect::<Vec<_>>() {
            task.await.expect("no worker panicked");
        }
        samples.push(per_sec(workers() * PER_WORKER, started.elapsed()));
    }

    let (mid, lo, hi) = median(samples);
    println!("\nhot profile reads   median {mid:.0} ops/sec   range {lo:.0} .. {hi:.0}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement, not a correctness check"]
async fn mixed_read_write() {
    const PER_WORKER: usize = 50_000;

    let mut samples = Vec::with_capacity(ROUNDS);

    for _ in 0..ROUNDS {
        let (repository, ids) = seeded(64, KEYS).await;
        let started = Instant::now();

        let tasks = (0..workers()).map(|w| {
            let repository = Arc::clone(&repository);
            let ids = Arc::clone(&ids);
            tokio::spawn(async move {
                let mut key = w * 7919;
                for step in 0..PER_WORKER {
                    key = (key + 7919) % ids.len();
                    let user = &ids[key];

                    if step % 10 == 9 {
                        let current = repository
                            .get(user)
                            .await
                            .expect("healthy")
                            .expect("seeded");
                        let _: Update = repository
                            .update(user, current.version, input(step))
                            .await
                            .expect("healthy");
                    } else {
                        repository.get(user).await.expect("healthy");
                    }
                }
            })
        });
        for task in tasks.collect::<Vec<_>>() {
            task.await.expect("no worker panicked");
        }
        samples.push(per_sec(workers() * PER_WORKER, started.elapsed()));
    }

    let (mid, lo, hi) = median(samples);
    println!("\nmixed 9:1 read/write   median {mid:.0} ops/sec   range {lo:.0} .. {hi:.0}");
}

/// Shared secret the benchmark routers are built with.
const BENCH_TOKEN: &str = "bench-internal-token";

/// Drives `requests` reads through a router and returns the elapsed time.
///
/// At module scope so both HTTP benchmarks use the identical request, and
/// because one round per stage — unrepeated — was giving impossible readings,
/// including a middleware layer that apparently made requests faster.
async fn drive(app: &axum::Router, user: &UserId, requests: usize) -> Duration {
    use axum::body::Body;
    use axum::http::{Request, header};
    use common::headers;
    use tower::ServiceExt;

    let started = Instant::now();
    for _ in 0..requests {
        let request = Request::builder()
            .method("GET")
            .uri("/profile")
            .header(headers::INTERNAL_TOKEN, BENCH_TOKEN)
            .header(headers::USER_ID, user.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::empty())
            .expect("well-formed");
        let response = app.clone().oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), 200);
    }
    started.elapsed()
}

/// Prices the boxed future that `#[async_trait]` allocates per call.
///
/// The trait has to be async because a SQL backend does I/O, and it has to be
/// object-safe so the backend is a deployment choice — which today means every
/// call heap-allocates a future, even for a backend that never blocks. This
/// measures that tax directly by running the identical work through the
/// inherent synchronous method and through the trait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement, not a correctness check"]
async fn async_trait_boxing_cost() {
    const PER_WORKER: usize = 150_000;

    let (repository, ids) = seeded(64, KEYS).await;

    let mut sync_samples = Vec::with_capacity(ROUNDS);
    let mut trait_samples = Vec::with_capacity(ROUNDS);

    for _ in 0..ROUNDS {
        let started = Instant::now();
        let tasks = (0..workers()).map(|w| {
            let repository = Arc::clone(&repository);
            let ids = Arc::clone(&ids);
            tokio::spawn(async move {
                let mut key = w * 7919;
                for _ in 0..PER_WORKER {
                    key = (key + 7919) % ids.len();
                    let _ = repository.read(&ids[key]);
                }
            })
        });
        for task in tasks.collect::<Vec<_>>() {
            task.await.expect("no worker panicked");
        }
        sync_samples.push(per_sec(workers() * PER_WORKER, started.elapsed()));

        trait_samples.push(per_sec(
            workers() * PER_WORKER,
            spread_round(&repository, &ids, PER_WORKER).await,
        ));
    }

    let (sync_mid, ..) = median(sync_samples);
    let (trait_mid, ..) = median(trait_samples);
    println!("\ninherent sync read     median {sync_mid:.0} ops/sec");
    println!(
        "through async trait    median {trait_mid:.0} ops/sec   ({:+.0}%)",
        (trait_mid / sync_mid - 1.0) * 100.0
    );
    println!(
        "boxed-future tax       {:.0} ns/op",
        (1.0 / trait_mid - 1.0 / sync_mid) * 1e9 * workers() as f64
    );
}

/// Puts the store's cost next to the cost of serving a request.
///
/// The number that decides whether any of the above is worth chasing. A store
/// read is measured in hundreds of nanoseconds; an HTTP request carries routing,
/// two extractors, a middleware, JSON serialisation and response construction.
/// If the transport is orders of magnitude more expensive, the store is not the
/// bottleneck and further micro-optimisation buys nothing a client can observe.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement, not a correctness check"]
async fn store_cost_versus_request_cost() {
    const REQUESTS: usize = 20_000;

    let store = Arc::new(InMemoryProfiles::new());
    let user = UserId::from("bench-user");
    store.create(&user, input(0)).await.expect("healthy");

    let app = profile_service::router(profile_service::AppState {
        profiles: Arc::clone(&store) as Arc<dyn ProfileRepository>,
        internal_token: Arc::from(BENCH_TOKEN),
        idempotency: Arc::new(profile_service::IdempotencyStore::default()),
    });

    let _ = drive(&app, &user, REQUESTS).await; // warm-up
    let elapsed = drive(&app, &user, REQUESTS).await;
    let http_ns = elapsed.as_nanos() as f64 / REQUESTS as f64;

    let started = Instant::now();
    for _ in 0..REQUESTS {
        let _ = store.read(&user);
    }
    let store_ns = started.elapsed().as_nanos() as f64 / REQUESTS as f64;

    // Where does the rest go? Serialisation is the one part of the handler
    // that could plausibly be precomputed, so it is worth pricing separately.
    let profile = store.read(&user).expect("seeded");
    let started = Instant::now();
    for _ in 0..REQUESTS {
        let _ = serde_json::to_vec(&*profile).expect("serialises");
    }
    let json_ns = started.elapsed().as_nanos() as f64 / REQUESTS as f64;

    println!(
        "\nstore read          {store_ns:>9.0} ns/op   {:>5.1}% of a request",
        store_ns / http_ns * 100.0
    );
    println!(
        "JSON serialisation  {json_ns:>9.0} ns/op   {:>5.1}% of a request",
        json_ns / http_ns * 100.0
    );
    println!(
        "HTTP GET /profile   {http_ns:>9.0} ns/op   ({:.0}x the store)",
        http_ns / store_ns
    );
    println!(
        "unaccounted (routing, extractors, middleware, response): {:>4.1}%",
        (http_ns - store_ns - json_ns) / http_ns * 100.0
    );
}

/// Prices each middleware layer by adding them back one at a time.
///
/// The previous measurement said 95% of a request is spent outside the store.
/// This says where: a layer that costs more than the work it wraps is a knob
/// worth knowing about before someone concludes the database is slow.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement, not a correctness check"]
async fn middleware_cost() {
    use axum::routing::get;
    use tower_http::trace::TraceLayer;

    const REQUESTS: usize = 20_000;
    let store = Arc::new(InMemoryProfiles::new());
    let user = UserId::from("bench-user");
    store.create(&user, input(0)).await.expect("healthy");

    let state = profile_service::AppState {
        profiles: Arc::clone(&store) as Arc<dyn ProfileRepository>,
        internal_token: Arc::from(BENCH_TOKEN),
        idempotency: Arc::new(profile_service::IdempotencyStore::default()),
    };

    let build = |stage: usize| {
        let mut app =
            axum::Router::new().route("/profile", get(profile_service::routes::profile::read));
        if stage >= 1 {
            app = app.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                profile_service::auth::gateway_guard,
            ));
        }
        if stage >= 2 {
            app = app.layer(TraceLayer::new_for_http());
        }
        app.with_state(state.clone())
    };

    let mut previous = 0.0;
    println!("\nmedian of {ROUNDS} rounds, {REQUESTS} requests each, warm-up discarded\n");
    for (stage, label) in [
        (0, "route + handler only"),
        (1, "+ gateway guard (auth + id)"),
        (2, "+ TraceLayer"),
    ] {
        let app = build(stage);
        let _ = drive(&app, &user, REQUESTS).await; // warm-up

        let mut samples = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let elapsed = drive(&app, &user, REQUESTS).await;
            samples.push(elapsed.as_nanos() as f64 / REQUESTS as f64);
        }
        samples.sort_by(f64::total_cmp);
        let ns = samples[samples.len() / 2];

        let delta = if stage == 0 {
            String::new()
        } else {
            format!("(+{:.0} ns)", ns - previous)
        };
        println!(
            "{label:<30} {ns:>7.0} ns/op   {delta:<14} [{:.0} .. {:.0}]",
            samples[0],
            samples[samples.len() - 1]
        );
        previous = ns;
    }
}

/// Keeps [`Version`] referenced so the import is not dead in every build.
#[allow(dead_code)]
fn _version_in_scope() -> Version {
    Version::INITIAL
}

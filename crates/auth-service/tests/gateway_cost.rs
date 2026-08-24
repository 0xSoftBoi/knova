//! What the gateway hop is actually made of.
//!
//! Measured over real HTTP, a request through the gateway costs about 2.9 ms at
//! p50 against 1.0 ms straight to the profile service. That 1.9 ms is the thing
//! worth attacking; nanoseconds of middleware are not. This prices the parts of
//! it that live in this service.
//!
//! ```text
//! cargo test -p auth-service --release --test gateway_cost -- --ignored --nocapture
//! ```

#![allow(clippy::cast_precision_loss)]

use std::time::{Duration, Instant};

use auth_service::{KeyMaterial, TokenService};
use common::UserId;

const ROUNDS: usize = 5;
const ITERATIONS: usize = 50_000;

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn time(iterations: usize, mut work: impl FnMut()) -> f64 {
    let started = Instant::now();
    for _ in 0..iterations {
        work();
    }
    started.elapsed().as_nanos() as f64 / iterations as f64
}

#[test]
#[ignore = "performance measurement, not a correctness check"]
fn token_verification_cost() {
    let tokens = TokenService::new(
        &KeyMaterial::new("k1", "measurement-secret"),
        &[],
        Duration::from_secs(900),
    );
    let user = UserId::random();
    let token = tokens.issue(&user, "alice").expect("signs");

    let verify = median(
        (0..ROUNDS)
            .map(|_| {
                time(ITERATIONS, || {
                    tokens.verify(&token).expect("valid");
                })
            })
            .collect(),
    );

    let issue = median(
        (0..ROUNDS)
            .map(|_| {
                time(ITERATIONS / 10, || {
                    tokens.issue(&user, "alice").expect("signs");
                })
            })
            .collect(),
    );

    println!("\ntoken verify   {verify:>8.0} ns/op");
    println!("token issue    {issue:>8.0} ns/op");
    println!();
    println!("A gateway request over real HTTP costs ~2.9 ms at p50 and ~1.0 ms");
    println!("straight to the upstream, so the hop is ~1.9 ms. Verification is");
    println!("{:.3}% of that hop.", verify / 1_900_000.0 * 100.0);
}

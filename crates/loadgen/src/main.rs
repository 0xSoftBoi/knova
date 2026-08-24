//! Load generator for the running services. A development tool, not part of
//! either service.
//!
//! Every other measurement in this workspace drives a router in-process with
//! `oneshot`, which skips the socket, the HTTP codec, and — for anything behind
//! the gateway — an entire second service. Those numbers are useful for
//! comparing two implementations of the same handler and useless for saying
//! what the system costs.
//!
//! This drives real HTTP against both entry points so the gateway hop can be
//! priced by difference:
//!
//! ```text
//! cargo run --release -p loadgen -- gateway 64 2000
//! cargo run --release -p loadgen -- direct   64 2000
//! ```

use std::env;
use std::time::{Duration, Instant};

use common::headers;

const AUTH: &str = "http://127.0.0.1:8080";
const PROFILE: &str = "http://127.0.0.1:8081";
const INTERNAL_TOKEN: &str = "dev-only-internal-token-change-me";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mode = args.first().map_or("gateway", String::as_str).to_owned();
    let connections: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(64);
    let per_connection: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1000);

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(connections)
        .build()?;

    // One login for the whole run: the point is to measure the read path, and
    // Argon2 would otherwise dominate every number here.
    let token: String = serde_json::from_str::<serde_json::Value>(
        &client
            .post(format!("{AUTH}/login"))
            .json(&serde_json::json!({
                "username": "alice",
                "password": "correct-horse-battery-staple"
            }))
            .send()
            .await?
            .text()
            .await?,
    )?["access_token"]
        .as_str()
        .ok_or("login did not return a token")?
        .to_owned();

    let user_id = subject_of(&token).ok_or("token carried no subject")?;

    // Ensure the profile exists; a 409 here means a previous run made it.
    let _ = client
        .post(format!("{AUTH}/profile"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "address": "1 Main St", "phone_number": "5550100" }))
        .send()
        .await?;

    let run = |warm: bool| {
        let client = client.clone();
        let token = token.clone();
        let user_id = user_id.clone();
        let mode = mode.clone();
        async move {
            let requests = if warm {
                per_connection / 4
            } else {
                per_connection
            };
            let started = Instant::now();

            let workers = (0..connections).map(|_| {
                let client = client.clone();
                let token = token.clone();
                let user_id = user_id.clone();
                let mode = mode.clone();
                tokio::spawn(async move {
                    let mut latencies = Vec::with_capacity(requests);
                    for _ in 0..requests {
                        let at = Instant::now();
                        let response = if mode == "direct" {
                            client
                                .get(format!("{PROFILE}/profile"))
                                .header(headers::INTERNAL_TOKEN, INTERNAL_TOKEN)
                                .header(headers::USER_ID, &user_id)
                                .send()
                                .await
                        } else {
                            client
                                .get(format!("{AUTH}/profile"))
                                .bearer_auth(&token)
                                .send()
                                .await
                        };
                        let status = response.expect("request failed").status();
                        assert!(status.is_success(), "unexpected status {status}");
                        latencies.push(at.elapsed());
                    }
                    latencies
                })
            });

            let mut all = Vec::with_capacity(connections * requests);
            for worker in workers.collect::<Vec<_>>() {
                all.extend(worker.await.expect("no worker panicked"));
            }
            (all, started.elapsed())
        }
    };

    let _ = run(true).await;
    let (mut latencies, elapsed) = run(false).await;

    latencies.sort_unstable();
    let total = latencies.len();
    #[allow(clippy::cast_precision_loss)]
    let throughput = total as f64 / elapsed.as_secs_f64();

    println!("\n{mode}: {connections} connections x {per_connection} requests");
    println!("  throughput  {throughput:>10.0} req/sec");
    println!("  p50         {:>10.2?}", percentile(&latencies, 50.0));
    println!("  p90         {:>10.2?}", percentile(&latencies, 90.0));
    println!("  p99         {:>10.2?}", percentile(&latencies, 99.0));
    println!("  max         {:>10.2?}", latencies[total - 1]);

    Ok(())
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let index = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
    sorted[index]
}

/// Pulls `sub` out of a token without verifying it. Fine here: the generator is
/// not a security boundary, it just needs the id the gateway will inject.
fn subject_of(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let mut decoded = Vec::new();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in payload.bytes() {
        // The alphabet is 64 entries, so the index always fits.
        let value = u32::try_from(alphabet.iter().position(|c| *c == byte)?).ok()?;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push(u8::try_from((buffer >> bits) & 0xFF).ok()?);
        }
    }
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims["sub"].as_str().map(str::to_owned)
}

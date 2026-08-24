//! Login throttling.
//!
//! Two limits with different jobs, because one control cannot do both.
//!
//! **Per-address**, answered with `429`. Bounds how fast any single source can
//! guess, regardless of which account it aims at. Safe to report honestly: the
//! response depends on the caller's address, not on whether a username exists.
//!
//! **Per-account**, answered with the *ordinary* `401`. Stops credential
//! stuffing against one account from a botnet, where no single address trips
//! the address limit.
//!
//! # The trap
//!
//! A per-account lockout is the classic way to reintroduce the user enumeration
//! this service works to prevent. Answer a locked account with `429` and an
//! unknown one with `401`, and an attacker learns which usernames are real by
//! failing at them five times. Answer both with `401` but skip the Argon2
//! verification on the locked path, and the answer comes back in five
//! milliseconds instead of fifty — the same oracle, read off a stopwatch.
//!
//! So a throttled account gets the identical `401`, and the caller still pays
//! for a full verification (see [`Verdict::AccountThrottled`] in
//! `routes::login`). Throttling here buys credential-stuffing resistance, not
//! CPU: resource exhaustion is the semaphore's job and the address limit's.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// What the throttle permits for one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Proceed normally.
    Allowed,
    /// The calling address has failed too often. Answer `429`; this reveals
    /// nothing about any account.
    AddressThrottled,
    /// This account has failed too often. Answer with the ordinary
    /// `401`, after doing the same work a real rejection would.
    AccountThrottled,
}

/// Failure counting for one key within a rolling window.
#[derive(Debug)]
struct Failures {
    count: u32,
    window_started: Instant,
}

/// Counts recent login failures per account and per source address.
#[derive(Debug)]
pub struct LoginThrottle {
    by_account: Mutex<HashMap<String, Failures>>,
    by_address: Mutex<HashMap<IpAddr, Failures>>,
    account_limit: u32,
    address_limit: u32,
    window: Duration,
}

impl LoginThrottle {
    /// Failures against one account before it is throttled.
    pub const DEFAULT_ACCOUNT_LIMIT: u32 = 5;
    /// Failures from one address before it is throttled. Higher than the
    /// account limit because an office or mobile network shares an address.
    pub const DEFAULT_ADDRESS_LIMIT: u32 = 20;
    /// How long failures are remembered.
    pub const DEFAULT_WINDOW: Duration = Duration::from_secs(300);

    /// Builds a throttle with the given policy.
    #[must_use]
    pub fn new(account_limit: u32, address_limit: u32, window: Duration) -> Self {
        Self {
            by_account: Mutex::new(HashMap::new()),
            by_address: Mutex::new(HashMap::new()),
            account_limit,
            address_limit,
            window,
        }
    }

    /// Checks both limits for an attempt.
    ///
    /// The address limit is evaluated first: it is the one that can be reported
    /// honestly, and reporting it costs nothing.
    #[must_use]
    pub fn check(&self, username: &str, address: Option<IpAddr>) -> Verdict {
        if let Some(address) = address
            && over(
                &mut self.by_address.lock(),
                &address,
                self.address_limit,
                self.window,
            )
        {
            return Verdict::AddressThrottled;
        }

        if over(
            &mut self.by_account.lock(),
            &username.to_owned(),
            self.account_limit,
            self.window,
        ) {
            return Verdict::AccountThrottled;
        }

        Verdict::Allowed
    }

    /// Records a failed attempt against both keys.
    pub fn record_failure(&self, username: &str, address: Option<IpAddr>) {
        bump(
            &mut self.by_account.lock(),
            username.to_owned(),
            self.window,
        );
        if let Some(address) = address {
            bump(&mut self.by_address.lock(), address, self.window);
        }
    }

    /// Clears the account's counter after a successful login.
    ///
    /// The address counter is deliberately *not* cleared: one success does not
    /// excuse nineteen failures from the same source, which is exactly the
    /// shape of a spray across many accounts.
    pub fn record_success(&self, username: &str) {
        self.by_account.lock().remove(username);
    }
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_ACCOUNT_LIMIT,
            Self::DEFAULT_ADDRESS_LIMIT,
            Self::DEFAULT_WINDOW,
        )
    }
}

/// Whether `key` is over `limit` within the current window, expiring a stale one.
fn over<K: std::hash::Hash + Eq>(
    counters: &mut HashMap<K, Failures>,
    key: &K,
    limit: u32,
    window: Duration,
) -> bool {
    match counters.get(key) {
        Some(failures) if failures.window_started.elapsed() < window => failures.count >= limit,
        Some(_) => {
            counters.remove(key);
            false
        }
        None => false,
    }
}

/// Adds one failure to `key`, starting a fresh window if the last has lapsed.
fn bump<K: std::hash::Hash + Eq>(counters: &mut HashMap<K, Failures>, key: K, window: Duration) {
    let entry = counters.entry(key).or_insert_with(|| Failures {
        count: 0,
        window_started: Instant::now(),
    });

    if entry.window_started.elapsed() >= window {
        entry.count = 0;
        entry.window_started = Instant::now();
    }

    entry.count += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = Duration::from_millis(50);

    #[test]
    fn an_account_is_throttled_after_the_limit() {
        let throttle = LoginThrottle::new(3, 100, Duration::from_secs(60));

        for _ in 0..3 {
            assert_eq!(throttle.check("alice", None), Verdict::Allowed);
            throttle.record_failure("alice", None);
        }

        assert_eq!(throttle.check("alice", None), Verdict::AccountThrottled);
        assert_eq!(
            throttle.check("bob", None),
            Verdict::Allowed,
            "throttling one account must not affect another"
        );
    }

    #[test]
    fn success_clears_the_account_counter() {
        let throttle = LoginThrottle::new(3, 100, Duration::from_secs(60));

        throttle.record_failure("alice", None);
        throttle.record_failure("alice", None);
        throttle.record_success("alice");
        throttle.record_failure("alice", None);

        assert_eq!(throttle.check("alice", None), Verdict::Allowed);
    }

    #[test]
    fn an_address_is_throttled_independently_of_the_account() {
        let address = Some(IpAddr::from([203, 0, 113, 7]));
        let throttle = LoginThrottle::new(100, 3, Duration::from_secs(60));

        // A spray: a different username every time, so no account limit trips.
        for attempt in 0..3 {
            let username = format!("victim-{attempt}");
            assert_eq!(throttle.check(&username, address), Verdict::Allowed);
            throttle.record_failure(&username, address);
        }

        assert_eq!(
            throttle.check("victim-99", address),
            Verdict::AddressThrottled,
            "a spray across accounts must still be caught by the address limit"
        );
    }

    #[test]
    fn counters_lapse_with_the_window() {
        let throttle = LoginThrottle::new(2, 100, FAST);

        throttle.record_failure("alice", None);
        throttle.record_failure("alice", None);
        assert_eq!(throttle.check("alice", None), Verdict::AccountThrottled);

        std::thread::sleep(FAST * 2);
        assert_eq!(
            throttle.check("alice", None),
            Verdict::Allowed,
            "failures must not be remembered past the window"
        );
    }
}

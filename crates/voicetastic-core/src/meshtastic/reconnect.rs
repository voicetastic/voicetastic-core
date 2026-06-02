//! Sans-IO reconnect-backoff policy shared by every driver (native
//! btleplug, web-sys Web Bluetooth, future Serial reconnect).
//!
//! The policy is pure state + pure functions: it tracks the number of
//! consecutive failures and produces a [`Duration`] for the next
//! attempt. The driver is responsible for actually sleeping that long,
//! attempting the connect, and calling [`BleReconnectPolicy::record_failure`]
//! or [`BleReconnectPolicy::reset`] depending on the outcome.
//!
//! Why is this in core? Native and web BLE share the protocol but not
//! the runtime (Send + multi-thread tokio vs `!Send` single-thread wasm),
//! so concrete transport code doesn't unify well. A small sans-IO
//! state-machine like this one is the only piece of the BLE driver
//! that's identical across the two runtimes — keeping it in core means
//! changing the back-off curve once instead of twice.
//!
//! The defaults aim at the BLE-on-the-go failure profile: short first
//! retry (transient interference clears in 1–2 s), gentle exponential
//! up to one-per-minute, give up after ten consecutive failures
//! (~20 minutes of attempts), so a stale tab doesn't burn the radio's
//! BLE controller for the rest of the day.

use std::time::Duration;

/// Tuning knobs for [`BleReconnectPolicy`]. All fields are `pub` so a
/// caller can build a custom config; [`Default`] is the standard
/// "BLE-on-the-go" profile described in the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BleReconnectConfig {
    /// Delay before the first retry attempt.
    pub initial_delay: Duration,
    /// Multiplier applied to the delay on each subsequent failure.
    /// Stored as a numerator/denominator pair so the config stays
    /// `Eq` (no `f32`) and exponential growth is computed without
    /// floating-point drift.
    ///
    /// Default is `2/1`, doubling each attempt; `3/2` (1.5x) is a
    /// good "gentle" choice when retries are cheap.
    pub backoff_num: u32,
    pub backoff_den: u32,
    /// Upper bound on the delay between attempts. Once the geometric
    /// growth reaches this, the policy stays here for every
    /// subsequent attempt until `max_attempts` (if set) trips.
    pub max_delay: Duration,
    /// Stop retrying after this many consecutive failures. `None`
    /// keeps retrying forever (rarely what you want for a phone in
    /// a pocket — that's how batteries die).
    pub max_attempts: Option<u32>,
}

impl Default for BleReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(2),
            backoff_num: 2,
            backoff_den: 1,
            max_delay: Duration::from_secs(60),
            max_attempts: Some(10),
        }
    }
}

/// Tracks the geometric back-off state for one reconnect campaign.
/// One instance per attempted reconnect after a graceful or unexpected
/// disconnect; throw it away (or call [`reset`]) once the link is
/// re-established.
#[derive(Debug, Clone)]
pub struct BleReconnectPolicy {
    config: BleReconnectConfig,
    attempts: u32,
}

impl BleReconnectPolicy {
    /// Build a policy from a config. Use [`BleReconnectPolicy::default`]
    /// for the standard profile.
    pub fn new(config: BleReconnectConfig) -> Self {
        Self {
            config,
            attempts: 0,
        }
    }

    /// How long to wait before the next attempt. Always returns a
    /// positive duration; safe to call even after `should_give_up`
    /// returns `true` (callers usually check `should_give_up` first
    /// and bail without sleeping).
    pub fn next_delay(&self) -> Duration {
        // Multiply `initial_delay` by `(num/den)^attempts`, capped at
        // `max_delay`. Computed in integer milliseconds because
        // Duration's f32 path drifts after a few iterations.
        let mut ms = self.config.initial_delay.as_millis();
        let cap = self.config.max_delay.as_millis();
        let num = self.config.backoff_num as u128;
        let den = self.config.backoff_den as u128;
        if den == 0 {
            // Defensive: a zero denominator would `/ 0`. Treat as no
            // growth so we don't crash on a bad config.
            return self.config.initial_delay;
        }
        for _ in 0..self.attempts {
            ms = (ms.saturating_mul(num)) / den;
            if ms >= cap {
                return self.config.max_delay;
            }
        }
        Duration::from_millis(ms.min(u64::MAX as u128) as u64)
    }

    /// Record a failed reconnect attempt. Bumps the attempt counter
    /// so the next [`next_delay`] returns a longer wait.
    pub fn record_failure(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Connection succeeded — reset the attempt counter so the next
    /// disconnect starts from `initial_delay` again. Drop the policy
    /// entirely if you'd rather build a fresh one on the next drop.
    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    /// `true` if [`BleReconnectConfig::max_attempts`] is reached.
    pub fn should_give_up(&self) -> bool {
        matches!(self.config.max_attempts, Some(max) if self.attempts >= max)
    }

    /// Number of consecutive failures recorded so far. Mostly for
    /// status/log surfaces ("Reconnecting (attempt 3/10)…").
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

impl Default for BleReconnectPolicy {
    fn default() -> Self {
        Self::new(BleReconnectConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_curve_doubles_until_cap() {
        let mut p = BleReconnectPolicy::default();
        assert_eq!(p.next_delay(), Duration::from_secs(2));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_secs(4));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_secs(8));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_secs(16));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_secs(32));
        p.record_failure();
        // (2 * 2^5) = 64 > cap 60 → cap holds.
        assert_eq!(p.next_delay(), Duration::from_secs(60));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_secs(60));
    }

    #[test]
    fn give_up_after_max_attempts() {
        let mut p = BleReconnectPolicy::new(BleReconnectConfig {
            max_attempts: Some(3),
            ..BleReconnectConfig::default()
        });
        assert!(!p.should_give_up());
        p.record_failure();
        assert!(!p.should_give_up());
        p.record_failure();
        assert!(!p.should_give_up());
        p.record_failure();
        assert!(p.should_give_up());
    }

    #[test]
    fn reset_returns_to_initial_delay() {
        let mut p = BleReconnectPolicy::default();
        for _ in 0..5 {
            p.record_failure();
        }
        assert!(p.next_delay() > Duration::from_secs(2));
        p.reset();
        assert_eq!(p.next_delay(), Duration::from_secs(2));
        assert!(!p.should_give_up());
    }

    #[test]
    fn gentle_curve_15x() {
        let mut p = BleReconnectPolicy::new(BleReconnectConfig {
            initial_delay: Duration::from_millis(1_000),
            backoff_num: 3,
            backoff_den: 2,
            max_delay: Duration::from_secs(30),
            max_attempts: None,
        });
        assert_eq!(p.next_delay(), Duration::from_millis(1_000));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_millis(1_500));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_millis(2_250));
        p.record_failure();
        assert_eq!(p.next_delay(), Duration::from_millis(3_375));
    }

    #[test]
    fn no_max_means_never_gives_up() {
        let mut p = BleReconnectPolicy::new(BleReconnectConfig {
            max_attempts: None,
            ..BleReconnectConfig::default()
        });
        for _ in 0..1_000 {
            p.record_failure();
        }
        assert!(!p.should_give_up());
        assert_eq!(p.next_delay(), Duration::from_secs(60));
    }

    #[test]
    fn zero_denominator_falls_back_to_initial() {
        // Pathological config; should not panic.
        let p = BleReconnectPolicy::new(BleReconnectConfig {
            backoff_den: 0,
            ..BleReconnectConfig::default()
        });
        assert_eq!(p.next_delay(), Duration::from_secs(2));
    }
}

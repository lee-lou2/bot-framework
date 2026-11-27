//! Body size, attachment count, and token-bucket rate limits for gateway ingress.
mod failures;
pub use failures::SourceFailures;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub body_bytes: usize,
    pub attachments: usize,
    /// Per-binding requests per minute, including unauthenticated/unregistered attempts.
    pub per_entrance_per_min: u32,
    pub per_principal_per_min: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            body_bytes: 64 * 1024,
            attachments: 8,
            per_entrance_per_min: 120,
            per_principal_per_min: 30,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.body_bytes == 0 || self.body_bytes > 1024 * 1024 {
            return Err("body_bytes must be between 1 byte and 1 MiB");
        }
        if self.attachments == 0 || self.attachments > 64 {
            return Err("attachments must be between 1 and 64");
        }
        if self.per_entrance_per_min == 0 || self.per_principal_per_min == 0 {
            return Err("per-minute rate limits must be at least 1");
        }
        Ok(())
    }
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Keyed token bucket; callers must bound keys to registered entrances/principals to prevent unbounded growth.
pub struct RateLimiter {
    per_min: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

const PRUNE_ABOVE: usize = 4096;

impl RateLimiter {
    pub fn new(per_min: u32) -> Self {
        Self {
            per_min: f64::from(per_min.max(1)),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let rate = self.per_min / 60.0;
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PRUNE_ABOVE {
            // Fully refilled idle buckets are indistinguishable from absent entries.
            let full = self.per_min / rate;
            map.retain(|_, b| now.saturating_duration_since(b.at).as_secs_f64() < full);
        }
        let bucket = map.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.per_min,
            at: now,
        });
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(self.per_min);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / rate))
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_the_quota_at_once_then_refuses() {
        let limiter = RateLimiter::new(3);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check_at("desk", t0).is_ok());
        }
        let wait = limiter.check_at("desk", t0).unwrap_err();
        assert!(
            wait > Duration::from_secs(19) && wait <= Duration::from_secs(20),
            "{wait:?}"
        );
    }

    #[test]
    fn refills_over_time_without_exceeding_capacity() {
        let limiter = RateLimiter::new(60);
        let t0 = Instant::now();
        for _ in 0..60 {
            limiter.check_at("k", t0).unwrap();
        }
        assert!(limiter.check_at("k", t0).is_err());
        assert!(
            limiter
                .check_at("k", t0 + Duration::from_millis(1100))
                .is_ok()
        );
        assert!(
            limiter
                .check_at("k", t0 + Duration::from_millis(1200))
                .is_err()
        );
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..60 {
            limiter.check_at("k", later).unwrap();
        }
        assert!(limiter.check_at("k", later).is_err());
    }

    #[test]
    fn keys_do_not_share_budget() {
        let limiter = RateLimiter::new(1);
        let t0 = Instant::now();
        assert!(limiter.check_at("a", t0).is_ok());
        assert!(limiter.check_at("a", t0).is_err());
        assert!(limiter.check_at("b", t0).is_ok());
    }

    #[test]
    fn prunes_idle_buckets_when_the_map_grows() {
        let limiter = RateLimiter::new(10);
        let t0 = Instant::now();
        for i in 0..(PRUNE_ABOVE + 10) {
            limiter.check_at(&format!("k{i}"), t0).unwrap();
        }
        assert!(limiter.len() > PRUNE_ABOVE);
        limiter
            .check_at("fresh", t0 + Duration::from_secs(3600))
            .unwrap();
        assert!(limiter.len() < 10, "{}", limiter.len());
    }

    #[test]
    fn limits_validate_rejects_zero_and_huge_values() {
        assert!(Limits::default().validate().is_ok());
        for bad in [
            Limits {
                body_bytes: 0,
                ..Limits::default()
            },
            Limits {
                body_bytes: 2 * 1024 * 1024,
                ..Limits::default()
            },
            Limits {
                attachments: 0,
                ..Limits::default()
            },
            Limits {
                per_entrance_per_min: 0,
                ..Limits::default()
            },
            Limits {
                per_principal_per_min: 0,
                ..Limits::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}

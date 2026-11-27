//! Per-principal token-bucket rate and concurrency limiter scoped per profile.
//!
//! Accounting is process-local (multi-broker deployments scale limits by broker count).

use crate::sys::Clock;
use bot_core::id::{PrincipalId, ProfileId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateConfig {
    pub per_minute: u32,
    pub burst: u32,
    pub concurrent: u32,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            per_minute: 60,
            burst: 20,
            concurrent: 4,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Limit {
    Rate,
    Concurrency,
}

type Key = (ProfileId, PrincipalId);

struct Bucket {
    tokens: f64,
    at: SystemTime,
    in_flight: u32,
}

/// Bucket count threshold for sweeping idle, fully refilled entries to bound memory growth.
const SWEEP_AT: usize = 10_000;

struct State {
    buckets: HashMap<Key, Bucket>,
    next_sweep_at: usize,
}

pub struct Limiter {
    cfg: RateConfig,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

/// RAII concurrency permit released on drop.
pub struct Slot {
    limiter: Arc<Limiter>,
    key: Key,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(bucket) = self.limiter.lock().buckets.get_mut(&self.key) {
            bucket.in_flight = bucket.in_flight.saturating_sub(1);
        }
    }
}

impl Limiter {
    pub fn new(cfg: RateConfig, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            clock,
            state: Mutex::new(State {
                buckets: HashMap::new(),
                next_sweep_at: SWEEP_AT,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admits one call if within concurrency and token limits; rejected calls do not consume tokens.
    pub fn admit(
        self: &Arc<Self>,
        profile: &ProfileId,
        principal: &PrincipalId,
    ) -> Result<Slot, Limit> {
        let now = self.clock.now();
        let key = (profile.clone(), principal.clone());
        let mut state = self.lock();
        let burst = f64::from(self.cfg.burst);
        let rate = f64::from(self.cfg.per_minute) / 60.0;
        if state.buckets.len() >= state.next_sweep_at {
            state.buckets.retain(|_, b| {
                if b.in_flight > 0 {
                    return true;
                }
                let elapsed = now.duration_since(b.at).unwrap_or_default().as_secs_f64();
                let effective = (b.tokens + elapsed * rate).min(burst);
                effective < burst
            });
            state.next_sweep_at = state.buckets.len().saturating_mul(2).max(SWEEP_AT);
        }
        let bucket = state.buckets.entry(key.clone()).or_insert(Bucket {
            tokens: burst,
            at: now,
            in_flight: 0,
        });
        // Never refill if the wall clock moves backwards.
        let elapsed = now
            .duration_since(bucket.at)
            .unwrap_or_default()
            .as_secs_f64();
        bucket.at = now.max(bucket.at);
        bucket.tokens = (bucket.tokens + elapsed * rate).min(burst);
        if bucket.in_flight >= self.cfg.concurrent {
            return Err(Limit::Concurrency);
        }
        if bucket.tokens < 1.0 {
            return Err(Limit::Rate);
        }
        bucket.tokens -= 1.0;
        bucket.in_flight += 1;
        Ok(Slot {
            limiter: Arc::clone(self),
            key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ManualClock;
    use std::time::Duration;

    fn who(profile: &str, principal: &str) -> (ProfileId, PrincipalId) {
        (ProfileId(profile.into()), PrincipalId(principal.into()))
    }

    fn limiter(per_minute: u32, burst: u32, concurrent: u32) -> (Arc<Limiter>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(1_000));
        let limiter = Limiter::new(
            RateConfig {
                per_minute,
                burst,
                concurrent,
            },
            clock.clone(),
        );
        (limiter, clock)
    }

    #[test]
    fn burst_then_refill_by_elapsed_time() {
        let (limiter, clock) = limiter(60, 3, 100);
        let (p, a) = who("personal", "local:owner");
        for _ in 0..3 {
            drop(limiter.admit(&p, &a).unwrap());
        }
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Rate));
        clock.advance(Duration::from_secs(1));
        drop(limiter.admit(&p, &a).unwrap());
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Rate));
        clock.advance(Duration::from_secs(3600));
        for _ in 0..3 {
            drop(limiter.admit(&p, &a).unwrap());
        }
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Rate));
    }

    #[test]
    fn concurrency_slots_return_when_dropped() {
        let (limiter, _) = limiter(6000, 100, 2);
        let (p, a) = who("personal", "local:owner");
        let first = limiter.admit(&p, &a).unwrap();
        let _second = limiter.admit(&p, &a).unwrap();
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Concurrency));
        drop(first);
        let _third = limiter.admit(&p, &a).unwrap();
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Concurrency));
    }

    #[test]
    fn refused_calls_do_not_spend_tokens() {
        let (limiter, clock) = limiter(60, 1, 1);
        let (p, a) = who("personal", "local:owner");
        let held = limiter.admit(&p, &a).unwrap();
        for _ in 0..5 {
            assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Concurrency));
        }
        drop(held);
        clock.advance(Duration::from_secs(1));
        assert!(limiter.admit(&p, &a).is_ok());
    }

    #[test]
    fn principals_and_profiles_are_counted_separately() {
        let (limiter, _) = limiter(60, 1, 1);
        let (p, alice) = who("personal", "local:alice");
        let (_, service) = who("personal", "service:morning");
        let (other, _) = who("other", "x");
        let _a = limiter.admit(&p, &alice).unwrap();
        let _s = limiter.admit(&p, &service).unwrap();
        let _o = limiter.admit(&other, &alice).unwrap();
        assert_eq!(limiter.admit(&p, &alice).err(), Some(Limit::Concurrency));
    }

    #[test]
    fn a_clock_that_runs_backwards_never_refills() {
        let (limiter, clock) = limiter(60, 1, 10);
        let (p, a) = who("personal", "local:owner");
        drop(limiter.admit(&p, &a).unwrap());
        clock.set(500);
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Rate));
    }

    #[test]
    fn idle_buckets_are_evicted_after_refill_and_sweep_threshold_scales_for_active_buckets() {
        let (limiter, clock) = limiter(60, 2, 10);
        let p = ProfileId("personal".into());
        for i in 0..SWEEP_AT {
            let principal = PrincipalId(format!("local:u{i}"));
            drop(limiter.admit(&p, &principal).unwrap());
        }
        assert_eq!(limiter.lock().buckets.len(), SWEEP_AT);
        // Unrefilled buckets stay retained and double `next_sweep_at` to avoid O(N) scans per admit.
        let extra = PrincipalId("local:extra".into());
        let held = limiter.admit(&p, &extra).unwrap();
        {
            let st = limiter.lock();
            assert_eq!(st.buckets.len(), SWEEP_AT + 1);
            assert_eq!(st.next_sweep_at, SWEEP_AT * 2);
        }
        let mut st_count_before = limiter.lock().buckets.len();
        for i in st_count_before..(SWEEP_AT * 2) {
            let principal = PrincipalId(format!("local:u{i}"));
            drop(limiter.admit(&p, &principal).unwrap());
        }
        st_count_before = limiter.lock().buckets.len();
        assert_eq!(st_count_before, SWEEP_AT * 2);
        clock.advance(Duration::from_secs(60));
        let fresh = PrincipalId("local:fresh".into());
        drop(limiter.admit(&p, &fresh).unwrap());
        {
            let st = limiter.lock();
            // Only the in-flight `extra` and newly added `fresh` buckets remain.
            assert_eq!(st.buckets.len(), 2);
            assert_eq!(st.next_sweep_at, SWEEP_AT);
        }
        drop(held);
    }
}

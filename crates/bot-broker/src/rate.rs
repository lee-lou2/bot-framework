//! 주체별 속도·동시성 제한. 프로필과 주체(cron의 서비스 주체 포함)마다 따로 센다.
//! 토큰 버킷(분당 보충량·최대 순간량)과 동시에 진행 중인 호출 수를 함께 본다.
//! 한도는 호스트 프로세스 안에서만 센다(분리 배치에서 브로커를 여러 개 두면 개수만큼 늘어난다).

use crate::sys::Clock;
use bot_core::id::{PrincipalId, ProfileId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateConfig {
    /// 분당 보충하는 호출 수.
    pub per_minute: u32,
    /// 한꺼번에 쓸 수 있는 최대 호출 수(버킷 크기).
    pub burst: u32,
    /// 동시에 진행할 수 있는 호출 수.
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

/// 주체 수가 늘어도 메모리가 무한히 자라지 않게 쓰지 않는 항목을 치우는 기준.
const SWEEP_AT: usize = 10_000;

pub struct Limiter {
    cfg: RateConfig,
    clock: Arc<dyn Clock>,
    state: Mutex<HashMap<Key, Bucket>>,
}

/// 동시성 자리. 놓으면(drop) 반환된다.
pub struct Slot {
    limiter: Arc<Limiter>,
    key: Key,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(bucket) = self.limiter.lock().get_mut(&self.key) {
            bucket.in_flight = bucket.in_flight.saturating_sub(1);
        }
    }
}

impl Limiter {
    pub fn new(cfg: RateConfig, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            clock,
            state: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Bucket>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 호출 하나를 받아들인다. 속도 초과나 동시성 초과면 거절한다. 거절된 호출은 토큰을 쓰지 않는다.
    pub fn admit(
        self: &Arc<Self>,
        profile: &ProfileId,
        principal: &PrincipalId,
    ) -> Result<Slot, Limit> {
        let now = self.clock.now();
        let key = (profile.clone(), principal.clone());
        let mut state = self.lock();
        if state.len() >= SWEEP_AT {
            let burst = f64::from(self.cfg.burst);
            state.retain(|_, b| b.in_flight > 0 || b.tokens < burst);
        }
        let bucket = state.entry(key.clone()).or_insert(Bucket {
            tokens: f64::from(self.cfg.burst),
            at: now,
            in_flight: 0,
        });
        // 시계가 거꾸로 가면 보충하지 않는다.
        let elapsed = now
            .duration_since(bucket.at)
            .unwrap_or_default()
            .as_secs_f64();
        bucket.at = now.max(bucket.at);
        bucket.tokens = (bucket.tokens + elapsed * f64::from(self.cfg.per_minute) / 60.0)
            .min(f64::from(self.cfg.burst));
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
        clock.advance(Duration::from_secs(1)); // 분당 60 = 초당 1개
        drop(limiter.admit(&p, &a).unwrap());
        assert_eq!(limiter.admit(&p, &a).err(), Some(Limit::Rate));
        clock.advance(Duration::from_secs(3600)); // 오래 쉬어도 버킷 크기를 넘지 않는다
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
}

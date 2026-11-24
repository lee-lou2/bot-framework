//! 본문·첨부 한도와 속도 제한. 어댑터가 읽기 전에(본문 크기) 또는 접수 전에(속도) 적용한다.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 호스트 파일로 조정하는 한도. 0은 허용하지 않는다(`validate`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// 어댑터가 받는 본문 한 건의 최대 바이트 (HTTP 본문·CLI 한 줄·Slack 글)
    pub body_bytes: usize,
    /// 한 요청에 붙는 첨부 참조의 최대 개수
    pub attachments: usize,
    /// 입구(바인딩) 하나가 분당 받는 요청 수. 미등록 사용자의 시도도 센다.
    pub per_entrance_per_min: u32,
    /// 등록된 주체 한 명이 분당 보내는 요청 수
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
            return Err("본문 한도는 1바이트 이상 1 MiB 이하여야 해요");
        }
        if self.attachments == 0 || self.attachments > 64 {
            return Err("첨부 한도는 1~64개여야 해요");
        }
        if self.per_entrance_per_min == 0 || self.per_principal_per_min == 0 {
            return Err("분당 요청 한도는 1 이상이어야 해요");
        }
        Ok(())
    }
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// 키별 토큰 버킷. 용량은 분당 한도와 같아 «분당 N건»을 한꺼번에 쓸 수 있고 초당 N/60개씩 찬다.
/// 키는 호출하는 쪽이 정한다 — 입구 키와 등록된 주체 키만 넣어 상태가 무한히 늘지 않게 한다.
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

    /// 한 건을 쓴다. 모자라면 다음 한 건이 가능해질 때까지의 시간을 돌려준다.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let rate = self.per_min / 60.0;
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PRUNE_ABOVE {
            // 가득 찬 채 쉰 버킷은 새로 만든 것과 같으니 지운다
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
        // 분당 3건이면 한 건이 차는 데 20초
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
        // 초당 1건
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
        // 오래 쉬어도 용량(60)을 넘지 않는다
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

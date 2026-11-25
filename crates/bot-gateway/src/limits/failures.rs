//! 출처(접속한 피어 IP)마다 따로 세는 API 키 인증 실패 예산. 키 인증은 시도마다 모든 슬롯을 비밀 보관소에서 읽으므로(키체인이면 슬롯마다 프로세스 실행) 맞지 않는 키를
//! 계속 보내는 출처가 보관소에 부하를 줄 수 있다. 한 출처가 집계 창 안에 `max`번 실패하면 `block` 동안 그 출처의 요청은 **보관소를 읽지 않고** 거절한다.
//!
//! - 출처는 실제 연결의 피어 주소(`ConnectInfo`)다. `X-Forwarded-For` 같은 요청 머리는 속일 수 있어 쓰지 않는다. IPv4-매핑 IPv6는 IPv4로 바로잡아 센다.
//!   피어 주소를 알 수 없는 경로는 한 칸을 같이 쓴다(관리 화면과 같다). TLS 프록시 뒤에서는 프록시 주소 하나가 한 출처다.
//! - 한 출처의 실패가 다른 출처를 막지 않는다. 성공은 예산을 되돌리지 않는다(맞는 키로 섞어 보내며 추측을 이어 가지 못하게).
//! - 기억하는 출처 수는 한정한다(가득 차면 더는 기억할 이유가 없는 것부터, 그다음 차단 중이 아닌 것 가운데 가장 오래된 것, 모두 차단 중일 때만 차단이 가장 먼저 끝나는 것을 버린다).
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

/// 기본값: 집계 창 60초 안에 20번 실패하면 60초 동안 막는다.
pub const DEFAULT_MAX: usize = 20;
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
pub const DEFAULT_BLOCK: Duration = Duration::from_secs(60);

/// 기억하는 출처의 최대 수
const MAX_SOURCES: usize = 4096;

#[derive(Default)]
struct Entry {
    fails: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl Entry {
    /// 아직 기억해 둘 이유가 있는가(차단 중이거나 집계 창 안에 실패가 남음)
    fn active(&self, now: Instant, window: Duration) -> bool {
        self.blocked_until.is_some_and(|until| now < until)
            || self
                .fails
                .back()
                .is_some_and(|last| now.saturating_duration_since(*last) <= window)
    }
}

pub struct SourceFailures {
    max: usize,
    window: Duration,
    block: Duration,
    by: Mutex<HashMap<Option<IpAddr>, Entry>>,
}

impl Default for SourceFailures {
    fn default() -> Self {
        Self::new(DEFAULT_MAX, DEFAULT_WINDOW, DEFAULT_BLOCK)
    }
}

impl SourceFailures {
    pub fn new(max: usize, window: Duration, block: Duration) -> Self {
        Self {
            max: max.max(1),
            window,
            block,
            by: Mutex::new(HashMap::new()),
        }
    }

    /// 이 출처가 막혀 있으면 남은 시간을 돌려준다. 막혀 있지 않으면 아무것도 바꾸지 않는다(막힘이 끝났으면 그 출처의 기록을 비운다).
    pub fn check(&self, source: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        let mut by = self.by.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = by.get_mut(&source) else {
            return Ok(());
        };
        match entry.blocked_until {
            Some(until) if now < until => Err(until - now),
            Some(_) => {
                entry.blocked_until = None;
                entry.fails.clear();
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// 이 출처의 인증 실패 한 번을 센다. 집계 창 안의 실패가 `max`에 이르면 막는다.
    pub fn fail(&self, source: Option<IpAddr>, now: Instant) {
        let mut by = self.by.lock().unwrap_or_else(|e| e.into_inner());
        if !by.contains_key(&source) && by.len() >= MAX_SOURCES {
            self.make_room(&mut by, now);
        }
        let entry = by.entry(source).or_default();
        entry.fails.push_back(now);
        while entry
            .fails
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) > self.window)
        {
            entry.fails.pop_front();
        }
        if entry.fails.len() >= self.max {
            entry.blocked_until = Some(now + self.block);
        }
    }

    /// 새 출처가 들어갈 자리를 만든다: 먼저 더는 기억할 이유가 없는 것(창이 지난 실패뿐이고 차단이 끝난 것)을 모두 버리고, 그래도 가득 차면
    /// **차단 중이 아닌 것(최근 실패만 있는 것) 가운데 가장 오래된 것**을 하나 버린다. 모두 차단 중일 때만 차단이 가장 먼저 끝나는 것을 버린다.
    fn make_room(&self, by: &mut HashMap<Option<IpAddr>, Entry>, now: Instant) {
        by.retain(|_, entry| entry.active(now, self.window));
        if by.len() < MAX_SOURCES {
            return;
        }
        let blocked = |entry: &Entry| entry.blocked_until.is_some_and(|until| now < until);
        let victim = by
            .iter()
            .filter(|(_, entry)| !blocked(entry))
            .min_by_key(|(_, entry)| entry.fails.back().copied())
            .or_else(|| by.iter().min_by_key(|(_, entry)| entry.blocked_until))
            .map(|(source, _)| *source);
        if let Some(victim) = victim {
            by.remove(&victim);
        }
    }

    #[cfg(test)]
    fn sources(&self) -> usize {
        self.by.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests;

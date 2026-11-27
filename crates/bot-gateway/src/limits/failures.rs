//! Per-peer-IP authentication failure budget that blocks abusive callers before hitting `SecretStore`.
//!
//! Sources come strictly from socket peer `ConnectInfo` (never `X-Forwarded-For`), with IPv4-mapped IPv6 normalized
//! to IPv4 and unknown peers sharing one bucket. Successes do not reset the budget. Eviction prefers inactive entries,
//! then the oldest unblocked entry, and finally the earliest-expiring blocked entry.
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const DEFAULT_MAX: usize = 20;
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
pub const DEFAULT_BLOCK: Duration = Duration::from_secs(60);

const MAX_SOURCES: usize = 4096;

#[derive(Default)]
struct Entry {
    fails: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl Entry {
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

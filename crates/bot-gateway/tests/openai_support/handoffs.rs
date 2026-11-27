//! Boundary probe recording SSE chunks handed off to the HTTP response body.
use bot_gateway::http::{Passed, Probe};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
pub struct Handoffs(Arc<Mutex<Vec<(std::time::Instant, Passed, usize)>>>);

impl Handoffs {
    pub fn probe(&self) -> Probe {
        let log = self.0.clone();
        Arc::new(move |kind, bytes| {
            log.lock()
                .unwrap()
                .push((std::time::Instant::now(), kind, bytes));
        })
    }

    /// Count and byte total of `Passed::Data` chunks handed off after `at` (excludes keep-alives and errors).
    pub fn data_after(&self, at: std::time::Instant) -> (usize, usize) {
        let log = self.0.lock().unwrap();
        let after = log
            .iter()
            .filter(|(when, kind, _)| *when > at && *kind == Passed::Data);
        let (count, bytes) = after.fold((0, 0), |(c, b), (_, _, n)| (c + 1, b + n));
        (count, bytes)
    }

    pub fn data_total(&self) -> usize {
        self.data_after(std::time::Instant::now() - Duration::from_secs(3600))
            .0
    }

    pub fn errors(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, kind, _)| *kind == Passed::Error)
            .count()
    }
}

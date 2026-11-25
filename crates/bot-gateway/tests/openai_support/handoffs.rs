//! SSE 조각이 HTTP 본문으로 넘어가는 경계의 관찰 기록.
use bot_gateway::http::{Passed, Probe};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// 스트림 조각이 HTTP 본문으로 넘어가는 경계를 지난 기록(시각, 종류, 바이트 수)
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

    /// `at` 뒤에 본문으로 넘어간 **본문 조각**의 수·바이트(keep-alive 주석과 오류 사건은 세지 않는다)
    pub fn data_after(&self, at: std::time::Instant) -> (usize, usize) {
        let log = self.0.lock().unwrap();
        let after = log
            .iter()
            .filter(|(when, kind, _)| *when > at && *kind == Passed::Data);
        let (count, bytes) = after.fold((0, 0), |(c, b), (_, _, n)| (c + 1, b + n));
        (count, bytes)
    }

    /// 지금까지 넘어간 본문 조각의 수
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

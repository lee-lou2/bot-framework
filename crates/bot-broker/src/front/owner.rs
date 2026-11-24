//! 연결 하나의 소유권 감시: 세션 토큰 회수 신호와 주기적 원장 확인.
//!
//! 원장이 이 시도를 현재로 보는 동안에만 출력을 내보낸다. 확인할 수 없는 동안(원장 오류·시간 초과)에는 출력을 멈추고,
//! 처음 확인하지 못한 시점부터 `FrontConfig.verify_grace`가 지나도록 다시 확인하지 못하면 닫는다. 이 마감 시각은 하나뿐이라
//! 원장 조회도 재시도 사이의 쉼도 모두 그 시각에서 끊긴다(조회가 마감을 넘겨 늦게 성공해도 받아들이지 않는다).
//! 거절(인계·종료·취소)이나 토큰 회수는 곧바로 끝낸다.

use super::{Session, Shared};
use bot_core::{
    id::Attempt,
    port::{Error, Reply},
    state::RunContext,
};
use std::time::Duration;
use tokio::{
    sync::watch,
    time::{Instant, sleep_until, timeout_at},
};

/// 소유권 상태의 변화.
pub(super) enum Change {
    /// 다시 확인됐다(멈췄던 출력을 이어 간다).
    Confirmed,
    /// 확인하지 못했다(출력을 멈춘다).
    Unconfirmed,
    /// 토큰이 회수됐거나 원장이 더는 이 시도를 현재로 보지 않는다.
    Lost,
    /// 기한 안에 확인하지 못했다.
    Expired,
}

/// 토큰이 회수됐는가(회수 신호를 받았거나, 표가 이 세션을 이미 내려놓았다).
pub(super) fn revoked(gone: &watch::Receiver<bool>) -> bool {
    *gone.borrow() || gone.has_changed().is_err()
}

pub(super) struct Ownership<'a> {
    shared: &'a Shared,
    session: &'a Session,
    gone: watch::Receiver<bool>,
    confirmed: bool,
    /// 처음 확인하지 못한 시점(확인된 동안은 `None`). 이 시점 + `verify_grace`가 단일 마감이다.
    since: Option<Instant>,
    next_check: Instant,
    /// 진행 중인 확인과 그 시작 시각. 호출자가 `changed()`를 취소해도(이벤트가 먼저 와서) 확인은 이어서 기다린다:
    /// 요청 하나에는 확인 작업과 상태가 하나뿐이라, 원장이 느리고 스트림이 빨라도 확인이 밀리지 않는다.
    inflight: Option<(Reply<'a, RunContext>, Instant)>,
}

impl<'a> Ownership<'a> {
    pub(super) fn new(
        shared: &'a Shared,
        session: &'a Session,
        gone: watch::Receiver<bool>,
    ) -> Self {
        Self {
            shared,
            session,
            gone,
            confirmed: true,
            since: None,
            next_check: Instant::now() + shared.cfg.revoke_check,
            inflight: None,
        }
    }

    pub(super) fn attempt(&self) -> &Attempt {
        &self.session.attempt
    }

    /// 단일 마감 시각(확인하지 못하는 중일 때).
    fn deadline(&self) -> Option<Instant> {
        self.since.map(|at| at + self.shared.cfg.verify_grace)
    }

    /// 다음 확인까지의 간격: 확인된 동안은 `revoke_check`, 확인하지 못하는 중에는 마감 안에 여러 번 다시 보도록 마감 유예의 4분의 1
    /// (10ms 이상, `revoke_check` 이하)이다.
    fn pace(&self) -> Duration {
        let every = self.shared.cfg.revoke_check;
        if self.confirmed {
            return every;
        }
        (self.shared.cfg.verify_grace / 4).clamp(
            Duration::from_millis(10),
            every.max(Duration::from_millis(10)),
        )
    }

    fn confirm(&mut self) {
        self.confirmed = true;
        self.since = None;
        self.inflight = None;
        self.next_check = Instant::now() + self.shared.cfg.revoke_check;
    }

    /// 확인하지 못했다. 이미 확인하지 못하던 중이면 처음 시점을 그대로 둔다.
    fn lose_sight(&mut self, at: Instant) {
        self.confirmed = false;
        self.since.get_or_insert(at);
    }

    /// 지금 소유권을 새로 확인한다(응답을 내보내기 직전). 확인할 수 없으면 마감까지 간격을 두고 다시 확인하고, 거절이면 `Stale`,
    /// 마감을 넘기면 `Unavailable`이다. 마감은 처음 확인하지 못한 시점(없으면 이 호출을 시작한 시점)부터 `verify_grace`다.
    /// 회수 신호가 오면 곧바로 `Stale`이다.
    pub(super) async fn settle(&mut self) -> Result<(), Error> {
        self.inflight = None; // 새로 확인한다
        let since = self.since.unwrap_or_else(Instant::now);
        let deadline = since + self.shared.cfg.verify_grace;
        loop {
            if revoked(&self.gone) {
                return Err(Error::Stale);
            }
            if Instant::now() >= deadline {
                self.lose_sight(since);
                return Err(Error::Unavailable);
            }
            let check = timeout_at(
                deadline,
                self.shared.parts.model.verify(self.session.attempt.clone()),
            );
            let checked = tokio::select! {
                _ = self.gone.changed() => return Err(Error::Stale),
                checked = check => checked,
            };
            match checked {
                // 마감을 넘겨 도착한 성공은 받아들이지 않는다(실행이 밀려 마감과 조회가 함께 준비되면 조회 결과가 먼저 나올 수 있다).
                Ok(Ok(ctx)) if ctx.agent == self.session.agent && Instant::now() < deadline => {
                    self.confirm();
                    return Ok(());
                }
                Ok(Ok(ctx)) if ctx.agent == self.session.agent => {
                    self.lose_sight(since);
                    return Err(Error::Unavailable);
                }
                Ok(Err(Error::Unavailable)) | Err(_) => {
                    self.lose_sight(since);
                    // 다음 확인까지 쉬되 마감을 넘기지 않는다.
                    let wake = (Instant::now() + self.pace()).min(deadline);
                    tokio::select! {
                        _ = self.gone.changed() => return Err(Error::Stale),
                        () = sleep_until(wake) => {}
                    }
                }
                Ok(_) => return Err(Error::Stale),
            }
        }
    }

    /// 상태가 바뀔 때까지 기다린다. 취소돼도 진행 중인 확인과 다음 확인 시각을 잃지 않는다. 확인하지 못하는 중에는 조회도
    /// 쉼도 단일 마감 시각에서 끊고 `Expired`를 돌려준다.
    pub(super) async fn changed(&mut self) -> Change {
        let every = self.shared.cfg.revoke_check;
        let limit = every.max(Duration::from_secs(1));
        loop {
            if revoked(&self.gone) {
                return Change::Lost;
            }
            if self.deadline().is_some_and(|d| Instant::now() >= d) {
                return Change::Expired;
            }
            if self.inflight.is_none() {
                let wake = self
                    .deadline()
                    .map_or(self.next_check, |d| d.min(self.next_check));
                tokio::select! {
                    _ = self.gone.changed() => return Change::Lost,
                    () = sleep_until(wake) => {}
                }
                if self.deadline().is_some_and(|d| Instant::now() >= d) {
                    return Change::Expired;
                }
                let check: Reply<'a, RunContext> =
                    Box::pin(self.shared.parts.model.verify(self.session.attempt.clone()));
                self.inflight = Some((check, Instant::now()));
            }
            let Some(began) = self.inflight.as_ref().map(|(_, began)| *began) else {
                continue;
            };
            let cap = began + limit;
            let stop = self.deadline().map_or(cap, |d| d.min(cap));
            let Some((check, _)) = self.inflight.as_mut() else {
                continue;
            };
            let checked = tokio::select! {
                _ = self.gone.changed() => return Change::Lost,
                done = timeout_at(stop, &mut *check) => done.ok(),
            };
            self.inflight = None;
            // 이 조회의 마감(`stop`: 이 조회의 상한과 단일 마감 중 이른 쪽)을 넘겨 도착한 성공은 받아들이지 않는다. 시간 초과로 본다
            // (실행이 밀려 마감과 조회가 함께 준비되면 조회 결과가 먼저 나올 수 있다). 거절(Stale·Denied)은 늦어도 그대로 받는다.
            let checked = match checked {
                Some(Ok(_)) if Instant::now() >= stop => None,
                other => other,
            };
            if checked.is_none() && self.deadline().is_none_or(|d| Instant::now() < d) {
                // 시간 초과가 처음이면 여기서 확인하지 못한 시점을 정한다(이미 지난 유예는 아래에서 만료로 이어진다).
                self.lose_sight(began);
            }
            if checked.is_none() && self.deadline().is_some_and(|d| Instant::now() >= d) {
                return Change::Expired;
            }
            match checked {
                Some(Ok(ctx)) if ctx.agent == self.session.agent => {
                    let was = self.confirmed;
                    self.confirm();
                    if !was {
                        return Change::Confirmed;
                    }
                }
                Some(Err(Error::Unavailable)) | None => {
                    let was = self.confirmed;
                    self.lose_sight(began);
                    self.next_check = Instant::now() + self.pace();
                    if was {
                        return Change::Unconfirmed;
                    }
                    // 이미 확인하지 못하던 중이었다: 맨 위에서 마감을 본다.
                }
                Some(_) => return Change::Lost,
            }
        }
    }
}

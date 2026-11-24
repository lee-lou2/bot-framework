//! 대화에서 만든 예약. 신원과 원래 회신 경로는 저장소가 실행에서 복원한다.
use crate::{
    command::JsonPayload,
    cron::Cron,
    id::{
        AgentId, Attempt, BindingId, CallId, PrincipalId, ProfileId, Revision, RunId, ScheduleId,
    },
    ledger::StartCall,
    port::{Error, Reply},
    profile::Scopes,
    state::Conversation,
};
use std::time::SystemTime;

mod close;
mod create;
mod delivery;
mod unsent;
pub mod utc;
pub use close::CloseReason;
pub use create::CreateSeal;
pub use delivery::{DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DELIVERY_CARD_LABEL};
pub use unsent::{DeliveryFence, UNROUTED_NOTICE, UnsentReason};

/// 활성 예약 한도는 같은 프로필·소유자의 모든 에이전트에 걸쳐 적용한다.
pub const MAX_ACTIVE_PER_OWNER: usize = 32;
pub const MAX_INSTRUCTION_BYTES: usize = 8192;
/// 자동 후속 확인이 자체적으로 무한 예약을 만들지 못하게 한다.
pub const MAX_WAKE_DEPTH: u8 = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Timing {
    At(SystemTime),
    Cron { expression: Cron, timezone: String },
}

/// 명시 목적지를 쓰더라도 원래 대화가 기억 가시성과 승인 경로를 결정한다.
#[derive(Clone)]
pub struct DeliveryTarget {
    pub binding: BindingId,
    pub conversation: Conversation,
}

/// 운영자 등록부에서 같은 프로필의 명시 목적지를 확인한다. 수신자 허용 목록은 프로필 멤버가 공유한다.
/// CLI·HTTP 입구는 원래 대화에만 답하며 호출자가 명시 목적지를 거절한다.
pub trait ScheduleRoutes: Send + Sync {
    /// 바인딩·종류·스레드를 보존하고 수신 ID를 실제 전송 경로로 고정한다.
    /// Slack 채널의 정규 ID는 팀ID/채널ID다. 이미 고정된 ID는 같은 경로일 때만 그대로 반환한다.
    /// 생성·발화·발송에서 각자의 현재 등록부를 대조한다. 외부 서비스 조회는 없다.
    /// 등록되지 않은 수신자나 재지정된 경로는 Denied, 잘못된 입력은 Invalid다.
    fn resolve(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error>;
}

#[derive(Clone)]
pub struct ScheduleSpec {
    pub timing: Timing,
    pub instruction: String,
    pub quiet: bool,
    /// None은 원래 대화. Some은 별도로 허가된 같은 프로필의 수신 경로다.
    pub destination: Option<DeliveryTarget>,
}

/// 목록에는 지시문·수신자·원래 대화의 내용이 없다.
#[derive(Clone, Debug)]
pub struct ScheduleSummary {
    pub id: ScheduleId,
    pub timing: Timing,
    pub quiet: bool,
}

/// 직전 커서와 확인할 시각. 다른 스케줄러가 먼저 움직였다면 같은 발생에 수렴한다.
#[derive(Clone)]
pub struct DueSchedule {
    pub profile: ProfileId,
    pub id: ScheduleId,
    pub owner: PrincipalId,
    pub agent: AgentId,
    pub origin: DeliveryTarget,
    pub ceiling: Scopes,
    pub spec: ScheduleSpec,
    pub cursor: SystemTime,
    /// 다음 확인 시각: 저장소가 읽는 순서 `(at, profile, id)`의 첫 칸이다. 같은 쪽의 `UnreadableSchedule`과 섞어 원래 순서로 처리하는 데 쓴다.
    pub at: SystemTime,
}

/// `(다음 확인 시각, 프로필, ID)` 순서에서 예약 한 행의 자리.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuePos {
    pub at: SystemTime,
    pub profile: ProfileId,
    pub id: ScheduleId,
}

/// `after`는 직전 쪽의 마지막 자리이고, `end`는 첫 쪽을 읽을 때 고정한 끝이다.
/// 각 쪽은 현재 확인 대상 중 `after < 자리 <= end`인 행을 읽는다.
/// 새로 확인 대상이 된 예약도 이 범위에 들면 이번 바퀴에 보일 수 있다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DueKey {
    pub after: DuePos,
    pub end: DuePos,
}

/// 저장된 값(시각 정의 · 목적지)을 읽지 못한 예약. 닫는 데 필요한 것(이름표 · 읽은 커서 · 닫을 사유)뿐이다 — 이 행에서 발화는 일어나지 않는다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnreadableSchedule {
    pub profile: ProfileId,
    pub id: ScheduleId,
    pub cursor: SystemTime,
    /// 다음 확인 시각(저장소 정렬 키의 첫 칸) — 정상 예약과 섞인 쪽을 원래 순서로 처리하는 데 쓴다.
    pub at: SystemTime,
    /// 닫을 사유: `InvalidTiming`(cron을 읽을 수 없다) 또는 `InvalidDestination`(목적지 종류를 읽을 수 없다).
    pub reason: CloseReason,
}

pub struct DuePage {
    pub now: SystemTime,
    pub schedules: Vec<DueSchedule>,
    /// 저장된 값을 읽지 못한 행. 한 행의 손상이 쪽 전체를 막지 않게 따로 돌려준다 — `due`는 읽기 전용이라 닫지 않고, 스케줄러가 한 행씩 `reason`으로 닫는다.
    pub unreadable: Vec<UnreadableSchedule>,
    /// 이 바퀴에 읽을 예약이 더 남았다면 다음 쪽을 읽을 자리. 고정한 끝(`end`)까지 읽었으면 None — 한 바퀴를 마쳤으니 다음에는 처음부터 새 바퀴를 시작한다.
    pub next: Option<DueKey>,
}

/// Fire는 접수 한 건과 다음 확인 시각을 한 번에 쓴다. None은 마지막 발생이다.
pub enum NextOccurrence {
    Fire {
        at: SystemTime,
        next: Option<SystemTime>,
        scopes: Scopes,
    },
    /// 첫 발생이 아직 미래인 cron. 커서를 넘겨 발생을 버리지 않는다.
    WaitUntil(SystemTime),
    /// 예약 종료·사유 감사·원래 대화 안내(해당 사유만)를 한 커밋에 기록한다.
    Close(CloseReason),
}

pub struct AdvanceSchedule {
    pub profile: ProfileId,
    pub id: ScheduleId,
    pub expected_cursor: SystemTime,
    /// None은 Close(ProfileInactive)에만 유효하며 저장소가 실제 비활성을 확인한다.
    pub checked_revision: Option<Revision>,
    pub next: NextOccurrence,
}

/// 신뢰 도구의 성공 응답이 요청하는 한 번짜리 후속 확인. 원래 대화에만 회신한다.
pub struct Wake {
    pub at: SystemTime,
    pub instruction: String,
    pub quiet: bool,
}

pub trait UserSchedules: Send + Sync {
    /// 권한/인자/승인과 한도를 검사해 생성·호출 완료·감사를 함께 커밋한다.
    fn create(&self, start: StartCall, spec: ScheduleSpec) -> Reply<'_, ScheduleId>;
    /// 현재 claim과 활성 판본을 대조해 본인 활성 예약만 ID 순서로 반환한다.
    fn list(&self, attempt: Attempt, checked: Revision) -> Reply<'_, Vec<ScheduleSummary>>;
    /// 소유자의 취소·호출 완료·감사 한 커밋. 이미 접수한 실행은 원래 run으로 추적한다.
    fn cancel(&self, start: StartCall, id: ScheduleId) -> Reply<'_, ()>;
    /// Started 도구의 결과와 (run, call)에 유일한 후속 예약을 한 커밋에 보존한다.
    fn finish_with_wake(
        &self,
        attempt: Attempt,
        call: CallId,
        checked: Revision,
        result: JsonPayload,
        wake: Wake,
    ) -> Reply<'_, ScheduleId>;
    /// DB 시각까지 확인할 예약을 (다음 확인 시각, 프로필, ID) 순으로 `limit`개까지 읽는다. `after`가 있으면 그 자리 **다음**부터다.
    /// 호출자는 돌려받은 `DuePage::next`를 다음 호출의 `after`로 돌려주고, None이 오면(한 바퀴를 마침) 다음에는 처음(`None`)부터 읽는다 —
    /// 처리하지 못해 남겨 둔 예약이 첫 쪽을 계속 채워 뒤의 예약을 막지 않게 한다. 바퀴의 끝은 바퀴를 시작할 때(`after`가 없을 때) 고정한다:
    /// 계속 새 예약이 들어와도 바퀴는 끝나고, 앞쪽에 남겨 둔 예약이 다음 바퀴에 다시 보인다.
    fn due(&self, after: Option<DueKey>, limit: u32) -> Reply<'_, DuePage>;
    /// 활성 판본·예약 상태·커서를 잠금 안에서 대조한다. 같은 발생은 같은 run이다.
    fn advance(&self, command: AdvanceSchedule) -> Reply<'_, Option<RunId>>;
}

/// 예약 성공의 최종 텍스트에만 적용한다. 승인·오류·첨부 회신은 별도로 처리한다.
pub fn quiet_completion(quiet: bool, final_text: &str) -> bool {
    quiet && final_text.trim() == "<조용히>"
}

/// 다른 수신자에게 보내기 전 운영 경로가 동결한 회신. 내용은 진단 출력에서 제외한다.
pub struct DeliveryDraft {
    /// 실제 게이트웨이의 수신자 매핑까지 고정한 호스트 활성 집합.
    pub activation: Revision,
    pub target: DeliveryTarget,
    /// 순서가 있는 최종 게이트웨이 본문. 첨부가 있으면 승인자가 그 내용을 확인해야 한다.
    pub answers: Vec<JsonPayload>,
}

/// 워커와 결정 브로커가 공유하는 PG 포트. 초안과 승인은 원래 대화의 소유자에게만 보인다.
pub trait ScheduledReplies: Send + Sync {
    /// 마지막 claim에 이미 커밋된 등록 또는 미발송 영수증을 돌려준다. 미종결이면 None이다.
    /// 저장소는 한 시점에서 프로필·run·세대·claim_epoch, 현재 활성 집합과 종결 당시 집합을 대조한다.
    /// 초안의 본문과 목적지는 반환하지 않는다.
    fn receipt(&self, _attempt: Attempt) -> Reply<'_, Option<JsonPayload>> {
        Box::pin(async { Err(crate::port::Error::Unavailable) })
    }

    /// Some은 최종 결과·답을 한 번 동결한다. None은 재개 시 기존 초안만 읽는다.
    /// 일반 실행, 원래 대화로 보내는 답, 첨부 없는 quiet 성공이면 None이다.
    fn draft(
        &self,
        attempt: Attempt,
        completion: Option<crate::command::Finish>,
    ) -> Reply<'_, Option<DeliveryDraft>>;
    /// 현재 집합·유효 claim에서 이 발생의 고정 목적지만 읽는다. 초안이 옛 집합에서 동결됐어도 내용을 노출하지 않고 철회를 판별한다.
    fn target(&self, _attempt: Attempt) -> Reply<'_, Option<DeliveryTarget>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    /// 현재 집합·fence·보관한 목적지를 대조한 뒤 발생 하나만 종결한다.
    /// 같은 트랜잭션에서 해당 전송의 소유자 거절·철회가 확인되면 요청 사유가 Unrouted여도 Rejected로 기록한다.
    /// Rejected에는 안내가 없다. 재전달에는 실제로 커밋한 사유의 영수증을 반환한다.
    /// Unrouted는 원래 대화에 고정 안내 한 건, 기존 승인 무효화, 미발송 영수증과 실행 완료를 함께 커밋한다.
    /// 동일 종결의 재전달은 같은 영수증이다. 목적지 발송·승인 소비 또는 이미 등록된 답의 취소는 허용하지 않는다.
    fn finish_unsent(
        &self,
        _fence: DeliveryFence,
        _target: DeliveryTarget,
        _reason: UnsentReason,
    ) -> Reply<'_, JsonPayload> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    /// 현재 사람 소유자·권한·활성 집합·저장된 초안의 sha256()을 확인한다.
    /// DELIVER_CALL의 승인 소비, 명시 목적지 outbox, run 완료를 한 커밋에 쓴다.
    fn enqueue(&self, approval: StartCall) -> Reply<'_, ()>;
}

/// 운영 호스트의 예약 회신 전용 입구. Done에는 저장소가 종결한 등록 또는 미발송 영수증이 담긴다.
/// 일시 확인 실패는 Unavailable이며 새 발송·안내·승인 소비 없이 같은 발생을 재시도한다.
pub trait ScheduleDelivery: Send + Sync {
    fn release(&self, attempt: Attempt) -> Reply<'_, crate::command::CallResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_quiet_success_suppresses_the_scheduled_reply() {
        assert!(quiet_completion(true, " <조용히>\n"));
        for text in ["", "변경 없음", "확인 필요 <조용히>"] {
            assert!(!quiet_completion(true, text));
        }
        assert!(!quiet_completion(false, "<조용히>"));
    }
}

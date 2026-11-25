//! 구현자는 각 경계에서 인증된 상태를 다시 읽고 판정한다.
use crate::{
    command::*,
    id::*,
    intake::{RequestKey, RequestState, RunProgress},
    state::*,
};
use std::{future::Future, path::Path, pin::Pin};

#[derive(Debug)]
pub enum Error {
    Invalid,
    Denied,
    Conflict,
    Stale,
    Missing,
    Unavailable,
}

pub type Reply<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

pub trait ProfileSource: Send + Sync {
    fn validate<'a>(&'a self, root: &'a Path) -> Reply<'a, Revision>;
}

pub trait RunStore: Send + Sync {
    fn accept(&self, command: Accept) -> Reply<'_, RunId>;
    /// 어댑터가 인증한 원천·넘김 사슬(`AcceptExtras`)을 접수와 한 커밋에 남긴다. 덧붙일 것이 없으면 `accept`다.
    /// 기본 구현은 덧붙인 것을 저장하지 않고 일반 접수로 처리한다(덧붙임은 증거와 후보를 늘릴 뿐이라 빠져도 더 허용하는 일은 없다).
    fn accept_with(&self, command: Accept, _extras: AcceptExtras) -> Reply<'_, RunId> {
        self.accept(command)
    }
    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>>;
    /// run 트리의 현재 진행 요약(하위 에이전트·실행 중인 도구·승인 대기 여부). 기본 구현은 `None`.
    fn progress(&self, _profile: ProfileId, _run: RunId) -> Reply<'_, Option<RunProgress>> {
        Box::pin(async { Ok(None) })
    }
    /// 준비된 작업을 점유할 때 같은 트랜잭션에 현재 호스트 활성 집합을 기록한다.
    /// 리스 만료 인계 시 이전·현재 집합이 모두 있고 다를 때만 시도 횟수를 유지한다.
    /// 같은 집합 또는 이전 기록 부재는 한 번 차감한다. 집합 조회 오류는 쓰기 없이 실패한다.
    /// 인계는 세대를 올리고 이전 승인을 무효화한다. 자발적 승인 대기에서 재개하면 세대를 유지한다.
    fn claim(&self) -> Reply<'_, Option<RunContext>>;
    /// 정상 종료 유예가 지난 뒤 호스트가 이 claim의 자식 프로세스 종료와 세션 회수를 확인하고 호출한다.
    /// 미취소 run의 유효한 리스·세대·claim_epoch 소유자만 ready로 돌린다. 시도 횟수는 유지한다.
    /// 세대 증가와 옛 승인 무효화는 한 커밋이며 바로 다음 점유는 세대를 보존한다.
    /// 이미 반납했거나 소유권을 잃은 시도는 Stale이며 이후 소유자에게 영향을 주지 않는다.
    fn release_for_shutdown(&self, _attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    /// 현재 소유자·세대·만료를 대조하고 리스를 늘린다. 옛 세대·만료 뒤는 Stale.
    fn renew(&self, attempt: Attempt) -> Reply<'_, Beat>;
    /// 모델 step·도구 결과 체크포인트. 같은 key는 멱등.
    fn checkpoint(&self, attempt: Attempt, step: Step) -> Reply<'_, ()> {
        self.checkpoint_batch(attempt, vec![step])
    }
    /// 현재 claim으로 차례의 레코드·도구·그룹을 한 커밋에 저장한다.
    /// 기존 key의 내용이 하나라도 다르면 전체를 롤백한다. 같은 내용의 재전송은 멱등이다.
    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()>;
    /// 결과·답(outbox)·부모 깨우기를 한 커밋으로 기록한다.
    fn finish(&self, attempt: Attempt, done: Finish) -> Reply<'_, ()>;
    /// `finish`와 같되 실행 계량(잘림·사용량)도 같은 커밋에 기록한다. 계량을 저장하지 않는 구현은 계량을 버리고 `finish`로 끝낸다(기본 구현).
    fn finish_with(&self, attempt: Attempt, done: Finish, _outcome: RunOutcome) -> Reply<'_, ()> {
        self.finish(attempt, done)
    }
    /// 일시 오류는 백오프 재시도, 영구 오류·시도 한도 초과는 dead letter.
    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry>;
    /// 같은 대화의 최근 차례(오래된 순). 요청자에게 보이는 것만.
    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>>;
}

/// 운영자 조치. 모두 감사에 남는다.
pub trait RunAdmin: Send + Sync {
    /// dead letter 재개. 같은 논리 run·효과 키를 유지하고 시도 횟수만 비운다.
    fn resume(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()>;
    /// 취소 요청. 자식도 함께 요청하고, 실행 중이면 다음 외부 효과 전에 멈춘다.
    fn cancel(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()>;
}

/// 조회는 모델 전달 전 private_memory_read를 요청 트리에 원자적으로 기록한다.
pub trait MemoryStore: Send + Sync {
    fn search(&self, attempt: Attempt, query: String) -> Reply<'_, Vec<MemoryHit>>;
    /// 현재 claim과 search의 가시성 규칙으로 ID 한 건만 조회한다. private 반환 전 요청 트리 플래그를 기록한다.
    fn preview(&self, attempt: Attempt, id: MemoryId) -> Reply<'_, MemoryPreview>;
    fn change(&self, attempt: Attempt, change: MemoryChange) -> Reply<'_, MemoryId>;
    /// 승인과 현재 소유자·본문 지문을 대조하고 승인 소비·공유·호출 완료·감사를 한 번에 커밋한다.
    /// 지문 불일치는 Conflict다. 완료한 호출의 재시도는 기억의 가시성을 다시 바꾸지 않는다.
    fn share(&self, start: crate::ledger::StartCall, id: MemoryId) -> Reply<'_, MemoryId>;
    fn seed(&self, seed: MemorySeed) -> Reply<'_, ()>;
}

/// 호스트가 바인딩된 profile로 얻은 핸들. 호출에 전체 경로나 프로필을 받지 않는다.
pub trait ProfileSecrets: Send + Sync {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue>;
}

/// 진단 출력을 위한 Debug/Display 미제공.
pub struct SecretValue(pub Vec<u8>);

pub trait SecretStore: Send + Sync {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>>;
}

pub trait Broker: Send + Sync {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult>;
    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()>;
}

pub trait Gateway: Send + Sync {
    fn deliver(&self, run: RunId, profile: ProfileId) -> Reply<'_, ()>;
}

pub trait ScheduleStore: Send + Sync {
    /// 원래 예정 시각과 발생 접수·커서 이동을 함께 커밋한다. 같은 (profile, agent, trigger, 시각)은 기존 run.
    fn accept_occurrence(&self, occurrence: Occurrence, command: Accept) -> Reply<'_, RunId>;
    fn snapshot(&self) -> Reply<'_, ScheduleSnapshot>;
}

pub trait DelegationStore: Send + Sync {
    /// 현 claim·판본과 자식의 실제 대상·지시·부여 범위를 대조해 완료 결과만 읽는다.
    /// 아직 완료하지 않았거나 자식이 없으면 None이다. 새 위임이나 부모 대기를 만들지 않는다.
    fn result(&self, command: Delegate) -> Reply<'_, Option<JsonPayload>>;
    fn delegate(&self, command: Delegate) -> Reply<'_, RunId>;
}

pub trait EffectJournal: Send + Sync {
    /// 새 단계 또는 운영자의 NotExecuted 판정 뒤 첫 점유에서만 send=true를 반환한다.
    /// 판정 소비와 새 점유는 한 커밋이다. 이후 조회는 소비된 판정을 반환하지 않는다.
    fn request(&self, request: EffectRequest) -> Reply<'_, EffectPermit>;
    /// 호출 채널에 묶인 도구와 현재 attempt·call을 검증한 뒤 step과 request와 같은 인자 지문으로 조회한다.
    /// 유효한 호출에 단계가 없으면 None, 같은 단계의 지문이 다르면 Conflict다.
    /// 조회는 단계·영수증·감사를 쓰거나 전송을 허가하지 않는다.
    /// 보류 단계의 미소비 운영자 판정은 resolution에 담는다. 반복 조회해도 소비되지 않는다.
    fn lookup(&self, _request: EffectRequest, _tool: ToolId) -> Reply<'_, Option<EffectStatus>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn status(&self, attempt: Attempt, call: CallId, effect: EffectId) -> Reply<'_, EffectStatus>;
    fn unresolved(&self, attempt: Attempt, call: CallId) -> Reply<'_, Vec<EffectStatus>>;
    fn record(&self, attempt: Attempt, call: CallId, status: EffectStatus) -> Reply<'_, ()>;
}

pub struct EffectPermit {
    pub status: EffectStatus,
    pub send: bool,
}

pub trait AuditLog: Send + Sync {
    /// 상태 변경과 함께 쓰는 감사는 저장소가 같은 트랜잭션에서 직접 기록한다. 이것은 거부 판정 같은 단독 기록용이다.
    fn append(&self, entry: AuditEntry) -> Reply<'_, u64>;
    /// 신뢰 호스트의 체인 전체를 검증한다. 사용자용 진입점은 운영자 권한을 확인한다.
    fn verify(&self) -> Reply<'_, ChainReport>;
}

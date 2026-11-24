//! 모델 실행 포트와 구현. 워커가 `ModelRunner::step`으로 부른다.
//!
//! 실행기는 두 방식(`RunnerKind`)이다. 워커는 `ModelRunner::kind()`로 도구 중계 책임을 가른다.
//!
//! - **Step**(API 모델·데모): 워커가 맥락(`context::assemble`)과 이전 교환(`Exchange`)으로 `ModelRequest`를 만들어 `step`을 **차례마다** 부른다.
//!   돌아온 `ModelStep::Calls`는 워커가 `Broker::call`로 중계하고(승인 대기·보류도 워커 몫), 모델 차례와 호출 결과를 체크포인트에 적은 뒤
//!   다음 차례를 부른다. `ModelStep::Final`이면 답을 `Finish`로 올린다. 이 방식의 실행기는 도구를 직접 부르지 않는다.
//! - **Loop**(CLI: Claude Code·Pi): CLI가 자기 도구 순환을 가지므로 워커는 차례를 제어하지 않는다. `step`은 한 번만 부르고 항상 `Final`(또는 오류)을 받는다.
//!   모델 요청과 도구는 run별 브로커 프런트(`ModelRequest::relay`: HTTP 주소 + 이 시도에 발급한 세션 토큰)로 CLI가 브로커에 직접 낸다.
//!   실행기는 그 주소·토큰으로 CLI의 MCP 설정 파일과 환경을 만들어 줄 뿐이다. 프런트 없이는 CLI가 모델에 닿지 못하므로 채널이 없으면 실행하지 않는다. 승인 대기·보류로 저장소가 run을 멈추면(리스를 놓는다)
//!   워커의 `renew`가 `Stale`이 되므로 워커는 `cancel`로 CLI를 끝내야 한다.
//!
//! 모델 샌드박스는 `bot_core::sandbox`의 포트(`Sandbox<ModelSpec>`)다. 모델 CLI는 **VM**(macOS는 Apple container, `--internal` 망)에서 돌고
//! 외부와 통신하지 못하며 브로커 프런트 한 곳에만 닿는다(`container.rs`). 도구 서버의 격리(srt)는 별개이고 브로커 쪽이다. 여기에는 VM 어댑터와 가짜(`FakeSandbox`)가 있다.
//! 오류는 고정 어휘 코드만 싣는다(로그·감사에 자유 문장·stderr를 남기지 않는다).

mod claude_code;
mod cli;
pub mod container;
mod demo;
mod driver;
mod exchange;
mod pi;
mod prompt;
mod relay;
mod sandbox;
pub mod session;

pub use claude_code::ClaudeCodeRunner;
pub use cli::{CliConfig, CliModel};
pub use demo::DemoModel;
pub use driver::{Event, Output, Parser, classify, is_transient};
pub use exchange::{CallEnd, DoneCall, Exchange};
pub use pi::PiRunner;
pub use relay::{RelayChannel, RelayGrant, TOKEN_ENV};
pub use sandbox::{FakeSandbox, Script};
pub use session::{PortFuture, Session, SessionError, SessionPort, Slot, ToolOutput};

use bot_core::{
    command::{Failure, JsonPayload},
    id::{Attempt, CallId, PrincipalId, RunId, ToolId},
    profile::Builtin,
    state::Risk,
};
use std::{collections::BTreeSet, fmt, future::Future, pin::Pin, time::Duration};
use tokio::sync::watch;

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<ModelStep, ModelError>> + Send + 'a>>;

/// 실행기가 도구 순환을 누가 도는가.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerKind {
    /// 워커가 차례마다 `step`을 부르고 `ModelStep::Calls`를 `Broker::call`로 중계한다.
    Step,
    /// 실행기(CLI)가 자기 루프를 가진다. `step`은 한 번, 결과는 항상 `Final`. 도구는 `ModelRequest::relay`로 나간다.
    Loop,
}

/// 모델 실행 포트.
pub trait ModelRunner: Send + Sync {
    fn kind(&self) -> RunnerKind;
    /// Step 방식은 모델 한 차례를, Loop 방식은 CLI 한 번 실행 전체를 돌려준다.
    fn step(&self, request: ModelRequest) -> StepFuture<'_>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

/// 대화 한 줄. `speaker`는 사용자 줄의 발화자(에이전트 줄은 None)다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub role: Role,
    pub speaker: Option<PrincipalId>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    /// 스킬이 선언한 도구. 호출은 `Broker::call`로 간다.
    Skill,
    /// 직계 하위 에이전트에게 맡기는 위임. 호출은 `DelegationStore::delegate`로 간다(워커가 가로챈다).
    Delegate,
}

/// 모델에게 보여 주는 도구 하나. 권한 판정은 브로커가 호출마다 다시 하므로 이 목록은 안내일 뿐 경계가 아니다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSpec {
    pub id: ToolId,
    pub kind: ToolKind,
    pub class: Risk,
    pub description: String,
    /// 입력 JSON 스키마
    pub input: JsonPayload,
}

/// 한 차례에 주는 시간. `idle`은 출력이 없는 채로 기다리는 한도다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub total: Duration,
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(600),
            idle: Duration::from_secs(120),
        }
    }
}

/// 한 차례의 입력. 취소는 `true`가 되는 순간 실행기가 프로세스를 끝내고 `Cancelled`를 돌려준다(송신 쪽이 사라지면 취소하지 않는다).
#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub attempt: Attempt,
    /// 프로필의 모델 별칭
    pub model: String,
    /// 조립한 지침(규칙·스킬·지식·기억)
    pub system: String,
    /// 이전 차례와 이번 입력(오래된 순, 마지막이 이번 입력)
    pub conversation: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// 이번 run에서 이미 한 모델 차례와 호출 결과(체크포인트에서 복원)
    pub exchanges: Vec<Exchange>,
    pub limits: Limits,
    pub cancel: watch::Receiver<bool>,
    /// 에이전트가 AGENT.md에 명시한 읽기 전용 내장 도구(상속 없음, 기본 비어 있다). Step 방식은 쓰지 않는다.
    pub builtin: BTreeSet<Builtin>,
    /// Loop 방식 실행기의 브로커 프런트(모델·도구 중계). 없으면 CLI가 모델에 닿지 못하므로 실행하지 않는다(Step 방식은 쓰지 않는다).
    pub relay: Option<RelayChannel>,
    /// 세션 복구(승인·위임 재개)를 호스트가 대 주는 포트. 없으면 세션을 남기지 않고 매번 처음부터 한다(Claude Code 실행기만 쓴다).
    pub session: Option<Session>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::Add for Usage {
    type Output = Usage;

    fn add(self, other: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
        }
    }
}

/// 모델이 요청한 호출 하나. `call`은 재시도·재개에도 같아야 하므로 워커는 이 값을 체크포인트에 적은 뒤에 호출한다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingCall {
    pub call: CallId,
    pub tool: ToolId,
    pub args: JsonPayload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStep {
    /// 최종 답
    Final { text: String, usage: Usage },
    /// 도구를 부르고 결과를 다음 차례에 넘겨야 한다. `text`는 호출과 함께 나온 모델의 말(없으면 빈 글).
    Calls {
        text: String,
        calls: Vec<PendingCall>,
        usage: Usage,
    },
}

/// 실행 실패. 코드는 고정 어휘다: `cancelled` `timeout` `idle_timeout` `sandbox_failed` `prepare_failed` `unknown_model`
/// `invalid_relay` `no_result` `empty_answer` `rate_limited` `overloaded` `network` `model_error`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    /// 취소 신호를 받아 멈췄다.
    Cancelled,
    /// 전체 시간(`Limits::total`)을 넘겼다.
    Timeout,
    /// 세션을 이으려던 슬롯이 다시 승인·자식 결과를 기다려야 한다: run은 다시 멈춘다(저장소가 이미 멈췄다).
    Parked,
    /// 이 claim이 더는 소유자가 아니다: 아무것도 쓰지 않고 물러난다.
    Lost,
    Failed {
        code: &'static str,
        transient: bool,
    },
}

impl ModelError {
    pub fn code(&self) -> &'static str {
        match self {
            ModelError::Cancelled => "cancelled",
            ModelError::Timeout => "timeout",
            ModelError::Parked => "parked",
            ModelError::Lost => "lost",
            ModelError::Failed { code, .. } => code,
        }
    }

    /// 잠시 뒤 다시 하면 풀릴 만한 실패인가.
    pub fn transient(&self) -> bool {
        match self {
            ModelError::Cancelled | ModelError::Parked | ModelError::Lost => false,
            ModelError::Timeout => true,
            ModelError::Failed { transient, .. } => *transient,
        }
    }

    /// `RunStore::fail`에 넘길 값. 취소는 `Permanent("cancelled")`다(저장소가 취소 요청을 확인하고 취소로 끝낸다).
    pub fn failure(&self) -> Failure {
        if self.transient() {
            Failure::Transient(self.code())
        } else {
            Failure::Permanent(self.code())
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model step failed: {}", self.code())
    }
}

impl std::error::Error for ModelError {}

/// 한 run 안에서 안정적인 호출 ID: `<run>:<차례>:<순번>`. 같은 입력을 결정적으로 다시 만드는 모델은 재개해도 같은 ID를 낸다.
pub fn call_id(run: &RunId, step: usize, index: usize) -> CallId {
    CallId(format!("{}:{step}:{index}", run.0))
}

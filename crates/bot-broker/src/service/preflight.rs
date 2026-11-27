//! 실행 전에 확정되는 거절의 사전 판정(CLI 프런트와 단계형 워커의 공통 거절 표식).
//!
//! 도구 선언과 기본 시간대는 실행이 고정한 프로필 판본에서 읽는다. 한 번 예약의 시각은 새 호출과 같은 범위로 확인한다.
//! - 에이전트가 고정 판본에서 볼 수 있는 도구가 아니다 → `UnknownTool`.
//! - 그 도구 선언의 입력 검사(봉인)가 `Invalid`로 실패한다 → `Invalid`.
//!
//! 판본을 확인할 수 없거나 저장소·세대 문제로 답하지 못하면 **오류**다(확정이 아니고 "거절 아님"도 아니다). 저장소 오류가 `Missing`·`Invalid`로
//! 섞여 나오는 일반 `Broker::call`의 결과로는 구분할 수 없어서, 호출한 쪽이 이 판정을 따로 묻는다. 내장 도구의 입력 검사는 각 처리기와 공유한다.

use super::{
    BrokerService, schedule,
    share::{self, SHARE_TOOL},
};
use crate::resolve::find_tool;
use bot_core::{
    cli::CliRefusal,
    command::{JsonPayload, ToolCall},
    id::{Attempt, ToolId},
    ledger::CallLedger,
    port::{Error, Reply},
};

/// 모델이 부르는 내장 위임 도구의 이름(MCP에서 보이는 그대로). 카탈로그에 없고 프런트가 입력·인가를 직접 판정한다.
pub use bot_core::builtin::DELEGATE_TOOL;

pub(crate) fn expected(error: &Error) -> Option<CliRefusal> {
    Some(match error {
        Error::Missing => CliRefusal::UnknownTool,
        Error::Invalid => CliRefusal::Invalid,
        Error::Denied => CliRefusal::Denied,
        Error::Conflict => CliRefusal::Conflict,
        Error::Stale => CliRefusal::Stale,
        Error::RateLimited => CliRefusal::RateLimited,
        Error::Unavailable => return None,
    })
}

/// 이미 실행 중이거나 원장에 결론이 있으면 그 기록이 우선한다. 입력·도구 없음은 고정 판본의 사전 판정과 일치해야 한다.
pub(crate) async fn confirm(
    ledger: &dyn CallLedger,
    judge: Option<&dyn Preflight>,
    call: ToolCall,
    expected: CliRefusal,
) -> Result<bool, Error> {
    let known = ledger.lookup(call.attempt.clone(), call.call).await?;
    if !CliRefusal::can_replay(known.as_ref()) {
        return Ok(false);
    }
    match expected {
        CliRefusal::UnknownTool | CliRefusal::Invalid => {
            let Some(judge) = judge else { return Ok(false) };
            Ok(judge.refusal(call.attempt, call.tool, call.args).await? == Some(expected))
        }
        CliRefusal::Denied | CliRefusal::Conflict | CliRefusal::Stale | CliRefusal::RateLimited => {
            Ok(true)
        }
    }
}

pub trait Preflight: Send + Sync {
    /// 이 호출이 실행 전에 확정적으로 거절되는가. 현재 claim을 쥔 시도만 묻는다(아니면 `Stale`).
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>>;
}

impl Preflight for BrokerService {
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>> {
        Box::pin(async move {
            let ctx = self.parts.ledger.context(attempt.clone()).await?;
            // 호스트 전용 ID는 모델이 부를 수 없다: 늘 `unknown_tool`이다(존재도 알리지 않는다).
            if tool.0 == schedule::DELIVER_TOOL {
                return Ok(Some(CliRefusal::UnknownTool));
            }
            // 위임은 카탈로그에 없다: 프런트가 입력·인가를 직접 판정하므로 여기서는 확정하지 않는다.
            if tool.0 == DELEGATE_TOOL {
                return Ok(None);
            }
            // 고정 판본을 확인할 수 없으면 확정하지 않는다.
            let pinned = self
                .parts
                .catalog
                .lookup(&attempt.profile, &ctx.work.revision)
                .ok_or(Error::Unavailable)?;
            // 내장 도구는 카탈로그에 없다: 인자 검사(호출 기록 전의 같은 검사)만 확정할 수 있다.
            if tool.0 == SHARE_TOOL {
                return Ok(share::judged(&args));
            }
            if schedule::is_builtin(&tool.0) {
                return Ok(schedule::judged(
                    &tool.0,
                    &args,
                    &pinned.timezone,
                    self.parts.clock.now(),
                ));
            }
            if let Some(spec) = bot_core::builtin::manage_tool(&tool.0) {
                let decl = spec.declaration();
                return match self.parts.sealer.seal(&decl, &args) {
                    Ok(_) => Ok(None),
                    Err(Error::Invalid) => Ok(Some(CliRefusal::Invalid)),
                    Err(error) => Err(error),
                };
            }
            let Some(declared) = find_tool(&pinned, &ctx.agent, &tool) else {
                return Ok(Some(CliRefusal::UnknownTool));
            };
            match self.parts.sealer.seal(declared, &args) {
                Ok(_) => Ok(None),
                Err(Error::Invalid) => Ok(Some(CliRefusal::Invalid)),
                Err(error) => Err(error),
            }
        })
    }
}

//! 실행 전에 확정되는 거절의 사전 판정(CLI 재개의 `CliRefusal` 표식을 남길 근거).
//!
//! 판정은 **실행이 고정한 프로필 판본만** 쓴다(지금 활성 판본·정책·원장 기록·시각에 기대지 않는다). 그래서 같은 계획 위치는 몇 번 다시
//! 물어도 같은 답이고, 재개가 모델이 본 것과 같은 단어로 이 자리를 마무리할 수 있다.
//! - 에이전트가 고정 판본에서 볼 수 있는 도구가 아니다 → `UnknownTool`.
//! - 그 도구 선언의 입력 검사(봉인)가 `Invalid`로 실패한다 → `Invalid`.
//!
//! 판본을 확인할 수 없거나 저장소·세대 문제로 답하지 못하면 **오류**다(확정이 아니고 "거절 아님"도 아니다). 저장소 오류가 `Missing`·`Invalid`로
//! 섞여 나오는 일반 `Broker::call`의 결과로는 구분할 수 없어서, 호출한 쪽이 이 판정을 따로 묻는다. 호스트 내장 도구는 대상이 아니다.

use super::{
    BrokerService, schedule,
    share::{self, SHARE_TOOL},
};
use crate::resolve::find_tool;
use bot_core::{
    cli::CliRefusal,
    command::JsonPayload,
    id::{Attempt, ToolId},
    port::{Error, Reply},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

tokio::task_local! {
    /// 한 번의 `Broker::call`(`noting_limit`이 감싼 호출)만의 표지: 속도·동시성 한도가 그 호출을 거절하면 켜진다.
    static LIMITED: Arc<AtomicBool>;
}

/// `call`(한 번의 `Broker::call`)을 돌리고, 그 안에서 속도·동시성 **한도**가 이 호출을 거절했는지 함께 돌려준다. 거절의 원인은 이 호출만의 표지에만 적힌다 —
/// 호출 위치나 요청이 같아도 다른 호출의 거절·읽히지 않은 표시가 섞이지 않는다(공유 목록이 없다). 오류는 늘 `Unavailable`이라 저장소 장애 같은 다른 이유의
/// `Unavailable`과는 이 표지로만 구별한다. 호출이 일을 다른 태스크로 넘기면 그 안의 거절은 보이지 않는다(한도로 확정하지 않는 쪽이다).
pub(crate) async fn noting_limit<T>(call: impl Future<Output = T>) -> (T, bool) {
    let flag = Arc::new(AtomicBool::new(false));
    let output = LIMITED.scope(flag.clone(), call).await;
    (output, flag.load(Ordering::SeqCst))
}

/// 모델이 부르는 내장 위임 도구의 이름(MCP에서 보이는 그대로). 카탈로그에 없고 프런트가 입력·인가를 직접 판정한다.
pub const DELEGATE_TOOL: &str = "delegate";

pub trait Preflight: Send + Sync {
    /// 이 호출이 실행 전에 확정적으로 거절되는가. 현재 claim을 쥔 시도만 묻는다(아니면 `Stale`).
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>>;
}

impl BrokerService {
    /// 속도·동시성 한도가 이 호출을 거절했다: 오류는 늘 `Unavailable`이다(모델에 가는 단어는 그대로). 이 호출을 감싼 `noting_limit`의 표지를 켜
    /// 호출한 쪽이 그 `Unavailable`이 **한도 때문인지** 알 수 있게 한다(감싸지 않은 호출은 아무도 묻지 않는다).
    pub(super) fn limited() -> Error {
        let _ = LIMITED.try_with(|flag| flag.store(true, Ordering::SeqCst));
        Error::Unavailable
    }
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

//! 프런트가 모델에 돌려주는 거절 단어의 확정 기록. 검증된 CLI 호출 위치(`CallId::cli`)에서 실행 전에 거절한 호출은 **모델이 본 그 단어**를 run 체크포인트의
//! 표식(`bot_core::cli::CliRefusal`)으로 먼저 남기고, 남긴 **뒤에만** 단어를 돌려준다. 재전달·재개는 호출을 다시 판정하지 않고 같은 단어를 재생한다
//! (정상 거절은 시도 횟수를 쓰지 않는다). 표식은 판정 사유(감사 낱말)를 싣지 않는다.
//!
//! - 오류 → 단어: `Missing`=`unknown_tool`, `Invalid`=`invalid`, `Denied`=`denied`, `Conflict`=`conflict`, `Stale`=`stale`, 속도·동시성 한도=`unavailable`.
//!   `Missing`·`Invalid`는 저장소 오류가 섞여 나올 수 있어 브로커의 사전 판정이 가리고, 한도는 **그 호출의 결과와 함께** 한도가 거절했다고 확인된
//!   호출만 확정이다(`service::preflight::noting_limit`: 호출마다 따로인 표지라 다른 요청이나 읽히지 않은 표시와 섞이지 않는다). 저장소·소유권 장애의
//!   `Unavailable`과 `Stale`(claim을 잃으면 표식을 쓸 수 없다)은 거절로 봉인하지 않는다.
//! - 원장이 이미 이 호출을 시작·보류·완료·거절로 말하면 표식이 아니라 원장을 따른다(`CallBook::confirm`). 계획과 대조하지 않은 위치는 표식이 없다.
//! - 표식을 쓰지 못했으면 거절 단어를 돌려주지 않고 그 쓰기 실패의 단어(`unavailable`·`stale`)를 돌려준다. 표식이 없는 그 위치는 재개가 다시 판정한다.

use super::{Session, Shared};
use bot_core::{
    cli::{CliRefusal, error_word},
    command::JsonPayload,
    id::{CallId, ToolId},
    port::Error,
};

/// 거절된 호출에 모델이 볼 단어. 확정이면 표식을 남긴 뒤에만 그 단어이고, 확정이 아니거나 확인·기록하지 못했으면 오류의 단어 그대로다.
pub(super) async fn word(
    shared: &Shared,
    session: &Session,
    (call, tool, args): (CallId, ToolId, JsonPayload),
    error: &Error,
    limited: bool,
) -> &'static str {
    let Some(refusal) = crate::service::preflight::expected(error, limited) else {
        return error_word(error);
    };
    let attempt = session.attempt.clone();
    match shared
        .parts
        .calls
        .confirm(attempt.clone(), call.clone(), tool, args, refusal)
        .await
    {
        Ok(true) => seal(shared, session, call, refusal).await,
        Ok(false) => error_word(error),
        Err(failure) => error_word(&failure),
    }
}

/// 이미 확정한 거절을 표식으로 남기고 그 단어를 돌려준다(쓰지 못하면 쓰기 실패의 단어).
pub(super) async fn seal(
    shared: &Shared,
    session: &Session,
    call: CallId,
    refusal: CliRefusal,
) -> &'static str {
    match shared
        .parts
        .turns
        .refused(session.attempt.clone(), call, refusal)
        .await
    {
        Ok(()) => refusal.word(),
        Err(failure) => error_word(&failure),
    }
}

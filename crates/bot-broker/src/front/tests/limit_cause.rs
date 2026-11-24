//! 한도 거절의 원인은 **이 호출의 결과와 함께** 온다(`noting_limit`): 호출마다 따로인 표지라, 앞 요청이 한도 거절을 받고 아무도 읽지 않은 채 사라져도(접속이
//! 끊겨도) 같은 위치에서 나중에 난 저장소 장애를 한도로 확정해 봉인하지 않는다. 진짜 한도 거절만 `unavailable` 표식이 된다.

use super::{
    cli::{call, record},
    refusal::{judged, word},
    refuse_all::{Inject, Wrapped, one_token, wrapper},
    *,
};
use bot_core::{
    cli::CliRefusal,
    command::{JsonPayload, ToolCall},
    port::Broker,
};

const SAY: &str = r#"{"text":"hi"}"#;

#[tokio::test]
async fn a_limit_refusal_nobody_read_cannot_seal_a_later_storage_failure_at_the_same_position() {
    let wrapped = Arc::new(Wrapped::default());
    let r = rig_full(|_| {}, |_| {}, judged(300), one_token(), wrapper(&wrapped)).await;
    // 첫 위치가 한도의 유일한 자리를 쓴다
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    let reply = rpc(&r, &call("toolu_a", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    // 앞 요청이 두 번째 위치에서 한도 거절을 받았지만 접속이 끊겨 프런트가 결과를 읽지 못했다(호출만 하고 버린다)
    record(&r, 8, &[("toolu_b", "echo_say", SAY)]);
    let abandoned = wrapped
        .call(ToolCall {
            attempt: r.attempt.clone(),
            call: CallId("cli-8-0".into()),
            tool: ToolId("echo.say".into()),
            args: JsonPayload(SAY.into()),
        })
        .await;
    assert!(matches!(abandoned, Err(bot_core::port::Error::Unavailable)));
    // 한도가 회복된 뒤 같은 자리에 진짜 저장소 오류가 났다: 거절이 아니므로 표식이 없다
    r.clock.advance(Duration::from_secs(60));
    wrapped.fail_with(Some(Inject::Unavailable));
    assert_eq!(word(&r, "toolu_b", "echo_say", SAY).await, "unavailable");
    assert!(
        r.turns.refusals().is_empty(),
        "저장소 장애를 한도 거절로 봉인했다: {:?}",
        r.turns.refusals()
    );
    assert_eq!(r.runner.jobs().len(), 1);
    // 저장소가 돌아오면 같은 위치를 다시 시도해 실행한다
    wrapped.fail_with(None);
    let reply = rpc(&r, &call("toolu_b", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 2);
    // 진짜 한도 거절은 그 호출의 결과와 함께 와서 `unavailable` 표식이 된다
    record(&r, 12, &[("toolu_c", "echo_say", SAY)]);
    assert_eq!(word(&r, "toolu_c", "echo_say", SAY).await, "unavailable");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-12-0".into()), CliRefusal::RateLimited)]
    );
    assert_eq!(
        r.runner.jobs().len(),
        2,
        "한도에 걸린 호출은 실행하지 않았다"
    );
}

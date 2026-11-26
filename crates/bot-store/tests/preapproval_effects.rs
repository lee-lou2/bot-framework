//! F1 사전 승인의 쓰기 허가: 허용 쌍·일별 한도(경합·UTC 일 경계·재전달·미실행 판정 뒤 재허가)·실제 효과 채널 응답. 실제 PG 위에서 확인한다.

mod support;

use bot_core::{command::*, grant::reason, id::*, ledger::*, port::Error, state::*};
use support::{preapproval::*, *};

const PAIRS: &[(&str, &str)] = &[("post", "channel:reports"), ("notify", "user:ops")];

fn confirmed(effect: &EffectId, receipt: &str) -> EffectStatus {
    EffectStatus {
        effect: effect.clone(),
        state: EffectState::Confirmed,
        resolution: None,
        receipt: Some(receipt.into()),
    }
}

#[tokio::test]
async fn only_the_exact_pairs_get_a_write_and_each_new_write_counts_once() {
    let Some(r) = rig(10, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    // 허용 쌍은 새 쓰기를 허가하고 사용량을 1 올린다. 호출 영수증과 단계 영수증에 승인 ID가 남는다
    let first = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(first.send);
    let step_grant: String = r.db.scalar("select grant_id from bot_effects").await;
    assert_eq!(step_grant, r.grant.0);
    assert_eq!(r.used().await, 1);
    assert!(
        r.verdicts("effect.start")
            .await
            .contains(&format!("s1;grant={}", r.grant.0))
    );
    // 허용 쌍의 교차 조합·밖의 대상·모양이 다른 요청은 모두 쓰기 0이고 사용량도 그대로다
    let wrong = [
        (
            "교차(post→user:ops)",
            r#"{"operation":"post","target":"user:ops","params":{}}"#,
        ),
        (
            "교차(notify→channel)",
            r#"{"operation":"notify","target":"channel:reports","params":{}}"#,
        ),
        (
            "밖의 대상",
            r#"{"operation":"post","target":"channel:other","params":{}}"#,
        ),
        (
            "params 없음",
            r#"{"operation":"post","target":"channel:reports"}"#,
        ),
        (
            "params가 객체가 아님",
            r#"{"operation":"post","target":"channel:reports","params":"x"}"#,
        ),
        (
            "모르는 키",
            r#"{"operation":"post","target":"channel:reports","params":{},"to":"x"}"#,
        ),
        (
            "숫자 대상",
            r#"{"operation":"post","target":5,"params":{}}"#,
        ),
        ("옛 모양(step만)", r#"{"item":"42"}"#),
    ];
    // wire 분류: 잘 구성됐지만 허용 쌍 밖이면 denied, 봉투 모양이 다르면 invalid
    for (i, (name, args)) in wrong.into_iter().enumerate() {
        let refused = effects(&r.db)
            .request(effect_req(&ctx, &call, &format!("bad-{i}"), args))
            .await;
        match (i < 3, &refused) {
            (true, Err(Error::Denied)) | (false, Err(Error::Invalid)) => {}
            _ => panic!("{name}: 분류가 다르다"),
        }
    }
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_effects").await,
        1,
        "거절은 단계를 만들지 않는다"
    );
    assert_eq!(r.used().await, 1);
    assert_eq!(
        r.verdicts("effect.refuse")
            .await
            .iter()
            .filter(|v| *v == reason::TARGET_DENIED)
            .count(),
        8
    );
    // 다른 허용 쌍의 새 단계는 각각 한 번이다. 같은 단계의 재전달은 0회(보내지 않는다)
    assert!(
        r.effect(&ctx, &call, "s2", "notify", "user:ops")
            .await
            .unwrap()
            .send
    );
    assert_eq!(r.used().await, 2);
    let again = r
        .effect(&ctx, &call, "s2", "notify", "user:ops")
        .await
        .unwrap();
    assert!(!again.send, "재전달은 쓰기를 다시 허가하지 않는다");
    assert_eq!(r.used().await, 2);
    // 같은 단계의 다른 지문은 충돌이다(사용량 0)
    let conflict = r.effect(&ctx, &call, "s2", "post", "channel:reports").await;
    assert!(matches!(conflict, Err(Error::Conflict)));
    assert_eq!(r.used().await, 2);
    // 먼저 허가한 쓰기의 결과 기록은 한도와 상관없이 받는다
    let id = first.status.effect;
    effects(&r.db)
        .record(ctx.attempt.clone(), call.clone(), confirmed(&id, "msg-1"))
        .await
        .unwrap();
}

#[tokio::test]
async fn the_daily_limit_holds_across_connections_runs_days_and_republishing() {
    let Some(r) = rig(1, PAIRS).await else {
        return;
    };
    let (a, a_call) = r.started().await;
    let (b, b_call) = r.started().await;
    // 서로 다른 프로세스·서로 다른 run이 같은 트리거의 한도 1을 두고 겨룬다: 외부 쓰기 허가는 정확히 1
    let peer = r.db.peer("second-host").await;
    let (first, second) = tokio::join!(
        async {
            bot_core::port::EffectJournal::request(
                &r.db.store,
                effect_req(
                    &a,
                    &a_call,
                    "x",
                    r#"{"operation":"post","target":"channel:reports","params":{"i":1}}"#,
                ),
            )
            .await
        },
        async {
            bot_core::port::EffectJournal::request(
                &peer,
                effect_req(
                    &b,
                    &b_call,
                    "y",
                    r#"{"operation":"notify","target":"user:ops","params":{"i":2}}"#,
                ),
            )
            .await
        }
    );
    let sent = [&first, &second]
        .iter()
        .filter(|p| p.as_ref().is_ok_and(|p| p.send))
        .count();
    let refused = [&first, &second]
        .iter()
        .filter(|p| matches!(p, Err(Error::Denied)))
        .count();
    assert_eq!((sent, refused), (1, 1));
    assert_eq!(r.used().await, 1);
    assert_eq!(r.audits("effect.start").await, 1);
    assert_eq!(
        r.verdicts("effect.refuse").await,
        vec![reason::LIMIT.to_owned()]
    );
    // 한도 초과 뒤 같은 단계를 다시 부르거나 자정 전에 재게시·재활성화해도 사용량은 그대로다
    let loser = if first.is_ok() {
        (&b, &b_call, "y")
    } else {
        (&a, &a_call, "x")
    };
    let retry = effects(&r.db)
        .request(effect_req(
            loser.0,
            loser.1,
            loser.2,
            if first.is_ok() {
                r#"{"operation":"notify","target":"user:ops","params":{"i":2}}"#
            } else {
                r#"{"operation":"post","target":"channel:reports","params":{"i":1}}"#
            },
        ))
        .await;
    assert!(matches!(retry, Err(Error::Denied)));
    let changed = world(1, &[("post", "channel:reports"), ("notify", "user:ops")]);
    let spec = spec_of(&changed, IMAGE);
    assert!(
        publish(&r.db, &changed, vec![spec]).await.is_empty(),
        "같은 선언은 새 ID가 아니다"
    );
    assert_eq!(r.used().await, 1);
    // UTC 날짜가 바뀌면(어제의 행) 새 날의 한도가 열린다. 사용량 행은 (트리거, 일)이 키다
    r.db.sql("update bot_grant_usage set day = day - 1").await;
    let next = r
        .effect(loser.0, loser.1, "z", "post", "channel:reports")
        .await
        .unwrap();
    assert!(next.send);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_grant_usage")
            .await,
        2
    );
    assert_eq!(r.used().await, 2, "어제 1 + 오늘 1");
}

#[tokio::test]
async fn a_hold_resolved_as_not_executed_costs_a_new_write_and_a_duplicate_costs_none() {
    let Some(r) = rig(2, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let first = r
        .effect(&ctx, &call, "up", "post", "channel:reports")
        .await
        .unwrap();
    assert!(first.send);
    // 시도가 끊겨 다른 소유자가 인계하고 호출을 풀어 다시 시작해도 옛 단계를 다시 보내지 않는다(불명 보류)
    let new = r.db.takeover(&ctx.attempt.run).await;
    calls(&r.db)
        .finish(FinishCall {
            attempt: new.attempt.clone(),
            call: call.clone(),
            outcome: Outcome::Released,
        })
        .await
        .unwrap();
    assert_eq!(
        r.start(&new, &call).await.unwrap(),
        bot_core::grant::Granted::Started
    );
    let held = r
        .effect(&new, &call, "up", "post", "channel:reports")
        .await
        .unwrap();
    assert!(!held.send);
    assert_eq!(r.used().await, 1, "보류 재요청은 0회");
    // 운영자가 미실행을 입증하면 새 외부 쓰기이므로 그날 한도 1회를 더 쓴다
    r.db.sql("update bot_effects set resolution = 'not_executed'")
        .await;
    let resend = r
        .effect(&new, &call, "up", "post", "channel:reports")
        .await
        .unwrap();
    assert!(resend.send);
    assert_eq!(r.used().await, 2);
    assert!(
        r.verdicts("effect.restart")
            .await
            .contains(&format!("up;grant={}", r.grant.0))
    );
    // 한도를 다 쓰면 다음 미실행 판정 뒤 재허가는 한도로 끝난다(밀린 쓰기를 자동 재생하지 않는다)
    let other = r.effect(&new, &call, "dn", "notify", "user:ops").await;
    assert!(matches!(other, Err(Error::Denied)));
    assert_eq!(r.used().await, 2);
}

#[tokio::test]
async fn a_recorded_step_with_other_args_is_stale_without_a_valid_grant_and_a_conflict_only_with_one()
 {
    let Some(r) = rig(10, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let first = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(first.send);
    let other = |args: &str| effects(&r.db).request(effect_req(&ctx, &call, "s1", args));
    let changed = r#"{"operation":"post","target":"channel:reports","params":{"item":"other"}}"#;
    // 같은 유효한 승인(철회·교체·정지 세대 변화 없음) 안에서 기록된 단계의 인자만 바뀌면 conflict
    assert!(matches!(other(changed).await, Err(Error::Conflict)));
    // 정지는 세대를 올린다: 정지 전에 시작한 호출은 정지 중에도 옛 세대라 같은 요청이 conflict가 아니라 stale이다
    r.pause(&r.grant.0, 0, true).await.unwrap();
    assert!(matches!(other(changed).await, Err(Error::Stale)));
    // 재개해도 마찬가지다(세대는 다시 한 번 올랐다)
    r.pause(&r.grant.0, 1, false).await.unwrap();
    assert!(matches!(other(changed).await, Err(Error::Stale)));
    // 같은 인자의 재요청은 새 쓰기가 아니라 상태를 돌려준다(낡아도 먼저 허가한 쓰기의 복구는 받는다)
    let replay = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(!replay.send);
    // 승인이 교체(철회)돼도 마찬가지다
    let (fresh, fresh_call) = r.started().await;
    assert!(
        r.effect(&fresh, &fresh_call, "t1", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    let republished = world(11, PAIRS);
    publish(&r.db, &republished, vec![spec_of(&republished, IMAGE)]).await;
    let revoked = effects(&r.db)
        .request(effect_req(&fresh, &fresh_call, "t1", changed))
        .await;
    assert!(
        matches!(revoked, Err(Error::Stale)),
        "철회된 승인의 다른 인자"
    );
    assert_eq!(
        r.verdicts("effect.refuse")
            .await
            .iter()
            .filter(|v| *v == reason::STALE)
            .count(),
        3,
        "정지 중·재개 뒤·철회 뒤 한 번씩"
    );
    assert_eq!(r.used().await, 2, "충돌·낡은 요청은 사용량을 올리지 않는다");
}

async fn say(channel: &bot_broker::testing::EffectChannel, text: String) -> String {
    channel.handle(text.as_bytes()).await
}

/// 실제 `EffectChannel`이 PG 위에서 내는 응답: 새 허가·재전달·미실행 판정 뒤 재허가·분류된 오류.
#[tokio::test]
async fn the_effect_channel_answers_new_and_restarted_permits_and_typed_errors() {
    use bot_broker::testing::EffectChannel;
    use std::sync::Arc;
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let channel = |attempt: &Attempt| {
        EffectChannel::new(
            Arc::new(r.db.store.clone()),
            attempt.clone(),
            call.clone(),
            ToolId(TOOL.into()),
        )
    };
    let frame = |id: u32, step: &str, args: &str| {
        format!(r#"{{"v":1,"id":"{id}","method":"effect.request","step":"{step}","args":{args}}}"#)
    };
    let post = |item: u32| {
        format!(r#"{{"operation":"post","target":"channel:reports","params":{{"item":"{item}"}}}}"#)
    };
    let ch = channel(&ctx.attempt);
    // 새 허가: started·send true. 같은 단계의 재전달은 send false(보류)다
    let first = say(&ch, frame(1, "s1", &post(1))).await;
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(
        (value["state"].clone(), value["send"].clone()),
        ("started".into(), true.into()),
        "{first}"
    );
    let effect = value["effect"].as_str().unwrap().to_owned();
    let replay = say(&ch, frame(2, "s1", &post(1))).await;
    assert!(
        replay.contains(r#""send":false"#) && replay.contains(r#""state":"held""#),
        "{replay}"
    );
    // 분류: 허용 쌍 밖 denied · 봉투 모양 invalid · 같은 단계의 다른 지문 conflict
    let outside = r#"{"operation":"post","target":"channel:nope","params":{}}"#;
    assert_eq!(
        say(&ch, frame(3, "s2", outside)).await,
        r#"{"v":1,"id":"3","error":"denied"}"#
    );
    assert_eq!(
        say(
            &ch,
            frame(
                4,
                "s2",
                r#"{"operation":"post","target":"channel:reports"}"#
            )
        )
        .await,
        r#"{"v":1,"id":"4","error":"invalid"}"#
    );
    assert_eq!(
        say(&ch, frame(5, "s1", &post(2))).await,
        r#"{"v":1,"id":"5","error":"conflict"}"#
    );
    assert_eq!(r.used().await, 1, "오류에서 쓰기 허가 0");
    // 정지 중 새 허가는 denied, 먼저 허가한 쓰기의 결과 보고는 받는다
    pause(&r, &r.grant.0, 0, true).await.unwrap();
    assert_eq!(
        say(&ch, frame(6, "s3", &post(3))).await,
        r#"{"v":1,"id":"6","error":"denied"}"#
    );
    let report = say(
        &ch,
        format!(r#"{{"v":1,"id":"7","method":"effect.status","effect":"{effect}","outcome":"confirmed","receipt":"msg-1"}}"#),
    )
    .await;
    assert!(report.contains(r#""state":"confirmed""#), "{report}");
    // 재개 뒤에도 정지 전에 시작한 호출은 새 허가를 받지 못한다(stale)
    pause(&r, &r.grant.0, 1, false).await.unwrap();
    assert_eq!(
        say(&ch, frame(8, "s3", &post(3))).await,
        r#"{"v":1,"id":"8","error":"stale"}"#
    );
    assert_eq!(r.used().await, 1);
    // 재개 뒤 새 발생: 인계·미실행 판정 뒤 같은 단계를 다시 보내면 그날 한도를 한 번 더 쓴다
    let (fresh, fresh_call) = r.started().await;
    let channel = |attempt: &Attempt| {
        EffectChannel::new(
            Arc::new(r.db.store.clone()),
            attempt.clone(),
            fresh_call.clone(),
            ToolId(TOOL.into()),
        )
    };
    let up = say(&channel(&fresh.attempt), frame(9, "up", &post(9))).await;
    assert!(up.contains(r#""send":true"#), "{up}");
    let new = r.db.takeover(&fresh.attempt.run).await;
    calls(&r.db)
        .finish(FinishCall {
            attempt: new.attempt.clone(),
            call: fresh_call.clone(),
            outcome: Outcome::Released,
        })
        .await
        .unwrap();
    assert_eq!(
        r.start(&new, &fresh_call).await.unwrap(),
        bot_core::grant::Granted::Started
    );
    let held = say(&channel(&new.attempt), frame(10, "up", &post(9))).await;
    assert!(
        held.contains(r#""state":"held""#) && held.contains(r#""send":false"#),
        "{held}"
    );
    assert_eq!(r.used().await, 2);
    r.db.sql("update bot_effects set resolution = 'not_executed' where step = 'up'")
        .await;
    let again = say(&channel(&new.attempt), frame(11, "up", &post(9))).await;
    assert!(
        again.contains(r#""state":"started""#) && again.contains(r#""send":true"#),
        "{again}"
    );
    assert_eq!(r.used().await, 3, "미실행 판정 뒤 새 send:true는 그날 1회");
    let last = say(&channel(&new.attempt), frame(12, "z", &post(10))).await;
    assert_eq!(last, r#"{"v":1,"id":"12","error":"denied"}"#, "한도 3 소진");
}

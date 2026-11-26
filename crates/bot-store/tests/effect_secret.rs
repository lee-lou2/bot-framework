//! 도구가 보고하는 영수증·보류 사유에 알려진 자격이 섞이면 PG의 단계 원장과 감사 체인에 그 값이 한 글자도 남지 않는다: 실제 `EffectChannel`(자격을 아는 채널)이
//! 실제 PG 위에서 원문을 저장하지 않고 고정 사유(`receipt_credential`·`reason_credential`)로 보류한다. 관리자 보류 목록도 고정 사유만 본다.

mod support;

use bot_broker::testing::EffectChannel;
use bot_core::{admin::*, id::*, ledger::*, port::*, state::*};
use std::sync::Arc;
use support::{admin::*, *};

/// 합성 비밀: 선언한 자격 값 둘(하나는 소문자·숫자라 보류 사유로도 들어올 수 있다)과 토큰 모양(소스에 리터럴이 남지 않게 이어 붙인다).
const PASSWORD: &str = "synthsecret0x7";
const API_KEY: &str = "Synth-API-Key-ABC123";

fn token() -> String {
    format!("{}-{}", "xoxb", "1234567890-synthetictoken")
}

async fn running(db: &TestDb) -> RunContext {
    let run = leased(db, "r").await;
    begin_call(db, &run.attempt, "c1").await;
    calls(db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    run
}

fn channel(db: &TestDb, at: &Attempt) -> EffectChannel {
    EffectChannel::new(
        Arc::new(db.store.for_activation(Revision("set-1".into()))),
        at.clone(),
        call("c1"),
        tool("web.search"),
    )
    .with_credentials(vec![
        PASSWORD.as_bytes().to_vec(),
        API_KEY.as_bytes().to_vec(),
    ])
}

fn request(step: &str) -> String {
    format!(r#"{{"v":1,"id":"1","method":"effect.request","step":"{step}","args":{{"n":1}}}}"#)
}

fn report(effect: &str, outcome: &str, field: &str, value: &str) -> String {
    format!(
        r#"{{"v":1,"id":"2","method":"effect.status","effect":"{effect}","outcome":"{outcome}","{field}":{}}}"#,
        serde_json::to_string(value).unwrap()
    )
}

fn effect_of(reply: &str) -> String {
    serde_json::from_str::<serde_json::Value>(reply).unwrap()["effect"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// 값이 들어 있는 곳이 하나도 없다: 단계 원장 전체와 감사 체인의 모든 열.
async fn nowhere(db: &TestDb, secret: &str) {
    let like = format!("%{secret}%");
    let effects: i64 =
        sqlx::query_scalar("select count(*) from bot_effects where bot_effects::text like $1")
            .bind(&like)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    let audit: i64 =
        sqlx::query_scalar("select count(*) from bot_audit where bot_audit::text like $1")
            .bind(&like)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!((effects, audit), (0, 0), "{secret}");
}

#[tokio::test]
async fn a_receipt_that_carries_a_secret_is_held_with_a_fixed_reason_and_the_value_is_stored_nowhere()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let c = channel(&db, &run.attempt);
    for (step, secret) in [
        ("by_password", PASSWORD.to_owned()),
        ("by_key", API_KEY.to_owned()),
        ("by_token", token()),
    ] {
        let effect = effect_of(&c.handle(request(step).as_bytes()).await);
        let reply = c
            .handle(
                report(
                    &effect,
                    "confirmed",
                    "receipt",
                    &format!("saved;{secret};ok"),
                )
                .as_bytes(),
            )
            .await;
        assert!(
            reply.contains(r#""state":"held""#) && reply.contains("receipt_credential"),
            "{reply}"
        );
        assert!(
            !reply.contains(&secret),
            "도구에 되돌려 주는 글에도 없다: {reply}"
        );
        nowhere(&db, &secret).await;
    }
    // 원장에는 세 단계 모두 고정 사유의 보류이고, 감사는 `held:receipt_credential`이다
    let rows: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as("select state, reason, receipt from bot_effects order by step")
            .fetch_all(db.store.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    for (state, reason, receipt) in rows {
        assert_eq!(
            (state.as_str(), reason.as_deref(), receipt),
            ("held", Some("receipt_credential"), None)
        );
    }
    let verdicts: Vec<String> = sqlx::query_scalar(
        "select verdict from bot_audit where action = 'effect.record' order by seq",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    assert_eq!(verdicts, ["held:receipt_credential"; 3]);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_hold_reason_that_carries_a_secret_is_replaced_and_the_value_is_stored_nowhere() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let c = channel(&db, &run.attempt);
    // 사유 낱말(소문자·숫자·밑줄)로 들어올 수 있는 자격 값만 사유 자리를 지나간다
    for (step, secret) in [("one", PASSWORD.to_owned())] {
        let effect = effect_of(&c.handle(request(step).as_bytes()).await);
        let reply = c
            .handle(report(&effect, "held", "reason", &secret).as_bytes())
            .await;
        assert!(reply.contains("reason_credential"), "{reply}");
        assert!(!reply.contains(&secret), "{reply}");
        nowhere(&db, &secret).await;
    }
    // 토큰 모양(`ghp_` + 소문자·숫자)도 사유 자리에서 막힌다
    let shaped = format!("{}_{}", "ghp", "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6");
    let effect = effect_of(&c.handle(request("two").as_bytes()).await);
    let reply = c
        .handle(report(&effect, "held", "reason", &shaped).as_bytes())
        .await;
    assert!(reply.contains("reason_credential"), "{reply}");
    nowhere(&db, &shaped).await;
    // 깨끗한 사유는 그대로 남는다
    let effect = effect_of(&c.handle(request("three").as_bytes()).await);
    c.handle(report(&effect, "held", "reason", "timeout").as_bytes())
        .await;
    let reasons: Vec<String> = sqlx::query_scalar("select reason from bot_effects order by step")
        .fetch_all(db.store.pool())
        .await
        .unwrap();
    assert_eq!(
        reasons,
        ["reason_credential", "timeout", "reason_credential"]
    );
    let verdicts: Vec<String> = sqlx::query_scalar(
        "select verdict from bot_audit where action = 'effect.record' order by seq",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        verdicts,
        [
            "held:reason_credential",
            "held:reason_credential",
            "held:timeout"
        ]
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_step_name_that_is_a_secret_is_refused_before_the_start_is_audited() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let c = channel(&db, &run.attempt);
    // `effect.start` 감사의 낱말은 단계 이름이라, 자격인 이름은 기록하기 전에 거절한다
    let reply = c.handle(request(PASSWORD).as_bytes()).await;
    assert_eq!(reply, r#"{"v":1,"id":"1","error":"invalid"}"#);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_effects").await,
        0
    );
    nowhere(&db, PASSWORD).await;
}

#[tokio::test]
async fn the_operator_sees_only_the_fixed_reason_and_a_later_lookup_shows_no_receipt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let c = channel(&db, &run.attempt);
    let effect = effect_of(&c.handle(request("upload").as_bytes()).await);
    c.handle(report(&effect, "confirmed", "receipt", &format!("pw={PASSWORD}")).as_bytes())
        .await;
    // 조회 양식도 영수증을 싣지 않는다(보류 사유 코드는 조회에 나오지 않는다)
    let lookup = c
        .handle(
            r#"{"v":1,"id":"7","method":"effect.status","step":"upload","args":{"n":1}}"#
                .as_bytes(),
        )
        .await;
    assert!(
        lookup.contains(r#""state":"held""#) && !lookup.contains("receipt"),
        "{lookup}"
    );
    assert!(!lookup.contains(PASSWORD));
    calls(&db)
        .finish(FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    // 관리자 보류 목록·상세에 단계의 사유가 고정 사유로만 보인다
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    let dump = format!("{holds:?}");
    assert!(!dump.contains(PASSWORD), "{dump}");
    assert!(dump.contains("receipt_credential"), "{dump}");
    let detail = AdminRead::detail(
        &db.store,
        &operator(),
        run.attempt.profile.clone(),
        run.attempt.run.clone(),
    )
    .await
    .unwrap();
    assert!(!format!("{detail:?}").contains(PASSWORD));
}

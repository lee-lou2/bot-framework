//! Credential scrubbing on `EffectChannel` over PG: secrets in tool receipts, hold reasons,
//! or step names never reach `bot_effects`, `bot_audit`, tool replies, or admin hold views.

use crate::support::{admin::*, *};
use bot_broker::testing::EffectChannel;
use bot_core::{admin::*, id::*, ledger::*, port::*, state::*};
use std::sync::Arc;

// Synthetic secrets: lowercase alphanumeric PASSWORD can also pass reason syntax; token is split to avoid literal matches.
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
            "tool reply must not echo secret: {reply}"
        );
        nowhere(&db, &secret).await;
    }
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
    for (step, secret) in [("one", PASSWORD.to_owned())] {
        let effect = effect_of(&c.handle(request(step).as_bytes()).await);
        let reply = c
            .handle(report(&effect, "held", "reason", &secret).as_bytes())
            .await;
        assert!(reply.contains("reason_credential"), "{reply}");
        assert!(!reply.contains(&secret), "{reply}");
        nowhere(&db, &secret).await;
    }
    // Token-shaped strings (`ghp_` + lowercase/digits) are also scrubbed from hold reasons.
    let shaped = format!("{}_{}", "ghp", "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6");
    let effect = effect_of(&c.handle(request("two").as_bytes()).await);
    let reply = c
        .handle(report(&effect, "held", "reason", &shaped).as_bytes())
        .await;
    assert!(reply.contains("reason_credential"), "{reply}");
    nowhere(&db, &shaped).await;

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
    // Reject credential step names before `effect.start` can audit them.
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

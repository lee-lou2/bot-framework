//! Pre-approval write permits (PG): allowed pairs, daily limits (races, UTC boundaries, replays, post-NotExecuted restarts), and `EffectChannel` responses.

use crate::support::{preapproval::*, *};
use bot_core::{command::*, grant::reason, id::*, ledger::*, port::Error, state::*};

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
    let wrong = [
        (
            "cross(post->user:ops)",
            r#"{"operation":"post","target":"user:ops","params":{}}"#,
        ),
        (
            "cross(notify->channel)",
            r#"{"operation":"notify","target":"channel:reports","params":{}}"#,
        ),
        (
            "outside target",
            r#"{"operation":"post","target":"channel:other","params":{}}"#,
        ),
        (
            "missing params",
            r#"{"operation":"post","target":"channel:reports"}"#,
        ),
        (
            "non-object params",
            r#"{"operation":"post","target":"channel:reports","params":"x"}"#,
        ),
        (
            "unknown key",
            r#"{"operation":"post","target":"channel:reports","params":{},"to":"x"}"#,
        ),
        (
            "numeric target",
            r#"{"operation":"post","target":5,"params":{}}"#,
        ),
        ("legacy shape (step only)", r#"{"item":"42"}"#),
    ];
    // Well-formed disallowed pairs return Denied; malformed envelopes return Invalid.
    for (i, (name, args)) in wrong.into_iter().enumerate() {
        let refused = effects(&r.db)
            .request(effect_req(&ctx, &call, &format!("bad-{i}"), args))
            .await;
        match (i < 3, &refused) {
            (true, Err(Error::Denied)) | (false, Err(Error::Invalid)) => {}
            _ => panic!("{name}: unexpected classification: {:?}", refused.err()),
        }
    }
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_effects").await,
        1,
        "refused requests must not create effect rows"
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
    assert!(!again.send, "replay must not re-grant send");
    assert_eq!(r.used().await, 2);
    let conflict = r.effect(&ctx, &call, "s2", "post", "channel:reports").await;
    assert!(matches!(conflict, Err(Error::Conflict)));
    assert_eq!(r.used().await, 2);
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
    // Two connections and two runs race for a daily limit of 1: exactly one send wins.
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
        "identical declaration keeps the same grant ID"
    );
    assert_eq!(r.used().await, 1);
    // Moving usage to the previous UTC day opens a fresh daily quota.
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
    assert_eq!(r.used().await, 2, "yesterday 1 + today 1");
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
    assert_eq!(r.used().await, 1, "held re-request spends no quota");
    // Re-permitting after NotExecuted is a new external write and spends 1 quota unit.
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
    assert!(matches!(other(changed).await, Err(Error::Conflict)));
    // Pausing increments grant epoch, turning changed-args requests on prior calls into Stale.
    r.pause(&r.grant.0, 0, true).await.unwrap();
    assert!(matches!(other(changed).await, Err(Error::Stale)));
    r.pause(&r.grant.0, 1, false).await.unwrap();
    assert!(matches!(other(changed).await, Err(Error::Stale)));
    // Same-args replay still returns status for recovery even after grant epoch changes.
    let replay = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(!replay.send);
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
        "changed args under revoked grant must be Stale"
    );
    assert_eq!(
        r.verdicts("effect.refuse")
            .await
            .iter()
            .filter(|v| *v == reason::STALE)
            .count(),
        3,
        "once while paused, once after resume, once after revocation"
    );
    assert_eq!(
        r.used().await,
        2,
        "conflicts and stale requests spend no quota"
    );
}

async fn say(channel: &bot_broker::testing::EffectChannel, text: String) -> String {
    channel.handle(text.as_bytes()).await
}

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
    assert_eq!(r.used().await, 1, "errors must not spend quota");
    // While paused, new permits are denied, but status reports for previously granted writes succeed.
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
    pause(&r, &r.grant.0, 1, false).await.unwrap();
    assert_eq!(
        say(&ch, frame(8, "s3", &post(3))).await,
        r#"{"v":1,"id":"8","error":"stale"}"#
    );
    assert_eq!(r.used().await, 1);
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
    assert_eq!(
        r.used().await,
        3,
        "restart after NotExecuted spends 1 quota unit"
    );
    let last = say(&channel(&new.attempt), frame(12, "z", &post(10))).await;
    assert_eq!(
        last, r#"{"v":1,"id":"12","error":"denied"}"#,
        "limit of 3 exhausted"
    );
}

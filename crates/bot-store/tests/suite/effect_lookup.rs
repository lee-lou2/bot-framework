//! End-to-end PG tests for `EffectJournal::lookup`: read-only step inspection, fencing, admin verdicts, and `EffectChannel` wire responses.

use crate::support::{admin::*, *};
use bot_broker::testing::EffectChannel;
use bot_core::{admin::*, command::*, id::*, ledger::*, port::*, state::*};
use std::sync::Arc;

const TOOL: &str = "web.search";

fn request(attempt: &Attempt, call_id: &str, name: &str, args: &str) -> EffectRequest {
    EffectRequest {
        attempt: attempt.clone(),
        call: call(call_id),
        step: name.into(),
        args: JsonPayload(args.into()),
    }
}

async fn lookup_as(
    db: &TestDb,
    at: &Attempt,
    call_id: &str,
    name: &str,
    args: &str,
    as_tool: &str,
) -> Result<Option<EffectStatus>, Error> {
    effects(db)
        .lookup(request(at, call_id, name, args), tool(as_tool))
        .await
}

async fn lookup(
    db: &TestDb,
    at: &Attempt,
    name: &str,
    args: &str,
) -> Result<Option<EffectStatus>, Error> {
    lookup_as(db, at, "c1", name, args, TOOL).await
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

async fn open_step(db: &TestDb, at: &Attempt, name: &str, args: &str) -> EffectId {
    effects(db)
        .request(request(at, "c1", name, args))
        .await
        .unwrap()
        .status
        .effect
}

async fn report(
    db: &TestDb,
    at: &Attempt,
    effect: &EffectId,
    state: EffectState,
    text: Option<&str>,
) {
    effects(db)
        .record(
            at.clone(),
            call("c1"),
            EffectStatus {
                effect: effect.clone(),
                state,
                resolution: None,
                receipt: text.map(str::to_owned),
            },
        )
        .await
        .unwrap();
}

/// Fingerprint of all tables and leases that read-only lookup must never mutate.
async fn snapshot(db: &TestDb) -> (i64, String, String, String) {
    (
        db.scalar("select count(*) from bot_audit").await,
        db.scalar(
            "select coalesce(string_agg(id || state || generation::text || coalesce(reason, '') || coalesce(receipt, '') || coalesce(resolution, '') || updated_ms::text, '|' order by id), '') from bot_effects",
        )
        .await,
        db.scalar(
            "select coalesce(string_agg(call || state || updated_ms::text, '|' order by call), '') from bot_calls",
        )
        .await,
        db.scalar(
            "select coalesce(string_agg(id || state || generation::text || claim_epoch::text || coalesce(lease_until_ms, 0)::text, '|' order by id), '') from bot_runs",
        )
        .await,
    )
}

#[tokio::test]
async fn a_lookup_reads_every_state_and_writes_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let at = &run.attempt;
    let before = snapshot(&db).await;
    assert!(lookup(&db, at, "upload", "{}").await.unwrap().is_none());
    assert_eq!(snapshot(&db).await, before);

    let started = open_step(&db, at, "started", "{}").await;
    let confirmed = open_step(&db, at, "confirmed", "{}").await;
    let rejected = open_step(&db, at, "rejected", "{}").await;
    let held = open_step(&db, at, "held", "{}").await;
    report(&db, at, &confirmed, EffectState::Confirmed, Some("file-17")).await;
    report(&db, at, &rejected, EffectState::Rejected, None).await;
    report(&db, at, &held, EffectState::Held, Some("timeout")).await;
    let before = snapshot(&db).await;
    for (name, effect, state, receipt) in [
        ("started", &started, EffectState::Started, None),
        (
            "confirmed",
            &confirmed,
            EffectState::Confirmed,
            Some("file-17"),
        ),
        ("rejected", &rejected, EffectState::Rejected, None),
        // Hold reason code is not returned as a receipt on lookup.
        ("held", &held, EffectState::Held, None),
    ] {
        for _ in 0..2 {
            let seen = lookup(&db, at, name, "{}").await.unwrap().expect(name);
            assert_eq!(seen.effect, *effect, "{name}");
            assert_eq!(seen.state, state, "{name}");
            assert_eq!(seen.receipt.as_deref(), receipt, "{name}");
            assert_eq!(seen.resolution, None, "{name}: before resolution");
        }
    }
    assert_eq!(snapshot(&db).await, before, "lookup must write nothing");
    // Only held rows surface a resolution; stray values on non-held rows are ignored.
    db.sql("update bot_effects set resolution = 'not_executed' where step in ('started', 'confirmed', 'rejected')")
        .await;
    for name in ["started", "confirmed", "rejected"] {
        let seen = lookup(&db, at, name, "{}").await.unwrap().expect(name);
        assert_eq!(seen.resolution, None, "{name}");
    }
}

#[tokio::test]
async fn a_lookup_never_turns_an_old_generations_started_step_into_a_hold() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let id = open_step(&db, &run.attempt, "upload", "{}").await;
    let new = db.takeover(&run.attempt.run).await;
    let before = snapshot(&db).await;
    // Only `request` transitions an old-generation started step to held; `lookup` leaves it started.
    let seen = lookup(&db, &new.attempt, "upload", "{}")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((seen.effect, seen.state), (id, EffectState::Started));
    assert_eq!(snapshot(&db).await, before);
    assert!(matches!(
        lookup(&db, &run.attempt, "upload", "{}").await,
        Err(Error::Stale)
    ));
    assert_eq!(snapshot(&db).await, before);
}

#[tokio::test]
async fn the_fingerprint_comes_from_the_same_canonical_arguments_as_the_request() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let at = &run.attempt;
    let id = open_step(&db, at, "upload", r#"{"file":"summary","target":"inbox"}"#).await;
    let before = snapshot(&db).await;
    let seen = lookup(
        &db,
        at,
        "upload",
        r#"{ "target": "inbox", "file": "summary" }"#,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(seen.effect, id);
    assert!(matches!(
        lookup(&db, at, "upload", r#"{"file":"other","target":"inbox"}"#).await,
        Err(Error::Conflict)
    ));
    assert!(
        lookup(&db, at, "notify", r#"{"file":"other","target":"inbox"}"#)
            .await
            .unwrap()
            .is_none()
    );
    for (call_id, name, args) in [
        ("c1", "", "{}"),
        ("", "upload", "{}"),
        ("c1", "upload", "{"),
    ] {
        assert!(
            matches!(
                lookup_as(&db, at, call_id, name, args, TOOL).await,
                Err(Error::Invalid)
            ),
            "{call_id:?} {name:?} {args:?}"
        );
    }
    assert_eq!(snapshot(&db).await, before);
}

#[tokio::test]
async fn a_lookup_only_sees_its_own_profile_run_call_tool_and_live_claim() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let at = &run.attempt;
    let id = open_step(&db, at, "upload", r#"{"file":"summary"}"#).await;
    runs(&db)
        .accept(accept("work", "other-profile", "hello"))
        .await
        .unwrap();
    let other = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(other.attempt.profile, ProfileId("work".into()));
    begin_call(&db, &other.attempt, "c1").await;
    calls(&db)
        .start(start_call(&other.attempt, "c1", None))
        .await
        .unwrap();
    assert!(
        lookup(&db, &other.attempt, "upload", r#"{"file":"summary"}"#)
            .await
            .unwrap()
            .is_none(),
        "step in another profile must not be visible"
    );
    let own = open_step(&db, &other.attempt, "upload", r#"{"file":"work"}"#).await;
    assert_ne!(own, id);
    let seen = lookup(&db, &other.attempt, "upload", r#"{"file":"work"}"#)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.effect, own);
    assert!(matches!(
        lookup(&db, &other.attempt, "upload", r#"{"file":"summary"}"#).await,
        Err(Error::Conflict)
    ));
    let foreign = Attempt {
        profile: ProfileId("work".into()),
        ..at.clone()
    };
    assert!(matches!(
        lookup(&db, &foreign, "upload", r#"{"file":"summary"}"#).await,
        Err(Error::Denied)
    ));
    let before = snapshot(&db).await;
    // Unknown call or wrong tool both return Denied without revealing existence.
    assert!(matches!(
        lookup_as(&db, at, "c9", "upload", r#"{"file":"summary"}"#, TOOL).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        lookup_as(
            &db,
            at,
            "c1",
            "upload",
            r#"{"file":"summary"}"#,
            "agenda.sync"
        )
        .await,
        Err(Error::Denied)
    ));
    let ghost = Attempt {
        run: RunId("no-such-run".into()),
        ..at.clone()
    };
    assert!(matches!(
        lookup(&db, &ghost, "upload", "{}").await,
        Err(Error::Missing)
    ));
    db.sql(&format!(
        "update bot_runs set claim_epoch = claim_epoch + 1 where id = '{}'",
        at.run.0
    ))
    .await;
    let bumped = snapshot(&db).await;
    assert!(matches!(
        lookup(&db, at, "upload", r#"{"file":"summary"}"#).await,
        Err(Error::Stale)
    ));
    assert_eq!(snapshot(&db).await, bumped);
    assert_ne!(before, bumped, "control: SQL above incremented claim_epoch");
    let mut current = at.clone();
    current.claim = current.claim.map(|c| ClaimEpoch(c.0 + 1));
    assert!(
        lookup(&db, &current, "upload", r#"{"file":"summary"}"#)
            .await
            .unwrap()
            .is_some()
    );
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        at.run.0
    ))
    .await;
    assert!(matches!(
        lookup(&db, &current, "upload", r#"{"file":"summary"}"#).await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn two_profiles_that_happen_to_share_run_call_and_step_ids_never_see_each_other() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let at = &run.attempt;
    let mine = open_step(&db, at, "upload", r#"{"file":"personal"}"#).await;
    // Clone the same (run, call, step) IDs into another profile with a distinct fingerprint and receipt.
    let id = &at.run.0;
    for sql in [
        format!("create table copy_runs as select * from bot_runs where profile = 'personal' and id = '{id}'"),
        "update copy_runs set profile = 'work'".to_owned(),
        "insert into bot_runs select * from copy_runs".to_owned(),
        format!("create table copy_calls as select * from bot_calls where profile = 'personal' and run = '{id}'"),
        "update copy_calls set profile = 'work'".to_owned(),
        "insert into bot_calls select * from copy_calls".to_owned(),
        format!("create table copy_effects as select * from bot_effects where profile = 'personal' and run = '{id}'"),
        "update copy_effects set profile = 'work', args_sha256 = sha256('work'::bytea), state = 'confirmed', receipt = 'work-receipt'".to_owned(),
        "insert into bot_effects select * from copy_effects".to_owned(),
    ] {
        db.sql(&sql).await;
    }
    let theirs = Attempt {
        profile: ProfileId("work".into()),
        ..at.clone()
    };
    let seen = lookup(&db, at, "upload", r#"{"file":"personal"}"#)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (seen.effect, seen.state, seen.receipt),
        (mine, EffectState::Started, None)
    );
    assert!(matches!(
        lookup(&db, &theirs, "upload", r#"{"file":"personal"}"#).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn a_real_admin_verdict_is_shown_without_being_consumed_and_the_next_request_takes_the_step_once()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let at = &run.attempt;
    let upload = open_step(&db, at, "upload", "{}").await;
    let notify = open_step(&db, at, "notify", "{}").await;
    // First step is Rejected and second is Held, so resolving notify as NotExecuted resumes the run.
    report(&db, at, &upload, EffectState::Rejected, None).await;
    report(&db, at, &notify, EffectState::Held, Some("timeout")).await;
    let seen = lookup(&db, at, "notify", "{}").await.unwrap().unwrap();
    assert_eq!((seen.state, seen.resolution), (EffectState::Held, None));
    calls(&db)
        .finish(FinishCall {
            attempt: at.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), Some(&notify)).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:admin".into()),
        target,
        HoldVerdict::NotExecuted {
            evidence: "not found at destination".into(),
        },
    )
    .await
    .unwrap();
    let second = runs(&db).claim().await.unwrap().expect("verdict wakes run");
    let before = snapshot(&db).await;
    for _ in 0..3 {
        let seen = lookup(&db, &second.attempt, "notify", "{}")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.effect, notify);
        assert_eq!(seen.state, EffectState::Held);
        assert_eq!(
            seen.resolution,
            Some(EffectResolution::NotExecuted),
            "repeated lookups must not consume the verdict"
        );
    }
    let done = lookup(&db, &second.attempt, "upload", "{}")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (done.state, done.receipt.as_deref(), done.resolution),
        (EffectState::Rejected, None, None)
    );
    assert_eq!(snapshot(&db).await, before, "lookup must write nothing");
    calls(&db)
        .finish(FinishCall {
            attempt: second.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Released,
        })
        .await
        .unwrap();
    calls(&db)
        .start(start_call(&second.attempt, "c1", None))
        .await
        .unwrap();
    let taken = effects(&db)
        .request(request(&second.attempt, "c1", "notify", "{}"))
        .await
        .unwrap();
    assert!(taken.send, "first request after verdict must send");
    assert_eq!(
        (taken.status.effect.clone(), taken.status.state),
        (notify.clone(), EffectState::Started)
    );
    let after = lookup(&db, &second.attempt, "notify", "{}")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.state, after.resolution),
        (EffectState::Started, None)
    );
    let again = effects(&db)
        .request(request(&second.attempt, "c1", "notify", "{}"))
        .await
        .unwrap();
    assert!(!again.send, "re-request within same claim must not send");
    assert_eq!(again.status.state, EffectState::Held);
    let kept = effects(&db)
        .request(request(&second.attempt, "c1", "upload", "{}"))
        .await
        .unwrap();
    assert!(!kept.send);
    assert_eq!(kept.status.state, EffectState::Rejected);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn the_real_effect_channel_answers_over_pg_with_the_wire_shape_the_tools_read() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let at = &run.attempt;
    let journal = Arc::new(db.store.for_activation(Revision("set-1".into())));
    let channel = EffectChannel::new(journal, at.clone(), call("c1"), tool(TOOL));
    let say = |frame: String| {
        let channel = &channel;
        async move { channel.handle(frame.as_bytes()).await }
    };
    let look = |name: &str| {
        format!(r#"{{"v":1,"id":"7","method":"effect.status","step":"{name}","args":{{}}}}"#)
    };
    let before = snapshot(&db).await;
    assert_eq!(
        say(look("upload")).await,
        r#"{"v":1,"id":"7","state":"none","send":false}"#
    );
    assert_eq!(snapshot(&db).await, before);
    let reply =
        say(r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{}}"#.into()).await;
    let effect: String = serde_json::from_str::<serde_json::Value>(&reply).unwrap()["effect"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        reply,
        format!(r#"{{"v":1,"id":"1","state":"started","effect":"{effect}","send":true}}"#)
    );
    assert_eq!(
        say(look("upload")).await,
        format!(r#"{{"v":1,"id":"7","state":"started","effect":"{effect}","send":false}}"#)
    );
    say(format!(
        r#"{{"v":1,"id":"2","method":"effect.status","effect":"{effect}","outcome":"held","reason":"timeout"}}"#
    ))
    .await;
    assert_eq!(
        say(look("upload")).await,
        format!(r#"{{"v":1,"id":"7","state":"held","effect":"{effect}","send":false}}"#)
    );
    assert_eq!(
        say(format!(
            r#"{{"v":1,"id":"5","method":"effect.status","effect":"{effect}"}}"#
        ))
        .await,
        r#"{"v":1,"id":"5","state":"held","send":false}"#
    );
    // Tools cannot inject a resolution through any frame.
    let injected = snapshot(&db).await;
    for frame in [
        r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{},"resolution":"not_executed"}"#.to_owned(),
        format!(r#"{{"v":1,"id":"1","method":"effect.status","effect":"{effect}","outcome":"held","reason":"x","resolution":"not_executed"}}"#),
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{},"resolution":"not_executed"}"#.to_owned(),
    ] {
        assert_eq!(say(frame).await, r#"{"v":1,"id":"1","error":"invalid"}"#);
    }
    assert_eq!(snapshot(&db).await, injected);
    calls(&db)
        .finish(FinishCall {
            attempt: at.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    let judged = EffectId(effect.clone());
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), Some(&judged)).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:admin".into()),
        target,
        HoldVerdict::NotExecuted {
            evidence: "not found at destination".into(),
        },
    )
    .await
    .unwrap();
    let second = runs(&db).claim().await.unwrap().unwrap();
    let journal = Arc::new(db.store.for_activation(Revision("set-1".into())));
    let channel = EffectChannel::new(journal, second.attempt.clone(), call("c1"), tool(TOOL));
    let reply = channel.handle(look("upload").as_bytes()).await;
    assert_eq!(
        reply,
        format!(
            r#"{{"v":1,"id":"7","state":"held","effect":"{effect}","resolution":"not_executed","send":false}}"#
        )
    );
    let journal = Arc::new(db.store.for_activation(Revision("set-1".into())));
    let other = EffectChannel::new(
        journal,
        second.attempt.clone(),
        call("c1"),
        tool("agenda.sync"),
    );
    assert_eq!(
        other.handle(look("upload").as_bytes()).await,
        r#"{"v":1,"id":"7","error":"denied"}"#
    );
}

//! 단계 조회(`EffectJournal::lookup`)의 PG 종단 시험. 조회는 이 호출의 그 단계만 같은 인자 지문으로 읽고, 기록·감사·점유·리스 연장을 하나도 하지 않는다.
//! - 단계가 없으면 `None`, 지문이 다르면 `Conflict`, 호출이 없거나 다른 도구의 것이면 `Denied`, 옛 claim·취소된 run은 `Stale`.
//! - 보류 단계의 운영자 판정(미실행)은 반복 조회해도 소비되지 않고, 다음 `request`가 한 번만 점유하며 소비한다(실제 관리자 판정 경로).
//! - 실제 브로커 `EffectChannel`이 PG 위에서 내는 응답 모양.

mod support;

use bot_broker::effects::EffectChannel;
use bot_core::{admin::*, command::*, id::*, ledger::*, port::*, state::*};
use std::sync::Arc;
use support::{admin::*, *};

/// `begin_call`이 호출에 기록하는 도구.
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

/// 시작한 호출(c1)이 있는 실행 중 run.
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

/// 조회가 건드리면 안 되는 모든 것(감사·단계·호출·run의 상태와 리스)의 지문.
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
    // 단계가 없다
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
        // 보류 사유 코드는 조회의 영수증이 아니다
        ("held", &held, EffectState::Held, None),
    ] {
        for _ in 0..2 {
            let seen = lookup(&db, at, name, "{}").await.unwrap().expect(name);
            assert_eq!(seen.effect, *effect, "{name}");
            assert_eq!(seen.state, state, "{name}");
            assert_eq!(seen.receipt.as_deref(), receipt, "{name}");
            assert_eq!(seen.resolution, None, "{name}: 판정 전");
        }
    }
    assert_eq!(snapshot(&db).await, before, "조회는 아무것도 쓰지 않는다");
    // 판정은 보류 단계에만 있다: 보류가 아닌 단계 행에 남은 판정 값(손상)은 조회가 싣지 않는다
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
    // 새 세대가 읽어도 옛 세대가 시작한 단계는 started 그대로다(보류로 바꾸는 것은 `request`뿐이다)
    let seen = lookup(&db, &new.attempt, "upload", "{}")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((seen.effect, seen.state), (id, EffectState::Started));
    assert_eq!(snapshot(&db).await, before);
    // 옛 세대의 시도는 읽지 못한다
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
    // 키 순서·공백이 달라도 같은 지문이다
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
    // 다른 인자는 충돌이고, 다른 단계 이름은 «없음»이다
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
    // 읽을 수 없는 입력은 `Invalid`다
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
    // 다른 프로필에 같은 호출·단계 이름이 있어도 서로 보이지 않는다
    runs(&db)
        .accept(accept("work", "other-profile", "안녕"))
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
        "다른 프로필의 같은 이름의 단계는 보이지 않는다"
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
    // 다른 프로필의 attempt로 이 프로필의 run을 읽지 못한다
    let foreign = Attempt {
        profile: ProfileId("work".into()),
        ..at.clone()
    };
    assert!(matches!(
        lookup(&db, &foreign, "upload", r#"{"file":"summary"}"#).await,
        Err(Error::Denied)
    ));
    let before = snapshot(&db).await;
    // 기록되지 않은 호출·다른 도구의 호출은 `Denied`다(존재 여부를 알려 주지 않는다)
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
    // 모르는 run은 `Missing`
    let ghost = Attempt {
        run: RunId("no-such-run".into()),
        ..at.clone()
    };
    assert!(matches!(
        lookup(&db, &ghost, "upload", "{}").await,
        Err(Error::Missing)
    ));
    // 같은 세대라도 옛 claim은 `Stale`, 취소가 요청된 run도 `Stale`이다
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
    assert_ne!(before, bumped, "(대조: 위 SQL이 claim을 올렸다)");
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
    // 같은 run·호출·단계 ID를 다른 프로필이 쓰는 상황(행 복제로 만든다): 지문과 상태·영수증만 다르다
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
    // 각자 자기 프로필의 단계만 본다(같은 키여도 다른 행이다)
    let seen = lookup(&db, at, "upload", r#"{"file":"personal"}"#)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (seen.effect, seen.state, seen.receipt),
        (mine, EffectState::Started, None)
    );
    // 다른 프로필 쪽 행은 지문이 달라 이 프로필의 인자로는 «충돌»이고, 그쪽 영수증은 새지 않는다
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
    // 첫 단계는 확정 거절, 둘째는 결과 불명이다. 실행된 단계가 없어 미실행 판정 후 재개할 수 있다. 판정 전에는 보류 단계에 판정이 없다
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
    // 운영자가 실제 관리자 경로로 «실행 안 됨»을 판정한다
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:admin".into()),
        target,
        HoldVerdict::NotExecuted {
            evidence: "수신처에 없음".into(),
        },
    )
    .await
    .unwrap();
    let second = runs(&db)
        .claim()
        .await
        .unwrap()
        .expect("판정이 run을 깨운다");
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
            "반복 조회해도 판정은 소비되지 않는다"
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
    assert_eq!(snapshot(&db).await, before, "조회는 아무것도 쓰지 않는다");
    // 호출을 풀고 다시 시작한 현재 세대가 같은 단계를 요청하면 한 번만 점유하고 판정은 소비된다
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
    assert!(taken.send, "판정 뒤 첫 점유만 보낸다");
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
    assert!(!again.send, "같은 점유의 재요청은 보내지 않는다");
    assert_eq!(again.status.state, EffectState::Held);
    // 거절을 기록한 단계는 다시 보내지 않는다
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
    // 요청은 한 번만 보낸다
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
    // 도구가 불명으로 보고하면 보류다: 조회에는 보류 사유 코드가 아니라 상태만 나온다
    say(format!(
        r#"{{"v":1,"id":"2","method":"effect.status","effect":"{effect}","outcome":"held","reason":"timeout"}}"#
    ))
    .await;
    assert_eq!(
        say(look("upload")).await,
        format!(r#"{{"v":1,"id":"7","state":"held","effect":"{effect}","send":false}}"#)
    );
    // 효과 ID 양식의 기존 응답은 그대로다(PG는 보류 사유 코드를 영수증 칸이 아니라 따로 두므로 이 양식에도 영수증이 없다)
    assert_eq!(
        say(format!(
            r#"{{"v":1,"id":"5","method":"effect.status","effect":"{effect}"}}"#
        ))
        .await,
        r#"{"v":1,"id":"5","state":"held","send":false}"#
    );
    // 도구는 판정을 주입하지 못한다(어느 프레임으로도): 원장은 그대로다
    let injected = snapshot(&db).await;
    for frame in [
        r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{},"resolution":"not_executed"}"#.to_owned(),
        format!(r#"{{"v":1,"id":"1","method":"effect.status","effect":"{effect}","outcome":"held","reason":"x","resolution":"not_executed"}}"#),
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{},"resolution":"not_executed"}"#.to_owned(),
    ] {
        assert_eq!(say(frame).await, r#"{"v":1,"id":"1","error":"invalid"}"#);
    }
    assert_eq!(snapshot(&db).await, injected);
    // 운영자가 실제 관리자 경로로 미실행을 판정하면 조회가 그 판정을 싣는다
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
            evidence: "수신처에 없음".into(),
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
    // 다른 도구에 묶인 채널은 이 호출의 단계를 읽지 못한다
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

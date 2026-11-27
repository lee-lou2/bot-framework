//! 키로 이어지는 긴 요청 입구(OpenAI 호환 HTTP)로 접수된 실행의 승인 열기는 호스트 활성 집합 잠금 아래에서 열린다(PG).
//! - 브로커가 승인 경로(지정된 사람·전달 바인딩)를 확인한 집합이 **기록하는 같은 커밋에서도** 현재 집합이어야 한다: 교체가 먼저 커밋됐으면 옛 로컬 카드(승인 행)도
//!   옛 Slack 전달 작업도 0이다(`Stale`). 교체가 진행 중이면 그 커밋을 기다린 뒤 판정한다. 집합을 고정하지 않은 핸들은 비교할 대상이 없어 거절한다.
//! - 요청자는 접수 행이 인증한 주체, Slack DM으로 가는 카드는 접수 때 확인한 전달 설치(team)가 근거다. 올라간 설치가 다르거나 고정한 team이 없으면 영수증이 아니다.
//! - 본문·레거시(webhook) 실행의 승인은 예전처럼 열린다.

use crate::support::{api::*, *};
use bot_core::{
    api::{ApiState, ApprovalBlock},
    cards::{CardKey, CardOutbox, CardOutcome},
    id::*,
    ledger::{ApprovalState, CallLedger, CallPhase, Decision, OpenApproval},
    port::Error,
    state::*,
};
use sqlx::{Connection, Executor};
use std::time::Duration;

const APPROVER: &str = "slack:home/U1";

/// 접수한 API 실행이 호출 하나를 열었다(승인은 아직 열지 않았다).
async fn api_run(db: &TestDb, key: &str, team: Option<&str>) -> RunContext {
    let mut req = request(key, "build", "assistant", 1, 8);
    req.team = team.map(str::to_owned);
    accepted(admit(db, req).await);
    let ctx = claimed(db).await;
    begin_call(db, &ctx.attempt, "c1").await;
    ctx
}

/// 지정된 사람 승인. `requester`는 요청자 주체이고, `delivery`가 있으면 그 전달 바인딩의 Slack DM으로 간다.
fn open_as(
    ctx: &RunContext,
    nonce: &str,
    requester: &PrincipalId,
    delivery: Option<&str>,
) -> OpenApproval {
    let mut open = open_approval(Approval {
        requester: requester.clone(),
        approver: PrincipalId(APPROVER.into()),
        ..binding(&ctx.attempt, "c1", nonce)
    });
    open.delivery = delivery.map(|d| BindingId(d.into()));
    open
}

/// (승인 행, 전달 작업) 수.
async fn rows(db: &TestDb) -> (i64, i64) {
    (
        db.scalar("select count(*) from bot_approvals").await,
        db.scalar("select count(*) from bot_card_jobs").await,
    )
}

async fn phase_of(db: &TestDb, ctx: &RunContext) -> CallPhase {
    calls(db)
        .lookup(ctx.attempt.clone(), call("c1"))
        .await
        .unwrap()
        .unwrap()
        .phase
}

#[tokio::test]
async fn a_replacement_that_commits_first_leaves_neither_the_local_card_nor_the_slack_job() {
    for (delivery, jobs) in [(None, 0), (Some("team"), 1)] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let ctx = api_run(&db, "k1", Some("T1")).await;
        // 브로커가 확인한 집합은 set-1이고, 확인 직후 교체가 먼저 커밋됐다
        let handle = pinned(&db);
        db.sql("update bot_active set revision = 'set-2'").await;
        let got =
            CallLedger::open_approval(&handle, open_as(&ctx, "n1", &principal("build"), delivery))
                .await;
        assert!(matches!(got, Err(Error::Stale)), "{delivery:?}: {got:?}");
        assert_eq!(
            rows(&db).await,
            (0, 0),
            "{delivery:?}: 옛 카드도 작업도 없다"
        );
        assert!(matches!(
            phase_of(&db, &ctx).await,
            CallPhase::Open { approval: None }
        ));
        assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
        assert_eq!(
            lookup(&db, "k1", "build").await.unwrap().state,
            ApiState::Active
        );
        // 대조군: 같은 집합이면 각각 하나다(Slack은 전달 작업까지)
        db.sql("update bot_active set revision = 'set-1'").await;
        CallLedger::open_approval(&handle, open_as(&ctx, "n1", &principal("build"), delivery))
            .await
            .unwrap();
        assert_eq!(rows(&db).await, (1, jobs), "{delivery:?}");
        assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "waiting");
    }
}

/// 집합 교체가 진행 중인 동안(활성 집합 행 잠금을 쥔 별도 연결) 같은 핸들이 API 출처 승인을 열려 한다. `replace`면 교체가 커밋되고, 아니면 잠금만 놓는다.
/// `None`이면 DB가 없다.
async fn open_during_a_replacement(
    delivery: Option<&str>,
    replace: bool,
) -> Option<(Result<(), Error>, (i64, i64), String)> {
    let db = TestDb::new().await?;
    let ctx = api_run(&db, "k1", Some("T1")).await;
    let handle = pinned(&db);
    let open = open_as(&ctx, "n1", &principal("build"), delivery);
    let mut replacer = db.raw().await;
    let mut tx = replacer.begin().await.unwrap();
    tx.execute("select 1 from bot_active for update")
        .await
        .unwrap();
    let task = tokio::spawn(async move { CallLedger::open_approval(&handle, open).await });
    // 승인 열기가 그 잠금을 기다리는 순간까지(시간 추정 없이) 기다린다
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = db
            .scalar(&format!(
                "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_active%'",
                db.schema
            ))
            .await;
        if waiting == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "승인 열기가 집합 잠금을 기다리지 않는다"
        );
        assert!(!task.is_finished(), "잠금을 기다리지 않고 끝났다");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if replace {
        tx.execute("update bot_active set revision = 'set-2'")
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let result = task.await.unwrap();
    Some((
        result,
        rows(&db).await,
        state_of_run(&db, &ctx.attempt.run).await,
    ))
}

#[tokio::test]
async fn opening_waits_for_a_replacement_in_flight_and_judges_after_it() {
    for (delivery, jobs) in [(None, 0), (Some("team"), 1)] {
        // 교체가 커밋되면 그 커밋을 기다린 뒤 낡은 집합으로 판정해 열지 않는다
        let Some((result, counted, state)) = open_during_a_replacement(delivery, true).await else {
            return;
        };
        assert!(
            matches!(result, Err(Error::Stale)),
            "{delivery:?}: {result:?}"
        );
        assert_eq!(counted, (0, 0), "{delivery:?}");
        assert_eq!(state, "leased");
        // 대조군: 교체 없이 잠금만 쥐었다 놓으면 기다린 뒤 같은 집합으로 연다
        let (result, counted, state) = open_during_a_replacement(delivery, false)
            .await
            .expect("DB");
        result.unwrap();
        assert_eq!(counted, (1, jobs), "{delivery:?}");
        assert_eq!(state, "waiting");
    }
}

#[tokio::test]
async fn a_handle_that_pins_no_set_cannot_open_an_api_origin_approval_but_a_legacy_run_still_opens()
{
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    for delivery in [None, Some("team")] {
        let got = calls(&db)
            .open_approval(open_as(&ctx, "n1", &principal("build"), delivery))
            .await;
        assert!(matches!(got, Err(Error::Denied)), "{delivery:?}: {got:?}");
        assert_eq!(rows(&db).await, (0, 0), "{delivery:?}");
    }
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
    // 본문·레거시 실행은 예전처럼 연다: 고정하지 않은 핸들로도, 집합이 바뀐 뒤에도
    let legacy = leased(&db, "legacy").await;
    begin_call(&db, &legacy.attempt, "c9").await;
    db.sql("update bot_active set revision = 'set-2'").await;
    calls(&db)
        .open_approval(open_approval(binding(&legacy.attempt, "c9", "n9")))
        .await
        .unwrap();
    assert_eq!(rows(&db).await, (1, 0));
    assert_eq!(state_of_run(&db, &legacy.attempt.run).await, "waiting");
}

#[tokio::test]
async fn the_requester_and_the_delivery_team_come_from_the_original_request_row() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    // 요청자는 접수 행이 인증한 주체여야 한다: 브로커가 싣는 값이 다르면 로컬 카드도 Slack 카드도 만들지 않는다
    for requester in [principal("other"), PrincipalId("local:owner".into())] {
        for delivery in [None, Some("team")] {
            let got =
                CallLedger::open_approval(&pinned(&db), open_as(&ctx, "n1", &requester, delivery))
                    .await;
            assert!(
                matches!(got, Err(Error::Denied)),
                "{requester:?}/{delivery:?}: {got:?}"
            );
            assert_eq!(rows(&db).await, (0, 0));
        }
    }
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
    // 맞는 요청자면 열린다
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    assert_eq!(rows(&db).await, (1, 1));
    // 접수 때 확인한 전달 설치(team)가 없는 요청은 Slack 카드를 받지 못한다. 로컬 승인은 team과 무관하다
    let bare = api_run(&db, "k2", None).await;
    let got = CallLedger::open_approval(
        &pinned(&db),
        open_as(&bare, "n2", &principal("build"), Some("team")),
    )
    .await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(rows(&db).await, (1, 1), "두 번째 승인 행·작업이 없다");
    assert_eq!(state_of_run(&db, &bare.attempt.run).await, "leased");
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&bare, "n2", &principal("build"), None),
    )
    .await
    .unwrap();
    assert_eq!(rows(&db).await, (2, 1));
}

async fn decide_as(db: &TestDb, nonce: &str, actor: &str) -> Result<ApprovalState, Error> {
    calls(db)
        .decide(Decision {
            nonce: ApprovalId(nonce.into()),
            actor: PrincipalId(actor.into()),
            allow: true,
        })
        .await
}

fn posted(team: &str) -> CardOutcome {
    CardOutcome::Posted {
        team: team.into(),
        dm: "D1".into(),
        ts: "1.5".into(),
    }
}

/// 전달 작업을 점유하고 최초 전송자 확인까지 마친다(올리기 직전).
async fn sending(db: &TestDb, nonce: &str) -> CardKey {
    let job = CardOutbox::claim(&db.store, 8).await.unwrap().remove(0);
    assert_eq!(job.key.nonce.0, nonce);
    CardOutbox::begin(&db.store, job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    job.key
}

#[tokio::test]
async fn a_receipt_from_another_team_is_no_receipt_and_closes_the_card_as_a_changed_route() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    let key = sending(&db, "n1").await;
    // 같은 별칭의 실제 팀이 바뀌어 다른 설치로 올라갔다: 영수증·카드가 없다
    let got = CardOutbox::settle(&db.store, key.clone(), posted("T2")).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(
        db.scalar::<String>("select state || '|' || closed_reason from bot_card_jobs")
            .await,
        "closed|route_changed"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs where team is not null or dm is not null or ts is not null")
            .await,
        0,
        "영수증이 없다"
    );
    assert!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .delivery
            .is_none()
    );
    // 사람이 할 일이 아니고, 승인 경로가 어긋난 것으로 읽힌다. 지정된 사람의 클릭도 승인을 소비하지 못한다
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::ApprovalBlocked(ApprovalBlock::RouteChanged)
    );
    assert!(decide_as(&db, "n1", APPROVER).await.is_err());
    assert_eq!(
        db.scalar::<String>("select state || '|' || decided_by from bot_approvals")
            .await,
        "denied|system"
    );
    // 같은 정산의 재전달도 열지 못한다
    let again = CardOutbox::settle(&db.store, key.clone(), posted("T2")).await;
    assert!(matches!(again, Err(Error::Conflict)), "{again:?}");
    let right = CardOutbox::settle(&db.store, key, posted("T1")).await;
    assert!(matches!(right, Err(Error::Conflict)), "{right:?}");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);
}

#[tokio::test]
async fn a_card_job_without_the_pinned_team_never_becomes_a_receipt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    // 고정한 team이 없는 작업(옛 형태의 행): 올라간 team이 무엇이든 영수증이 아니다
    db.sql("update bot_card_jobs set origin_team = null").await;
    let key = sending(&db, "n1").await;
    let got = CardOutbox::settle(&db.store, key, posted("T1")).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(
        db.scalar::<String>("select state || '|' || closed_reason from bot_card_jobs")
            .await,
        "closed|route_changed"
    );
    assert!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .delivery
            .is_none()
    );
    assert_ne!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
}

#[tokio::test]
async fn the_pinned_team_matches_the_receipt_and_a_redelivered_settlement_is_the_same_result() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    let key = sending(&db, "n1").await;
    CardOutbox::settle(&db.store, key.clone(), posted("T1"))
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // 같은 정산의 재전달은 같은 결과다
    CardOutbox::settle(&db.store, key, posted("T1"))
        .await
        .unwrap();
    assert_eq!(
        decide_as(&db, "n1", APPROVER).await.unwrap(),
        ApprovalState::Approved
    );
}

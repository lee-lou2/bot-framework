//! 소유자 DM 승인 카드 전달 실패(PG): 소유자 DM을 열 수 없거나(`Unavailable`) 올라갔는지 알 수 없으면(`Held`, 120초를 넘긴 `sending`) 카드는 닫힌 채 자동으로
//! 다시 올리지 않고 `approval_dm_unavailable`로 닫는다. 같은 커밋에서 승인을 시스템 거절로 닫고 감사를 한 번만 남기며, 생성 승인은 기다리던 run을 깨우고(예약 0)
//! 전송 승인은 이번 발생만 미발송으로 끝낸다(반복 예약의 정의는 그대로). 성공 안내와 목적지 발송은 0건이고, 원래 채널·스레드에는 닫혔다는 안내 한 줄만 남는다
//! (올릴 수 없다고 확정했으면 «보낼 수 없어», 불명이면 «확인하지 못해»).

mod support;

use bot_core::cards::{CardOutcome, DM_UNAVAILABLE_NOTICE, DM_UNCONFIRMED_NOTICE};
use bot_core::{id::*, port::*, state::*};
use std::time::Duration;
use support::{cards::*, delivery::*, schedule::*, *};

const UNSENT: &str = r#"{"status":"approval_dm_unavailable"}"#;

/// 닫힌 뒤의 공통 사실: 승인은 시스템 거절이고 작업은 닫혔으며 감사는 한 번이다. 늦은 클릭·게시 확인은 아무것도 열지 못하고, 원래 채널에는 닫혔다는 안내가 한 줄뿐이다
/// (초안·수신자·해시·승인 식별자는 없다).
async fn closed_once(db: &TestDb, run: &RunContext, approval: &Approval, reason: &str) {
    let notice = if reason == "unavailable" {
        DM_UNAVAILABLE_NOTICE
    } else {
        DM_UNCONFIRMED_NOTICE
    };
    assert_eq!(
        public_lines(db, run).await,
        vec![serde_json::json!({ "text": notice }).to_string()]
    );
    assert_eq!(
        db.scalar::<String>("select state || '|' || decided_by from bot_approvals")
            .await,
        "denied|system"
    );
    assert_eq!(
        db.scalar::<String>("select state || '|' || closed_reason from bot_card_jobs")
            .await,
        format!("closed|{reason}")
    );
    assert_eq!(audits(db, "approval.dm_unavailable").await, 1);
    assert_eq!(
        db.scalar::<String>(
            "select target || '|' || verdict from bot_audit where action = 'approval.dm_unavailable'"
        )
        .await,
        format!("{}|approval_dm_unavailable", approval.nonce.0)
    );
    assert!(decide_as_owner(db, approval).await.is_err());
    assert!(matches!(
        settle(&db.store, &approval.nonce, posted()).await,
        Err(Error::Conflict)
    ));
    assert_eq!(receipt(db, &approval.nonce).await, None);
}

#[tokio::test]
async fn an_unavailable_dm_ends_the_send_as_unsent_once_and_keeps_the_recurring_schedule() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 반복 예약은 이번 발생이 닫혀도 그대로다
    let origin = run_in(&db, "r0", OWNER, ConversationKind::Direct).await;
    let schedule = make(&db, &origin, "c0", spec_cron("0 9 * * *", "Asia/Seoul"))
        .await
        .unwrap();
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    settle(&db.store, &job.nonce, CardOutcome::Unavailable)
        .await
        .unwrap();

    closed_once(&db, &run, &approval, "unavailable").await;
    // 이번 발생만 미발송으로 끝났다: 영수증은 한 번 만들어 run 결과·호출·초안에 같은 바이트로 남고 초안 본문은 지워졌다
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
    let result = format!(
        "select result from bot_runs where id = '{}'",
        run.attempt.run.0
    );
    assert_eq!(db.scalar::<String>(&result).await, UNSENT);
    assert_eq!(
        db.scalar::<String>(
            "select state || '|' || result from bot_calls where call = 'schedule-deliver'"
        )
        .await,
        format!("done|{UNSENT}")
    );
    assert_eq!(
        db.scalar::<String>(
            "select state || '|' || unsent || '|' || unsent_receipt from bot_deliveries"
        )
        .await,
        format!("dropped|dm_unavailable|{UNSENT}")
    );
    assert!(
        db.scalar::<bool>("select answers is null from bot_deliveries")
            .await
    );
    // 마지막 claim은 그 영수증을 다시 읽는다
    let receipt = replies(&db).receipt(run.attempt.clone()).await.unwrap();
    assert_eq!(receipt.map(|r| r.0), Some(UNSENT.to_owned()));
    // 성공 안내도 목적지 발송도 0건이다(원래 채널의 한 줄은 닫혔다는 안내뿐이다: `closed_once`가 확인했다)
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_outbox where conv_id = 'D9'")
            .await,
        0
    );
    assert_eq!(column::<String>(&db, &schedule, "state").await, "active");
    // 같은 정산의 재전달(응답을 잃고 다시 보낸 경우)은 같은 결과이고 감사·영수증이 늘지 않는다
    for again in [CardOutcome::Unavailable, CardOutcome::Held] {
        settle(&db.store, &job.nonce, again).await.unwrap();
    }
    assert_eq!(audits(&db, "approval.dm_unavailable").await, 1);
    assert_eq!(
        public_lines(&db, &run).await.len(),
        1,
        "안내도 한 번뿐이다(다른 결과로 다시 와도 처음 것만 남는다)"
    );
    assert_eq!(db.scalar::<String>(&result).await, UNSENT);
}

#[tokio::test]
async fn an_unavailable_dm_closes_a_create_card_without_a_schedule_and_wakes_the_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_create(&db, "r1", "n1").await;
    // 활성 집합을 고정한 핸들로 점유·확인·정산한다
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let job = claim(&pinned, 8).await.unwrap().remove(0);
    begin(&pinned, &job.nonce, rev()).await.unwrap();
    settle(&pinned, &job.nonce, CardOutcome::Held)
        .await
        .unwrap();

    closed_once(&db, &run, &approval, "held").await;
    // 예약은 생기지 않고, 기다리던 run은 같은 세대로 깨어난다(호출은 거절된 승인을 만나 끝난다)
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
}

#[tokio::test]
async fn a_post_with_an_unknown_outcome_is_closed_after_two_minutes_and_never_reposted() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    // 2분이 안 됐다: 그대로 기다린다
    db.sql("update bot_card_jobs set sending_ms = bot_now_ms() - 100000")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "sending");
    // 2분을 넘겼다: 올라갔는지 모르므로 닫고 다시 올리지 않는다
    db.sql("update bot_card_jobs set sending_ms = bot_now_ms() - 130000")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    closed_once(&db, &run, &approval, "held").await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
    // 늦게 도착한 게시 확인은 카드를 열지 못한다(`closed_once`가 확인했다). 자동 재게시도 없다
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(receipt(&db, &job.nonce).await, None);
}

#[tokio::test]
async fn transient_failures_back_off_and_the_sixth_attempt_closes_the_card() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    // 기본 백오프: 바로 다시 점유되지 않는다
    let job = sending(&db).await;
    settle(&db.store, &job.nonce, not_sent(None)).await.unwrap();
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "staged");
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    db.sql("update bot_card_jobs set not_before_ms = 0").await;
    // 다섯 번까지는 다시 점유한다(백오프 0): 점유할 때마다 횟수가 오른다
    for attempt in 2..=5 {
        let job = sending(&db).await;
        assert_eq!(
            db.scalar::<i32>("select attempts from bot_card_jobs").await,
            attempt
        );
        settle(&db.store, &job.nonce, not_sent(Some(Duration::ZERO)))
            .await
            .unwrap();
        assert_eq!(state(&db, "bot_card_jobs", "n1").await, "staged");
    }
    // 여섯 번째 점유에서 또 못 올리면 DM 경로가 없는 것으로 닫는다
    let job = sending(&db).await;
    settle(&db.store, &job.nonce, not_sent(Some(Duration::ZERO)))
        .await
        .unwrap();
    closed_once(&db, &run, &approval, "unavailable").await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
}

#[tokio::test]
async fn an_abandoned_lease_is_claimed_again_but_a_job_that_began_is_not() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    let first = claim(&db.store, 8).await.unwrap();
    assert_eq!(first.len(), 1);
    // begin 전에 전달자가 사라졌다: 리스가 끝나면 다른 전달자가 점유한다
    db.sql("update bot_card_jobs set lease_until_ms = bot_now_ms() - 1")
        .await;
    let second = claim(&db.store, 8).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(
        db.scalar::<i32>("select attempts from bot_card_jobs").await,
        2
    );
    begin(&db.store, &second[0].nonce, rev()).await.unwrap();
    // begin을 마친 작업은 리스와 무관하게 다시 점유되지 않는다(올라갔을 수 있다)
    db.sql("update bot_card_jobs set lease_until_ms = bot_now_ms() - 1")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "sending");
}

#[tokio::test]
async fn a_failure_report_for_a_card_that_is_no_longer_open_only_closes_the_job() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, _) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    // 올리는 사이 카드가 만료됐다: 실패를 알려 와도 승인·run을 건드리지 않고 감사도 남기지 않는다(만료는 만료 처리가 맡는다)
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    settle(&db.store, &job.nonce, CardOutcome::Unavailable)
        .await
        .unwrap();
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "closed");
    assert_eq!(
        db.scalar::<String>("select closed_reason from bot_card_jobs")
            .await,
        "stale"
    );
    assert_eq!(state(&db, "bot_approvals", "n1").await, "pending");
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(audits(&db, "approval.dm_unavailable").await, 0);
    // 이미 다른 사유로 끝난 요청이라 «닫았어요» 안내도 없다
    assert!(public_lines(&db, &run).await.is_empty());
}

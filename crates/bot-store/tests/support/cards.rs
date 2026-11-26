//! 소유자 DM 승인 카드 전달 작업(`bot_card_jobs`) 시험 도구.
#![allow(dead_code)]

use super::{admin::*, delivery::*, schedule::*, *};
use bot_core::{
    admin::{AdminRead, ApprovalFilter, ApprovalQuery, Page},
    cards::{CardJob, CardKey, CardOutbox, CardOutcome, CardReceipt},
    intake::NotSentReason,
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL, DeliveryTarget},
};
use bot_store::Store;
use std::time::Duration;

/// 프로필 `personal`의 활성 판본(`TestDb`가 게시한다).
pub fn rev() -> Revision {
    Revision("r1".into())
}

/// 게이트웨이가 확인해 준 DM 게시 영수증.
pub fn posted() -> CardOutcome {
    CardOutcome::Posted {
        team: "T1".into(),
        dm: "D77".into(),
        ts: "1700000000.000100".into(),
    }
}

pub fn not_sent(retry: Option<Duration>) -> CardOutcome {
    CardOutcome::NotSent {
        reason: NotSentReason::BeforeWrite,
        retry_after: retry,
    }
}

/// 채널 출처 run이 전송 초안 승인 카드를 열었다: 카드는 닫힌 채 소유자 DM 전달 작업이 쌓인다(공개 채널에는 아무것도 가지 않는다).
pub async fn channel_card(db: &TestDb, request: &str, nonce: &str) -> (RunContext, Approval) {
    let run = destination_run_as(db, request, ConversationKind::Channel).await;
    let draft = replies(db)
        .draft(run.attempt.clone(), Some(answers(&["하나", "둘"])))
        .await
        .unwrap()
        .expect("초안이 필요하다");
    let hash = draft.sha256().unwrap();
    calls(db)
        .begin(
            run.attempt.clone(),
            call(DELIVER_CALL),
            tool(DELIVER_TOOL),
            hash,
        )
        .await
        .unwrap();
    let approval = delivery_approval(&run.attempt, nonce, hash);
    calls(db)
        .open_approval(open_delivery(&approval, &draft.approval_json().unwrap()))
        .await
        .unwrap();
    (run, approval)
}

/// 채널 출처 run이 다른 수신자를 지정한 예약 생성 카드를 열었다(활성 집합을 고정한 핸들로만 열린다).
pub async fn channel_create(db: &TestDb, request: &str, nonce: &str) -> (RunContext, Approval) {
    let run = run_in(db, request, OWNER, ConversationKind::Channel).await;
    let (approval, open) = create_card(db, &run, nonce).await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    CallLedger::open_approval(&pinned, open).await.unwrap();
    (run, approval)
}

/// 예약 생성 카드(수신자를 지정한)와 그 승인. 호출은 이미 열었다.
pub async fn create_card(db: &TestDb, run: &RunContext, nonce: &str) -> (Approval, OpenApproval) {
    let mut spec = spec_in(db, 3_600_000).await;
    spec.destination = Some(DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: "T1/C9".into(),
            thread: None,
        },
    });
    let hash = sealed(&spec);
    begin_tool(db, run, "c1", "schedule.create", hash).await;
    let approval = Approval {
        tool: tool("schedule.create"),
        args_sha256: hash,
        ..binding(&run.attempt, "c1", nonce)
    };
    let open = open_create(approval.clone(), &spec);
    (approval, open)
}

pub async fn state(db: &TestDb, table: &str, nonce: &str) -> String {
    db.scalar::<String>(&format!(
        "select state from {table} where nonce = '{nonce}'"
    ))
    .await
}

/// 원래 대화(outbox)에 이 run이 남긴 줄들(순서대로).
pub async fn public_lines(db: &TestDb, run: &RunContext) -> Vec<String> {
    sqlx::query_scalar("select body from bot_outbox where run = $1 order by seq")
        .bind(&run.attempt.run.0)
        .fetch_all(db.store.pool())
        .await
        .unwrap()
}

pub async fn audits(db: &TestDb, action: &str) -> i64 {
    db.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

pub async fn decide_as_owner(db: &TestDb, approval: &Approval) -> Result<ApprovalState, Error> {
    calls(db)
        .decide(Decision {
            nonce: approval.nonce.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        })
        .await
}

/// 소유자가 관리 화면에서 보는 대기 승인의 nonce들.
pub async fn owner_pending(db: &TestDb) -> Vec<String> {
    AdminRead::approvals(
        &db.store,
        &owner(),
        ApprovalQuery {
            profile: None,
            filter: ApprovalFilter::Pending,
            page: Page {
                limit: 50,
                after: None,
            },
        },
    )
    .await
    .unwrap()
    .items
    .iter()
    .map(|a| a.nonce.0.clone())
    .collect()
}

/// 전달자가 쓰는 포트(`CardOutbox`)를 한 줄로.
pub async fn claim(store: &Store, limit: u32) -> Result<Vec<CardJob>, Error> {
    CardOutbox::claim(store, limit).await
}

pub async fn begin(store: &Store, key: &CardKey, revision: Revision) -> Result<(), Error> {
    CardOutbox::begin(store, key.clone(), revision).await
}

pub async fn settle(store: &Store, key: &CardKey, outcome: CardOutcome) -> Result<(), Error> {
    CardOutbox::settle(store, key.clone(), outcome).await
}

/// 게시를 확인한 영수증(`StoredApproval::delivery`). 확정 전이면 `None`.
pub async fn receipt(db: &TestDb, nonce: &ApprovalId) -> Option<CardReceipt> {
    calls(db).approval(nonce.clone()).await.unwrap().delivery
}

/// 점유하고 최초 전송자 확인까지 마친 작업.
pub async fn sending(db: &TestDb) -> CardJob {
    let mut jobs = claim(&db.store, 8).await.unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = jobs.remove(0);
    begin(&db.store, &job.key, rev()).await.unwrap();
    job
}

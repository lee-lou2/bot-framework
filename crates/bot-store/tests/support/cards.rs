//! Test helpers for owner-DM approval card delivery jobs (`bot_card_jobs`).
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

pub fn rev() -> Revision {
    Revision("r1".into())
}

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

/// Opens a scheduled delivery approval card from a public channel (stages an owner-DM card job, posting nothing publicly).
pub async fn channel_card(db: &TestDb, request: &str, nonce: &str) -> (RunContext, Approval) {
    let run = destination_run_as(db, request, ConversationKind::Channel).await;
    let draft = replies(db)
        .draft(run.attempt.clone(), Some(answers(&["one", "two"])))
        .await
        .unwrap()
        .expect("draft required");
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

/// Opens an explicit-destination `schedule.create` approval card from a channel using a set-pinned handle.
pub async fn channel_create(db: &TestDb, request: &str, nonce: &str) -> (RunContext, Approval) {
    let run = run_in(db, request, OWNER, ConversationKind::Channel).await;
    let (approval, open) = create_card(db, &run, nonce).await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    CallLedger::open_approval(&pinned, open).await.unwrap();
    (run, approval)
}

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

pub async fn claim(store: &Store, limit: u32) -> Result<Vec<CardJob>, Error> {
    CardOutbox::claim(store, limit).await
}

pub async fn begin(store: &Store, key: &CardKey, revision: Revision) -> Result<(), Error> {
    CardOutbox::begin(store, key.clone(), revision).await
}

pub async fn settle(store: &Store, key: &CardKey, outcome: CardOutcome) -> Result<(), Error> {
    CardOutbox::settle(store, key.clone(), outcome).await
}

pub async fn receipt(db: &TestDb, nonce: &ApprovalId) -> Option<CardReceipt> {
    calls(db).approval(nonce.clone()).await.unwrap().delivery
}

pub async fn sending(db: &TestDb) -> CardJob {
    let mut jobs = claim(&db.store, 8).await.unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = jobs.remove(0);
    begin(&db.store, &job.key, rev()).await.unwrap();
    job
}

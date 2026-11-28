//! OpenAI-compatible HTTP run approval cards: staged closed for owner Slack DM, preserving admission evidence and route-block verdicts.

use crate::support::{api::*, *};
use bot_core::{
    api::{ApiState, ApprovalBlock},
    cards::{CardOrigin, CardOutbox, CardOutcome},
    command::Delegate,
    id::*,
    ledger::{ApprovalState, CallLedger, Decision},
    port::{DelegationStore, Error},
    state::*,
};

const APPROVER: &str = "slack:home/U1";

async fn waiting(
    db: &TestDb,
    key: &str,
    nonce: &str,
    team: Option<&str>,
) -> (RunContext, Approval) {
    let mut req = request(key, "build", "assistant", 1, 8);
    req.team = team.map(str::to_owned);
    accepted(admit(db, req).await);
    let ctx = claimed(db).await;
    let approval = open_for(db, &ctx, nonce, Some("team")).await;
    (ctx, approval)
}

async fn open_for(db: &TestDb, ctx: &RunContext, nonce: &str, delivery: Option<&str>) -> Approval {
    let call_id = format!("c-{nonce}");
    begin_call(db, &ctx.attempt, &call_id).await;
    let approval = Approval {
        approver: PrincipalId(APPROVER.into()),
        ..api_approval(&ctx.attempt, &call_id, nonce, "build")
    };
    let mut open = open_approval(approval.clone());
    open.delivery = delivery.map(|d| BindingId(d.into()));
    CallLedger::open_approval(&pinned(db), open).await.unwrap();
    approval
}

fn card(db: &TestDb) -> &dyn CardOutbox {
    &db.store
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

async fn outbox_rows(db: &TestDb) -> i64 {
    db.scalar("select count(*) from bot_outbox").await
}

#[tokio::test]
async fn an_api_origin_card_is_staged_closed_keeps_the_original_evidence_and_posts_no_public_line()
{
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (ctx, approval) = waiting(&db, "k1", "n1", Some("T1")).await;
    assert_eq!(
        db.scalar::<String>(
            "select binding || '|' || owner || '|' || origin_binding || '|' || requester || '|' || revision || '|' || origin_team || '|' || state from bot_card_jobs"
        )
        .await,
        format!("team|{APPROVER}|b-assistant|http:openai/build|r1|T1|staged")
    );
    assert_eq!(
        card(&db).origin(approval.nonce.clone()).await.unwrap(),
        Some(CardOrigin {
            binding: BindingId("b-assistant".into()),
            requester: principal("build"),
            revision: Revision("r1".into()),
            team: Some("T1".into()),
        })
    );
    assert_eq!(outbox_rows(&db).await, 0);
    // Card stays closed until a Posted receipt is recorded.
    assert!(matches!(
        decide_as(&db, "n1", APPROVER).await,
        Err(Error::Conflict)
    ));
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "waiting");
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    let job = card(&db).claim(8).await.unwrap().remove(0);
    assert_eq!(
        (job.binding.0.as_str(), job.owner.0.as_str()),
        ("team", APPROVER)
    );
    assert_eq!(job.source, None);
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db)
        .settle(
            job.key.clone(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        calls(&db)
            .approval(job.key.nonce.clone())
            .await
            .unwrap()
            .delivery
            .unwrap()
            .source,
        None
    );
    assert_eq!(
        outbox_rows(&db).await,
        0,
        "no public notice for HTTP-origin runs"
    );
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    let receipt = calls(&db)
        .approval(approval.nonce.clone())
        .await
        .unwrap()
        .delivery;
    let receipt = receipt.expect("delivery receipt");
    assert_eq!(
        (
            receipt.team.as_str(),
            receipt.owner.0.as_str(),
            receipt.binding.0.as_str()
        ),
        ("T1", APPROVER, "team")
    );
    assert!(matches!(
        decide_as(&db, "n1", "http:openai/build").await,
        Err(Error::Denied)
    ));
    assert_eq!(
        decide_as(&db, "n1", APPROVER).await.unwrap(),
        ApprovalState::Approved
    );
}

#[tokio::test]
async fn a_delivery_binding_without_an_authenticated_api_root_is_refused_and_records_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Legacy webhook/CLI runs are not attested by the API admission ledger and cannot use a delivery binding.
    let ctx = leased(&db, "legacy").await;
    begin_call(&db, &ctx.attempt, "c1").await;
    let mut open = open_approval(Approval {
        requester: principal("build"),
        approver: PrincipalId(APPROVER.into()),
        ..binding(&ctx.attempt, "c1", "n1")
    });
    open.delivery = Some(BindingId("team".into()));
    let got = calls(&db).open_approval(open).await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
    // API approvals without a delivery binding use the local path and open immediately.
    let (local, _) = {
        let req = request("k2", "build", "assistant", 2, 8);
        accepted(admit(&db, req).await);
        let local = claimed(&db).await;
        let approval = open_for(&db, &local, "n2", None).await;
        (local, approval)
    };
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(
        lookup(&db, "k2", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    assert_eq!(state_of_run(&db, &local.attempt.run).await, "waiting");
}

#[tokio::test]
async fn a_route_verdict_closes_the_card_before_posting_and_the_lookup_reads_the_stored_word() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (n, why) in [
        ApprovalBlock::NoApprover,
        ApprovalBlock::ApproverRevoked,
        ApprovalBlock::RouteChanged,
    ]
    .into_iter()
    .enumerate()
    {
        let (key, nonce) = (format!("k{n}"), format!("n{n}"));
        let (ctx, _) = waiting(&db, &key, &nonce, Some("T1")).await;
        let job = card(&db).claim(8).await.unwrap().remove(0);
        assert_eq!(job.key.nonce.0, nonce);
        card(&db).block(job.key.clone(), why).await.unwrap();
        assert_eq!(
            db.scalar::<String>(&format!(
                "select state || '|' || closed_reason from bot_card_jobs where nonce = '{nonce}'"
            ))
            .await,
            format!("closed|{}", why.code())
        );
        assert_eq!(
            db.scalar::<String>(&format!(
                "select state || '|' || decided_by from bot_approvals where nonce = '{nonce}'"
            ))
            .await,
            "denied|system"
        );
        assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "ready");
        assert_eq!(
            lookup(&db, &key, "build").await.unwrap().state,
            ApiState::ApprovalBlocked(why),
            "{}",
            why.code()
        );
        // Replaying the same block verdict is idempotent and cannot be reopened via Posted.
        card(&db).block(job.key.clone(), why).await.unwrap();
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select count(*) from bot_audit where action = 'approval.blocked' and target = '{nonce}'"
            ))
            .await,
            1
        );
        let posted = card(&db)
            .settle(
                job.key.clone(),
                CardOutcome::Posted {
                    team: "T1".into(),
                    dm: "D1".into(),
                    ts: "2.5".into(),
                },
            )
            .await;
        assert!(matches!(posted, Err(Error::Conflict)), "{posted:?}");
        db.sql(&format!(
            "update bot_runs set state = 'succeeded' where id = '{}'",
            ctx.attempt.run.0
        ))
        .await;
    }
    assert_eq!(outbox_rows(&db).await, 0);
}

#[tokio::test]
async fn an_opened_card_cannot_be_blocked_later_and_a_dm_failure_is_the_dm_verdict() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    waiting(&db, "k1", "n1", Some("T1")).await;
    let job = card(&db).claim(8).await.unwrap().remove(0);
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db)
        .settle(
            job.key.clone(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
    let late = card(&db).block(job.key, ApprovalBlock::RouteChanged).await;
    assert!(matches!(late, Err(Error::Conflict)), "{late:?}");
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    let (_, _) = waiting(&db, "k2", "n2", Some("T1")).await;
    let job = card(&db).claim(8).await.unwrap().remove(0);
    assert_eq!(job.key.nonce.0, "n2");
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db).settle(job.key, CardOutcome::Held).await.unwrap();
    assert_eq!(
        lookup(&db, "k2", "build").await.unwrap().state,
        ApiState::ApprovalBlocked(ApprovalBlock::ApprovalDmUnavailable)
    );
    assert_eq!(outbox_rows(&db).await, 0);
}

#[tokio::test]
async fn a_delegated_child_keeps_the_roots_origin_binding_and_delivery_team() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut req = request("k1", "build", "assistant", 1, 8);
    req.team = Some("T9".into());
    let root = accepted(admit(&db, req).await);
    let parent = claimed(&db).await;
    db.store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            checked_revision: Revision("r1".into()),
            step: "research".into(),
            child: AgentId("main/research".into()),
            instruction: "investigate".into(),
            scopes: scopes(&["run"]),
        })
        .await
        .unwrap();
    let child = claimed(&db).await;
    assert_ne!(child.attempt.run, root.run);
    assert_eq!(child.binding.0, "b-assistant");
    assert_eq!(child.root, root.run);
    assert_eq!(
        child
            .api
            .as_ref()
            .map(|a| (a.endpoint.as_str(), a.model.as_str())),
        Some(("openai", "assistant"))
    );
    open_for(&db, &child, "n1", Some("team")).await;
    assert_eq!(
        card(&db).origin(ApprovalId("n1".into())).await.unwrap(),
        Some(CardOrigin {
            binding: BindingId("b-assistant".into()),
            requester: principal("build"),
            revision: Revision("r1".into()),
            team: Some("T9".into()),
        })
    );
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    let job = card(&db).claim(8).await.unwrap().remove(0);
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db)
        .settle(
            job.key,
            CardOutcome::Posted {
                team: "T9".into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    assert_eq!(requests(&db).await, 1);
}

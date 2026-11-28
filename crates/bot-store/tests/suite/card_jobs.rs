//! Owner-DM approval card delivery jobs (PG): Slack channel-origin approvals stay closed
//! until a confirmed DM receipt opens them and posts a single fixed notice in the origin channel.

use crate::support::{admin::*, cards::*, delivery::*, *};
use bot_core::{
    admin::AdminRead,
    cards::{CardKey, CardOutcome, CardSource, DM_SENT_NOTICE, DM_UNAVAILABLE_NOTICE},
    command::{AcceptExtras, Delegate},
    id::*,
    ledger::*,
    port::*,
    slack::{SlackSource, SourceKind},
    state::*,
};
use std::collections::BTreeSet;

#[tokio::test]
async fn a_channel_card_waits_closed_and_stays_off_the_public_channel() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    assert_eq!(public_lines(&db, &run).await, Vec::<String>::new());
    assert_eq!(
        (
            state(&db, "bot_approvals", "n1").await,
            state(&db, "bot_card_jobs", "n1").await
        ),
        ("pending".to_owned(), "staged".to_owned())
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(
        db.scalar::<String>("select profile || '|' || binding || '|' || owner || '|' || generation from bot_card_jobs")
            .await,
        format!("personal|desk|{OWNER}|{}", run.attempt.generation.0)
    );
    // Unopened cards cannot be decided and stay hidden from admin pending counts.
    let got = decide_as_owner(&db, &approval).await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    assert_eq!(state(&db, "bot_approvals", "n1").await, "pending");
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(owner_pending(&db).await, Vec::<String>::new());
    let status = AdminRead::status(&db.store, &operator()).await.unwrap();
    assert!(
        status.iter().all(|p| p.approvals_pending == 0),
        "{status:?}"
    );
}

#[tokio::test]
async fn a_plain_slack_channel_approval_uses_owner_dm_while_direct_dm_stays_inline() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let channel = run_in(&db, "r2", OWNER, ConversationKind::Channel).await;
    begin_call(&db, &channel.attempt, "d1").await;
    let plain = Approval {
        tool: tool("web.search"),
        ..binding(&channel.attempt, "d1", "n2")
    };
    calls(&db)
        .open_approval(open_approval(plain.clone()))
        .await
        .unwrap();
    assert!(public_lines(&db, &channel).await.is_empty());
    assert_eq!(state(&db, "bot_card_jobs", "n2").await, "staged");
    assert!(matches!(
        decide_as_owner(&db, &plain).await,
        Err(Error::Conflict)
    ));
    let job = sending(&db).await;
    assert_eq!(job.key.nonce.0, "n2");
    settle(&db.store, &job.key, posted()).await.unwrap();
    assert_eq!(
        public_lines(&db, &channel).await,
        vec![serde_json::json!({"text": DM_SENT_NOTICE}).to_string()]
    );
    assert_eq!(
        decide_as_owner(&db, &plain).await.unwrap(),
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &channel.attempt.run).await, "ready");

    // A 1:1 DM conversation already runs in owner DM and posts its card inline.
    let db = TestDb::new().await.unwrap();
    let direct = destination_run_as(&db, "r1", ConversationKind::Direct).await;
    let (_, approval) = request_delivery(&db, &direct, answers(&["one"])).await;
    assert_eq!(public_lines(&db, &direct).await.len(), 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert!(decide_as_owner(&db, &approval).await.is_ok());
}

#[tokio::test]
async fn a_delegated_slack_channel_child_routes_general_tool_approvals_to_owner_dm() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let parent = run_in(&db, "r-parent", OWNER, ConversationKind::Channel).await;
    let child_id = db
        .store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            step: "media".into(),
            child: AgentId("main/media".into()),
            instruction: "generate image".into(),
            scopes: scopes(&["run"]),
            checked_revision: rev(),
        })
        .await
        .unwrap();
    let child = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(child.attempt.run, child_id);
    begin_call(&db, &child.attempt, "c1").await;
    let approval = Approval {
        tool: tool("shuut.generate-image"),
        risk: Risk::Irreversible,
        ..binding(&child.attempt, "c1", "n-child")
    };
    calls(&db)
        .open_approval(open_approval(approval))
        .await
        .unwrap();
    assert!(public_lines(&db, &child).await.is_empty());
    assert_eq!(state(&db, "bot_card_jobs", "n-child").await, "staged");

    // DM delivery failure closes the approval, wakes the child run, and posts one fixed channel notice.
    let job = sending(&db).await;
    settle(&db.store, &job.key, CardOutcome::Unavailable)
        .await
        .unwrap();
    assert_eq!(state(&db, "bot_approvals", "n-child").await, "denied");
    assert_eq!(state_of_run(&db, &child.attempt.run).await, "ready");
    assert_eq!(
        public_lines(&db, &child).await,
        vec![serde_json::json!({"text": DM_UNAVAILABLE_NOTICE}).to_string()]
    );
}

#[tokio::test]
async fn a_confirmed_receipt_opens_the_card_and_adds_exactly_one_public_line() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let jobs = claim(&db.store, 8).await.unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(
        (
            job.key.nonce.0.as_str(),
            job.owner.0.as_str(),
            job.profile.0.as_str(),
            job.binding.0.as_str()
        ),
        ("n1", OWNER, "personal", "desk")
    );
    let card: serde_json::Value = serde_json::from_str(&job.card.0).unwrap();
    assert_eq!(
        (card["nonce"].as_str(), card["title"].as_str()),
        (Some("n1"), Some("Scheduled delivery approval"))
    );
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    let early = settle(&db.store, &job.key, posted()).await;
    assert!(matches!(early, Err(Error::Conflict)), "{early:?}");
    begin(&db.store, &job.key, rev()).await.unwrap();
    let again = begin(&db.store, &job.key, rev()).await;
    assert!(matches!(again, Err(Error::Conflict)), "{again:?}");
    assert!(matches!(
        decide_as_owner(&db, &approval).await,
        Err(Error::Conflict)
    ));
    assert_eq!(receipt(&db, &job.key.nonce).await, None);
    assert!(public_lines(&db, &run).await.is_empty());

    settle(&db.store, &job.key, posted()).await.unwrap();
    let receipt = receipt(&db, &job.key.nonce).await.expect("receipt");
    assert_eq!(
        (
            receipt.team.as_str(),
            receipt.dm.as_str(),
            receipt.ts.as_str(),
            receipt.owner.0.as_str(),
            receipt.profile.0.as_str(),
            receipt.binding.0.as_str()
        ),
        ("T1", "D77", "1700000000.000100", OWNER, "personal", "desk")
    );
    assert_eq!(
        public_lines(&db, &run).await,
        vec![serde_json::json!({"text": DM_SENT_NOTICE}).to_string()]
    );
    // Identical settlement is idempotent; conflicting receipt or late failure is rejected.
    settle(&db.store, &job.key, posted()).await.unwrap();
    assert_eq!(public_lines(&db, &run).await.len(), 1);
    let other = bot_core::cards::CardOutcome::Posted {
        team: "T1".into(),
        dm: "D77".into(),
        ts: "9.9".into(),
    };
    assert!(matches!(
        settle(&db.store, &job.key, other).await,
        Err(Error::Conflict)
    ));
    for late in [
        bot_core::cards::CardOutcome::Unavailable,
        bot_core::cards::CardOutcome::Held,
    ] {
        let got = settle(&db.store, &job.key, late).await;
        assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    }
    assert_eq!(owner_pending(&db).await, vec!["n1".to_owned()]);
    assert_eq!(
        decide_as_owner(&db, &approval).await.unwrap(),
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_claimers_never_share_a_job_and_only_one_begin_passes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for n in 1..=4 {
        channel_card(&db, &format!("r{n}"), &format!("n{n}")).await;
    }
    let (a, b) = (db.peer("a").await, db.peer("b").await);
    let (first, second) = tokio::join!(claim(&a, 3), claim(&b, 3));
    let (first, second) = (first.unwrap(), second.unwrap());
    let seen: BTreeSet<String> = first
        .iter()
        .chain(&second)
        .map(|j| j.key.nonce.0.clone())
        .collect();
    assert_eq!(
        first.len() + second.len(),
        seen.len(),
        "job claimed by both peers"
    );
    assert_eq!(seen.len(), 4, "each job claimed once: {seen:?}");
    let key = &first[0].key;
    let (x, y) = tokio::join!(begin(&a, key, rev()), begin(&b, key, rev()));
    assert_eq!(
        [x.is_ok(), y.is_ok()].iter().filter(|ok| **ok).count(),
        1,
        "{x:?} {y:?}"
    );
}

#[tokio::test]
async fn a_card_that_went_stale_while_posting_is_never_opened() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    let got = settle(&db.store, &job.key, posted()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "closed");
    assert_eq!(
        db.scalar::<String>("select closed_reason from bot_card_jobs")
            .await,
        "stale"
    );
    assert!(public_lines(&db, &run).await.is_empty());
    assert_eq!(receipt(&db, &job.key.nonce).await, None);
    assert!(matches!(
        decide_as_owner(&db, &approval).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn an_expired_or_replaced_card_is_never_opened_and_begin_checks_the_profile_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    let jobs = claim(&db.store, 8).await.unwrap();
    let got = begin(&db.store, &jobs[0].key, Revision("r9".into())).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "leased");
    begin(&db.store, &jobs[0].key, rev()).await.unwrap();
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    let got = settle(&db.store, &jobs[0].key, posted()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "closed");
}

#[tokio::test]
async fn claim_closes_cards_that_are_no_longer_pending_and_skips_them() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(
        db.scalar::<String>("select closed_reason from bot_card_jobs")
            .await,
        "stale"
    );
    assert_eq!(audits(&db, "approval.dm_unavailable").await, 0);
}

#[tokio::test]
async fn a_channel_card_whose_approver_is_not_the_requester_opens_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Channel).await;
    let (approval, open) = create_card(&db, &run, "n1").await;
    let other = Approval {
        approver: PrincipalId(GUEST.into()),
        ..approval
    };
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let got = CallLedger::open_approval(
        &pinned,
        bot_core::ledger::OpenApproval {
            approval: other,
            ..open
        },
    )
    .await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");

    // Plain Slack channel tool approvals enforce the same requester == approver invariant.
    let run2 = run_in(&db, "r2", OWNER, ConversationKind::Channel).await;
    begin_call(&db, &run2.attempt, "c2").await;
    let mismatched = Approval {
        approver: PrincipalId(GUEST.into()),
        ..binding(&run2.attempt, "c2", "n2")
    };
    let got = calls(&db).open_approval(open_approval(mismatched)).await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(state_of_run(&db, &run2.attempt.run).await, "leased");
}

#[tokio::test]
async fn begin_refuses_a_card_that_expired_after_the_claim() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    let job = claim(&db.store, 8).await.unwrap().remove(0);
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    let got = begin(&db.store, &job.key, rev()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "leased");
    let key = CardKey {
        nonce: ApprovalId("nope".into()),
        attempt: 1,
    };
    let none = begin(&db.store, &key, rev()).await;
    assert!(matches!(none, Err(Error::Missing)), "{none:?}");
}

#[tokio::test]
async fn claimed_jobs_and_receipts_carry_slack_channel_thread_source_for_root_and_delegated_children()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut cmd = accept_as(
        "personal",
        "slack-reply",
        OWNER,
        ConversationKind::Channel,
        "C12345678",
        "run tool",
    );
    cmd.event.conversation.thread = Some("1700000000.000100".into());
    let extras = AcceptExtras {
        source: Some(SlackSource {
            kind: SourceKind::Message,
            team: "T1".into(),
            channel: "C12345678".into(),
            ts: "1700000000.000250".into(),
            thread_ts: Some("1700000000.000100".into()),
            event_id: "Ev1".into(),
            reaction: None,
        }),
        chain: None,
    };
    db.store.accept_with(cmd, extras).await.unwrap();
    let parent = runs(&db).claim().await.unwrap().unwrap();
    let child_id = db
        .store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            step: "worker".into(),
            child: AgentId("main/worker".into()),
            instruction: "execute".into(),
            scopes: scopes(&["run"]),
            checked_revision: rev(),
        })
        .await
        .unwrap();
    let child = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(child.attempt.run, child_id);
    begin_call(&db, &child.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&child.attempt, "c1", "n-src")))
        .await
        .unwrap();

    let expected = Some(CardSource {
        channel: "C12345678".into(),
        thread_ts: Some("1700000000.000100".into()),
        message_ts: Some("1700000000.000250".into()),
    });
    let job = sending(&db).await;
    assert_eq!(job.source, expected);
    settle(&db.store, &job.key, posted()).await.unwrap();
    let got_receipt = receipt(&db, &job.key.nonce).await.expect("receipt");
    assert_eq!(got_receipt.source, expected);
}

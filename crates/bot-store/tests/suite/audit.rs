use crate::support::*;
use bot_core::{command::*, id::*, port::*};

async fn some_traffic(db: &TestDb, n: usize) {
    for i in 0..n {
        db.store
            .accept(accept("personal", &format!("r{i}"), "hi"))
            .await
            .unwrap();
    }
    let got = db.store.claim().await.unwrap().unwrap();
    db.store.finish(got.attempt, done("ok")).await.unwrap();
}

fn profile() -> ProfileId {
    ProfileId("personal".into())
}

#[tokio::test]
async fn audit_chain_links_every_state_change() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let initial = db.store.verify().await.unwrap().entries;
    some_traffic(&db, 3).await;
    let report = db.store.verify().await.unwrap();
    assert_eq!(
        (report.entries, report.first_bad),
        (initial + 4, None),
        "3 accepts + 1 completion"
    );
}

#[tokio::test]
async fn audit_chain_detects_content_tampering() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    db.sql("update bot_audit set verdict = 'forged' where seq = 2")
        .await;
    assert_eq!(db.store.verify().await.unwrap().first_bad, Some(2));
}

#[tokio::test]
async fn audit_chain_detects_a_rewritten_previous_hash_column_even_if_the_row_hash_is_intact() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    // Rewriting `prev_hash` while leaving row content and `hash` intact must still be caught.
    db.sql("update bot_audit set prev_hash = decode(repeat('ab', 32), 'hex') where seq = 3")
        .await;
    assert_eq!(db.store.verify().await.unwrap().first_bad, Some(3));
}

#[tokio::test]
async fn audit_chain_detects_removed_rows_and_truncated_tail() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    let total = db.store.verify().await.unwrap().entries;
    db.sql("delete from bot_audit where seq = 2").await;
    assert_eq!(
        db.store.verify().await.unwrap().first_bad,
        Some(2),
        "sequence gap"
    );

    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    db.sql(&format!("delete from bot_audit where seq = {total}"))
        .await;
    assert_eq!(
        db.store.verify().await.unwrap().first_bad,
        Some(total),
        "tail deletion mismatches head"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_changes_keep_one_contiguous_chain() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let initial = db.store.verify().await.unwrap().entries;
    let mut tasks = Vec::new();
    for n in 0..16 {
        let store = db.peer(&format!("w{n}")).await;
        tasks.push(tokio::spawn(async move {
            store
                .accept(accept("personal", &format!("r{n}"), "hi"))
                .await
                .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let report = db.store.verify().await.unwrap();
    assert_eq!((report.entries, report.first_bad), (initial + 16, None));
}

#[tokio::test]
async fn standalone_append_extends_the_same_chain() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 1).await;
    let before = db.store.verify().await.unwrap().entries;
    let entry = AuditEntry {
        profile: profile(),
        actor: "broker".into(),
        action: "call.denied".into(),
        target: "c-1".into(),
        args_sha256: Some([7; 32]),
        verdict: "scope".into(),
    };
    assert_eq!(db.store.append(entry).await.unwrap(), before + 1);
    let report = db.store.verify().await.unwrap();
    assert_eq!((report.entries, report.first_bad), (before + 1, None));
}

/// Repeatedly verifies the chain under concurrent appends; reading rows and head in a single
/// `REPEATABLE READ` snapshot prevents false-positive tampering reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verifying_while_entries_are_appended_never_reports_tampering_that_did_not_happen() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 30).await;
    let writer_store = db.peer("appender").await;
    let (stop, appended) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
    );
    let writer = {
        let (stop, appended) = (stop.clone(), appended.clone());
        tokio::spawn(async move {
            // Pace writes slower than verification so the test terminates while still overlapping reads.
            while !stop.load(Ordering::SeqCst) && appended.load(Ordering::SeqCst) < 1500 {
                writer_store
                    .append(AuditEntry {
                        profile: profile(),
                        actor: "system".into(),
                        action: "test.append".into(),
                        target: format!("t{}", appended.load(Ordering::SeqCst)),
                        args_sha256: None,
                        verdict: "ok".into(),
                    })
                    .await
                    .unwrap();
                appended.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
    };
    let mut false_alarms = Vec::new();
    let mut round = 0;
    while round < 250 || !writer.is_finished() {
        let report = db.store.verify().await.unwrap();
        if let Some(bad) = report.first_bad {
            false_alarms.push((round, bad, report.entries));
        }
        round += 1;
    }
    stop.store(true, Ordering::SeqCst);
    writer.await.unwrap();
    assert!(
        appended.load(Ordering::SeqCst) > 500,
        "entries must be appended during verification"
    );
    assert!(
        false_alarms.is_empty(),
        "false tampering reported {} times (round, seq, entries): {:?}",
        false_alarms.len(),
        &false_alarms[..false_alarms.len().min(5)]
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampering_is_still_found_while_entries_are_appended() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 10).await;
    db.sql("update bot_audit set verdict = 'forged' where seq = 3")
        .await;
    let writer_store = db.peer("appender").await;
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut count = 0;
            while !stop.load(Ordering::SeqCst) && count < 500 {
                writer_store
                    .append(AuditEntry {
                        profile: profile(),
                        actor: "system".into(),
                        action: "test.append".into(),
                        target: "t".into(),
                        args_sha256: None,
                        verdict: "ok".into(),
                    })
                    .await
                    .unwrap();
                count += 1;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
    };
    for _ in 0..50 {
        assert_eq!(db.store.verify().await.unwrap().first_bad, Some(3));
    }
    stop.store(true, Ordering::SeqCst);
    writer.await.unwrap();
}

#[tokio::test]
async fn batched_commit_tail_audits_preserve_valid_chain_across_multi_entry_paths() {
    use bot_core::{
        intake::{DeliveryScope, PushTransportId},
        ledger::*,
        profile::ActiveRevisions,
        state::ConversationKind,
    };
    use std::collections::BTreeMap;
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 1) outbox_claim: hold 2 expired sending items and claim 1 pending item in one commit.
    for req in ["ob-1", "ob-2", "ob-3"] {
        let run = leased_in(&db, req, ConversationKind::Direct).await;
        runs(&db).finish(run.attempt, done("reply")).await.unwrap();
    }
    let scope = DeliveryScope::all(PushTransportId::SLACK);
    let two = outbox(&db).claim(&scope, 2).await.unwrap();
    assert_eq!(two.len(), 2);
    for item in &two {
        outbox(&db)
            .begin(item.key.clone(), Revision("r1".into()))
            .await
            .unwrap();
    }
    db.sql("update bot_outbox set lease_until_ms = bot_now_ms() - 1 where state = 'sending'")
        .await;
    let claimed = outbox(&db).claim(&scope, 2).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'outbox.held'")
            .await,
        2
    );

    // 2) approval_open superseding an approved row (supersede + open in one commit).
    let r_ap = leased(&db, "ap-batch").await;
    begin_call(&db, &r_ap.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&r_ap.attempt, "c1", "n-old")))
        .await
        .unwrap();
    calls(&db)
        .supersede(
            ApprovalId("n-old".into()),
            PrincipalId("local:owner".into()),
        )
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    calls(&db)
        .open_approval(open_approval(binding(&resumed.attempt, "c1", "n-new")))
        .await
        .unwrap();
    calls(&db)
        .decide(Decision {
            nonce: ApprovalId("n-new".into()),
            actor: PrincipalId("local:owner".into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed2 = runs(&db).claim().await.unwrap().unwrap();
    calls(&db)
        .open_approval(open_approval(binding(&resumed2.attempt, "c1", "n-new-2")))
        .await
        .unwrap();

    // 3) cancel_run: cancel parent and delegated subtree together.
    let parent = leased(&db, "cancel-tree").await;
    DelegationStore::delegate(
        &db.store,
        Delegate {
            parent: parent.attempt.clone(),
            step: "child-1".into(),
            child: AgentId("main/sub".into()),
            instruction: "subtask".into(),
            scopes: run_scopes(),
            checked_revision: Revision("r1".into()),
        },
    )
    .await
    .unwrap();
    RunAdmin::cancel(
        &db.store,
        profile(),
        parent.attempt.run.clone(),
        PrincipalId("local:admin".into()),
        "stop".into(),
    )
    .await
    .unwrap();

    // 4) ActiveRevisions::replace across multiple profiles.
    ActiveRevisions::replace(
        &db.store,
        Some(Revision("set-1".into())),
        Revision("set-2".into()),
        BTreeMap::from([(ProfileId("personal".into()), Revision("r2".into()))]),
        vec![],
        BTreeMap::new(),
    )
    .await
    .unwrap();

    let report = db.store.verify().await.unwrap();
    assert_eq!(report.first_bad, None, "batched audit chain must verify");
    assert!(report.entries >= 15);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_approval_supersede_and_call_ops_never_deadlock() {
    use bot_core::ledger::*;
    let Some(db) = TestDb::new().await else {
        return;
    };
    for round in 0..8 {
        let req = format!("race-sup-{round}");
        let run = leased(&db, &req).await;
        begin_call(&db, &run.attempt, "c1").await;
        let nonce_old = format!("n-race-old-{round}");
        calls(&db)
            .open_approval(open_approval(binding(&run.attempt, "c1", &nonce_old)))
            .await
            .unwrap();
        let s1 = db.peer("p1").await;
        let s2 = db.peer("p2").await;
        let nonce_for_sup = ApprovalId(nonce_old.clone());
        let nonce_for_dec = ApprovalId(nonce_old);
        let t1 = tokio::spawn(async move {
            CallLedger::supersede(&s1, nonce_for_sup, PrincipalId("local:owner".into())).await
        });
        let t2 = tokio::spawn(async move {
            CallLedger::decide(
                &s2,
                Decision {
                    nonce: nonce_for_dec,
                    actor: PrincipalId("local:owner".into()),
                    allow: true,
                },
            )
            .await
        });
        let (r1, r2) = tokio::join!(t1, t2);
        assert!(
            !matches!(r1.unwrap(), Err(Error::Unavailable)),
            "supersede must not deadlock"
        );
        assert!(
            !matches!(r2.unwrap(), Err(Error::Unavailable)),
            "decide must not deadlock"
        );
    }
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

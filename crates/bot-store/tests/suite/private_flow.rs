//! End-to-end private DM flow: real `PrivateGate` over PG `PrivateLedger`, enforcing zero plaintext storage and durable holds.

use crate::support::*;
use bot_broker::{
    ports::Ran,
    private::{FINGERPRINT_SECRET, PrivateGate, PrivateParts},
    rate::RateConfig,
    sys::SystemClock,
    testing::{
        FakeCatalog, FakeRunner, FakeSealer, FakeSecrets, RecordingTracer, snapshot,
        tool as tool_decl,
    },
};
use bot_core::{
    admin::{AdminAct, AdminRead, HoldRef, HoldVerdict, Viewer},
    command::*,
    id::*,
    intake::{PrivateCall, PrivateDecision, PrivateIntake, RequestKey},
    ledger::{PrivateLedger, PrivatePermit},
    port::{AuditLog, Error},
    state::{Conversation, ConversationKind, Risk},
};
use std::sync::Arc;

const SENTINEL: &str = "TOP-SECRET-DM-TEXT-7731";
const SENDER: &str = "slack:demo/U1";

struct Rig {
    gate: PrivateGate,
    runner: Arc<FakeRunner>,
}

fn rig(db: &TestDb) -> Rig {
    rig_with_pass(db, "r1", false)
}

fn rig_with_pass(db: &TestDb, revision: &str, private_pass: bool) -> Rig {
    let catalog = Arc::new(FakeCatalog::new());
    let mut snap = snapshot("personal", revision)
        .role("owner", &["run", "inbox.write"])
        .member(SENDER, &["owner"])
        .slack_dm_binding("direct", "main")
        .policy_scopes(&["run", "inbox.write"])
        .tool(
            "main",
            "inbox.receive",
            tool_decl(Risk::Write, &["inbox.write"]),
        )
        .private_trigger("main", "secure", "direct", "inbox.receive")
        .build();
    let agent = Arc::get_mut(&mut snap)
        .unwrap()
        .agents
        .get_mut(&AgentId("main".into()))
        .unwrap();
    agent.triggers[0].private_pass = private_pass;
    let mut other = agent.triggers[0].clone();
    other.id = "other".into();
    other.order = 2;
    agent.triggers.push(other);
    catalog.publish(snap);
    let runner = Arc::new(FakeRunner::new());
    let gate = PrivateGate::new(
        PrivateParts {
            catalog,
            ledger: Arc::new(db.store.clone()),
            secrets: Arc::new(FakeSecrets::new(&[(
                "personal",
                FINGERPRINT_SECRET,
                "profile-intake-key-bytes",
            )])),
            sealer: Arc::new(FakeSealer),
            runner: runner.clone(),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
        },
        RateConfig::default(),
    );
    Rig { gate, runner }
}

fn delivery(request: &str, text: &str) -> PrivateCall {
    PrivateCall {
        key: RequestKey {
            profile: ProfileId("personal".into()),
            binding: BindingId("direct".into()),
            principal: PrincipalId(SENDER.into()),
            request: RequestId(request.into()),
        },
        revision: Revision("r1".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        agent: AgentId("main".into()),
        trigger: "secure".into(),
        envelope_sha256: [1; 32],
        tool: ToolId("inbox.receive".into()),
        args: JsonPayload(serde_json::json!({ "text": text }).to_string()),
    }
}

fn ok() -> Ran {
    Ran::Done(JsonPayload(r#"{"isError":false,"content":[]}"#.into()))
}

#[tokio::test]
async fn a_committed_private_pass_survives_restart_until_public_intake_commits_once() {
    use bot_core::port::RunStore;
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = rig_with_pass(&db, "r1", true);
    first.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[],"isError":false,"_meta":{"bot/pass":true}}"#.into(),
    )));
    let input = || delivery("public-handoff", SENTINEL);
    assert_eq!(
        first.gate.receive(input()).await.unwrap(),
        PrivateDecision::Pass
    );
    assert_eq!(first.runner.jobs().len(), 1);
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);
    drop(first);

    let restarted = rig_with_pass(&db, "r1", true);
    // Changing envelope, trigger, or revision must conflict even when tool args match.
    let mut changed = input();
    changed.envelope_sha256 = [2; 32];
    assert!(matches!(
        restarted.gate.receive(changed).await,
        Err(Error::Conflict)
    ));
    let mut changed = input();
    changed.trigger = "other".into();
    assert!(matches!(
        restarted.gate.receive(changed).await,
        Err(Error::Conflict)
    ));
    let mut changed = input();
    changed.revision = Revision("r2".into());
    let newer = rig_with_pass(&db, "r2", true);
    assert!(matches!(
        newer.gate.receive(changed).await,
        Err(Error::Conflict)
    ));
    assert!(newer.runner.jobs().is_empty());
    let mut ids = Vec::new();
    for _ in 0..2 {
        assert_eq!(
            restarted.gate.receive(input()).await.unwrap(),
            PrivateDecision::Pass
        );
        let mut cmd = accept_as(
            "personal",
            "public-handoff:pass",
            SENDER,
            ConversationKind::Direct,
            "dm-1",
            SENTINEL,
        );
        cmd.event.binding = input().key.binding;
        ids.push(db.store.accept(cmd).await.unwrap());
    }
    assert_eq!(ids[0], ids[1]);
    assert!(restarted.runner.jobs().is_empty());
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'private.finish' and verdict = 'passed'"
        )
        .await,
        1
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where not private")
            .await,
        1
    );
}

#[tokio::test]
async fn upgrading_a_completed_private_request_preserves_only_its_private_receipt() {
    let Some(mut db) = TestDb::new().await else {
        return;
    };
    let before = rig(&db);
    before.runner.script(ok());
    let input = || delivery("legacy", SENTINEL);
    before.gate.receive(input()).await.unwrap();
    db.sql("alter table bot_runs drop column private_pass")
        .await;
    db.sql("delete from _sqlx_migrations where version = 23")
        .await;
    db.store.migrate().await.unwrap();
    db.reconnect().await;
    let after = rig_with_pass(&db, "r1", true);
    assert_eq!(
        after.gate.receive(input()).await.unwrap(),
        PrivateDecision::Consumed
    );
    assert!(matches!(
        after.gate.receive(delivery("legacy", "changed")).await,
        Err(Error::Conflict)
    ));
    assert!(after.runner.jobs().is_empty());
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);
}

#[tokio::test]
async fn private_pass_constraint_skips_validation_but_rejects_new_invalid_writes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let gateway = rig(&db);
    gateway.runner.script(ok());
    gateway
        .gate
        .receive(delivery("constraint", "question"))
        .await
        .unwrap();
    let validated = db
        .scalar::<bool>("select convalidated from pg_constraint where conrelid = 'bot_runs'::regclass and pg_get_constraintdef(oid) like '%private_pass%'")
        .await;
    assert!(!validated);
    let error = sqlx::query("update bot_runs set private_pass = 'unknown'")
        .execute(db.store.pool())
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
}

#[tokio::test]
async fn opt_in_without_a_valid_pass_never_grants_public_intake() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig_with_pass(&db, "r1", true);
    for (key, result) in [
        ("normal", ok()),
        (
            "malformed",
            Ran::Done(JsonPayload(
                r#"{"content":[],"_meta":{"bot/pass":true,"extra":1},"isError":false}"#.into(),
            )),
        ),
    ] {
        r.runner.script(result);
        for _ in 0..2 {
            assert_eq!(
                r.gate.receive(delivery(key, SENTINEL)).await.unwrap(),
                PrivateDecision::Consumed
            );
        }
    }
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);
}

fn operator() -> Viewer {
    Viewer {
        principal: PrincipalId("local:owner".into()),
        operator: true,
    }
}

/// Scans every base table in the test schema as text to verify zero plaintext leakage.
async fn rows_containing(db: &TestDb, needle: &str) -> i64 {
    let tables: Vec<String> = sqlx::query_scalar("select table_name::text from information_schema.tables where table_schema = current_schema() and table_type = 'BASE TABLE'")
        .fetch_all(db.store.pool())
        .await
        .unwrap();
    let mut found = 0;
    for table in tables {
        let sql = format!("select count(*) from \"{table}\" t where t::text like '%{needle}%'");
        found += db.scalar::<i64>(&sql).await;
    }
    found
}

async fn the_hold(db: &TestDb) -> HoldRef {
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1);
    assert!(holds[0].call.0.starts_with("private:"));
    HoldRef {
        hold_id: holds[0]
            .hold_id
            .clone()
            .expect("private DM hold must have hold_id"),
        profile: holds[0].profile.clone(),
        run: holds[0].run.clone(),
        call: holds[0].call.clone(),
        effect: None,
    }
}

#[tokio::test]
async fn a_private_dm_reaches_only_its_tool_once_and_leaves_no_plaintext_in_pg() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    r.runner.script(ok());
    r.gate.receive(delivery("req-1", SENTINEL)).await.unwrap();
    let jobs = r.runner.jobs();
    assert_eq!(jobs.len(), 1, "designated tool must run once");
    assert_eq!(jobs[0].principal.0, SENDER);
    assert!(
        jobs[0].args.contains(SENTINEL),
        "only the tool receives plaintext"
    );
    assert_eq!(
        db.scalar::<String>("select state from bot_runs").await,
        "succeeded"
    );
    assert!(
        db.scalar::<bool>("select private_started_ms is not null from bot_runs")
            .await,
        "private_started_ms is recorded before execution"
    );
    for table in ["bot_calls", "bot_effects", "bot_approvals", "bot_steps"] {
        let sql = format!("select count(*) from {table}");
        assert_eq!(db.scalar::<i64>(&sql).await, 0, "{table} must stay empty");
    }
    assert_eq!(
        rows_containing(&db, SENTINEL).await,
        0,
        "plaintext must never be stored in PG"
    );

    r.gate.receive(delivery("req-1", SENTINEL)).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);
    assert!(matches!(
        r.gate.receive(delivery("req-1", "different body")).await,
        Err(Error::Conflict)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn an_unknown_outcome_is_held_durably_never_rerun_and_the_verdict_input_is_not_stored() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    r.runner.script(Ran::Unknown("timeout"));
    r.gate.receive(delivery("req-2", SENTINEL)).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select state || ':' || wait_kind from bot_runs")
            .await,
        "waiting:hold"
    );
    r.gate.receive(delivery("req-2", SENTINEL)).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);

    let hold = the_hold(&db).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:owner".into()),
        hold,
        HoldVerdict::Confirmed {
            receipt: "evt-RECEIPT-1".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        db.scalar::<String>("select state from bot_runs where private")
            .await,
        "succeeded"
    );
    assert_eq!(rows_containing(&db, "evt-RECEIPT-1").await, 0);
    assert_eq!(
        db.scalar::<String>("select verdict from bot_audit where action = 'hold.resolve'")
            .await,
        "confirmed"
    );

    // NotExecuted evidence text must also never be persisted in PG or audit.
    r.runner.script(Ran::Unknown("timeout"));
    r.gate.receive(delivery("req-3", SENTINEL)).await.unwrap();
    let hold = the_hold(&db).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:owner".into()),
        hold,
        HoldVerdict::NotExecuted {
            evidence: "evidence-TEXT-9".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(rows_containing(&db, "evidence-TEXT-9").await, 0);
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'hold.resolve' and verdict = 'not_executed'"
        )
        .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_failure_before_sending_is_resumed_by_the_same_redelivery_with_a_new_claim() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    r.runner.script(Ran::NotSent);
    r.runner.script(ok());
    let first = r.gate.receive(delivery("req-4", SENTINEL)).await;
    assert!(matches!(first, Err(Error::Unavailable)), "must not ACK");
    assert_eq!(
        db.scalar::<String>("select state from bot_runs").await,
        "ready",
        "lease released; workers ignore private runs"
    );
    r.gate.receive(delivery("req-4", SENTINEL)).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(
        db.scalar::<String>(
            "select state || ':' || generation || ':' || claim_epoch from bot_runs"
        )
        .await,
        "succeeded:1:2"
    );
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);
}

#[tokio::test]
async fn a_dead_process_becomes_a_durable_hold_without_any_redelivery() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let request = private_request("req-5", 5);
    let PrivatePermit::Start { .. } = PrivateLedger::begin(&db.store, request).await.unwrap()
    else {
        panic!("expected Start on first intake")
    };
    assert!(matches!(
        PrivateLedger::begin(&db.store, private_request("req-5", 5))
            .await
            .unwrap(),
        PrivatePermit::InProgress
    ));
    assert!(
        AdminRead::holds(&db.store, &operator(), None)
            .await
            .unwrap()
            .is_empty(),
        "not expired yet"
    );

    db.sql("update bot_runs set lease_until_ms = bot_now_ms() - 1 where private")
        .await;
    // Admin holds view sweeps expired private leases into durable holds without needing redelivery.
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1, "expired private intake appears as held");
    assert_eq!(holds[0].reason, "unknown_outcome");
    assert_eq!(
        db.scalar::<String>("select state || ':' || wait_kind from bot_runs")
            .await,
        "waiting:hold"
    );
    assert!(
        matches!(
            PrivateLedger::begin(&db.store, private_request("req-5", 5))
                .await
                .unwrap(),
            PrivatePermit::Held
        ),
        "redelivery must not restart a held private run"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.held'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_redelivery_cannot_change_the_reply_thread_or_use_a_non_direct_conversation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    let mut first = delivery("req-6", SENTINEL);
    first.conversation.thread = Some("t1".into());
    r.gate.receive(first).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);

    let mut other_thread = delivery("req-6", SENTINEL);
    other_thread.conversation.thread = Some("t2".into());
    assert!(matches!(
        r.gate.receive(other_thread).await,
        Err(Error::Conflict)
    ));
    let no_thread = delivery("req-6", SENTINEL);
    assert!(matches!(
        r.gate.receive(no_thread).await,
        Err(Error::Conflict)
    ));
    let mut other_conversation = delivery("req-6", SENTINEL);
    other_conversation.conversation.thread = Some("t1".into());
    other_conversation.conversation.id = "dm-2".into();
    assert!(matches!(
        r.gate.receive(other_conversation).await,
        Err(Error::Conflict)
    ));
    let mut same = delivery("req-6", SENTINEL);
    same.conversation.thread = Some("t1".into());
    r.gate.receive(same).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);

    let mut channel = delivery("req-7", SENTINEL);
    channel.conversation.kind = ConversationKind::Channel;
    assert!(matches!(
        r.gate.receive(channel).await,
        Err(Error::Denied | Error::Invalid)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests where request = 'req-7'")
            .await,
        0
    );
}

fn private_request(request: &str, fingerprint: u8) -> bot_core::ledger::PrivateRequest {
    bot_core::ledger::PrivateRequest {
        key: RequestKey {
            profile: ProfileId("personal".into()),
            binding: BindingId("direct".into()),
            principal: PrincipalId(SENDER.into()),
            request: RequestId(request.into()),
        },
        fingerprint: [fingerprint; 32],
        pass_fingerprint: None,
        revision: Revision("r1".into()),
        agent: AgentId("main".into()),
        tool: ToolId("inbox.receive".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        scopes: scopes(&["inbox.write"]),
    }
}

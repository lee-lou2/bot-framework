//! Deployed migration immutability (SQLx SHA-384 checksums) and in-place upgrade tests.

use crate::support::*;
use bot_core::command::memory_share_hash;
use bot_core::{
    admin::{AdminRead, Viewer},
    command::{EffectRequest, HttpRoute, JsonPayload},
    id::*,
    intake::RequestKey,
    ledger::*,
    port::*,
    state::*,
};
use sha2::{Digest, Sha256, Sha384};

/// Frozen SHA-384 checksums for migrations 0001..0006; changing any breaks startup on deployed DBs.
const FROZEN: [(&str, &str, &str); 6] = [
    (
        "0001_runs",
        include_str!("../../migrations/0001_runs.sql"),
        "e9ea3b8863c22d6ecb5ae7d434d72368bdfe9e34f73858442356b36a1679d58806cc022fb1acb9dcfe014ec23b3ddcdc",
    ),
    (
        "0002_outbox",
        include_str!("../../migrations/0002_outbox.sql"),
        "6ee5e3a0c60c8c21159976a932fed2b0471ff5ad8bc553e7363eac788d0f461dd6915628d06bb27abaf4b0ade7bef7a8",
    ),
    (
        "0003_schedule",
        include_str!("../../migrations/0003_schedule.sql"),
        "aaed39b44954638cc8e709c977096a62a6dd0e2d8b8fa12f478cc1dd51706e8cf60e9ffe42957cc05b92c9ca7677a040",
    ),
    (
        "0004_active",
        include_str!("../../migrations/0004_active.sql"),
        "c83c3953263f3100c20e9e462a89cd3b674613d789bfd7fb4495333f29fb550348a64f5d85b59225865dd053a2879fb1",
    ),
    (
        "0005_ledger",
        include_str!("../../migrations/0005_ledger.sql"),
        "60fa30165622760240915043d634e902c377752c73d98a60dc6843fdb69a1a352a41a9b3387dc7955ea7f3c87dbf159c",
    ),
    (
        "0006_memory",
        include_str!("../../migrations/0006_memory.sql"),
        "d919f04b73e9de923dfe128dfc86ff789d738dce02712f23d60618e9f2c553c04be947984950779225fd8d9fd26c66a5",
    ),
];

#[test]
fn applied_migrations_keep_their_checksums() {
    for (name, sql, expected) in FROZEN {
        let digest = Sha384::digest(sql.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, expected,
            "{name} is already deployed: do not edit it; add a new numbered migration instead"
        );
    }
}

#[tokio::test]
async fn a_database_at_the_previous_version_upgrades_in_place() {
    let Some(mut db) = TestDb::bare().await else {
        return;
    };
    db.publish().await;
    let mut old_http = accept_as(
        "personal",
        "old-http",
        "http:inbox/sub",
        ConversationKind::Http,
        "inbox",
        "x",
    );
    runs(&db).accept(old_http.clone()).await.unwrap();
    let request = PrivateRequest {
        pass_fingerprint: None,
        key: RequestKey {
            profile: profile_id(),
            binding: BindingId("direct".into()),
            principal: PrincipalId("local:owner".into()),
            request: RequestId("old-dm".into()),
        },
        fingerprint: [4; 32],
        revision: Revision("r1".into()),
        agent: AgentId("main".into()),
        tool: ToolId("inbox.receive".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        scopes: scopes(&["inbox.write"]),
    };
    let PrivatePermit::Start { attempt: at, .. } =
        PrivateLedger::begin(&db.store, request).await.unwrap()
    else {
        panic!("Start")
    };
    // Roll schema back to pre-0007 layout to test in-place upgrade.
    for sql in [
        "alter table bot_requests drop constraint bot_requests_http_all_or_none, drop column http_method, drop column http_path, drop column http_response",
        "alter table bot_runs drop column private_started_ms",
        "delete from _sqlx_migrations where version = 7",
        "update bot_requests set input_sha256 = decode(repeat('00', 32), 'hex') where request = 'old-http'",
    ] {
        db.sql(sql).await;
    }

    db.store.migrate().await.expect("0007 must apply in place");
    db.store
        .migrate()
        .await
        .expect("re-running migrate must be idempotent");
    db.reconnect().await;

    // Legacy HTTP row has no route info and conflicts with a route-bearing redelivery.
    let key = RequestKey {
        profile: profile_id(),
        binding: old_http.event.binding.clone(),
        principal: old_http.event.principal.clone(),
        request: old_http.event.request.clone(),
    };
    let state = runs(&db).status(key).await.unwrap().unwrap();
    assert!(state.http.is_none());
    old_http.event.http = Some(HttpRoute {
        method: "POST".into(),
        path: "/agenda".into(),
        response: bot_core::profile::ResponseMode::Accepted,
    });
    assert!(matches!(
        runs(&db).accept(old_http).await,
        Err(Error::Conflict)
    ));

    // Legacy unfinished private run with null `private_started_ms` must hold on lease expiry.
    assert!(
        db.scalar::<bool>("select private_started_ms is null from bot_runs where private")
            .await
    );
    db.expire_lease(&at.run).await;
    let viewer = Viewer {
        principal: PrincipalId("local:owner".into()),
        operator: true,
    };
    assert_eq!(
        AdminRead::holds(&db.store, &viewer, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        PrivateLedger::finish(&db.store, at, PrivateOutcome::Completed).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn old_id_only_share_approvals_are_voided_unconsumed_and_their_runs_woken() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "legacy card note").await;
    let old_hash: [u8; 32] = Sha256::digest(format!(r#"{{"id":"{}"}}"#, id.0).as_bytes()).into();
    calls(&db)
        .begin(
            cli.attempt.clone(),
            call("share"),
            tool("memory.share"),
            old_hash,
        )
        .await
        .unwrap();
    let approval = Approval {
        tool: tool("memory.share"),
        args_sha256: old_hash,
        ..binding(&cli.attempt, "share", "old-card")
    };
    calls(&db)
        .open_approval(open_approval(approval))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &cli.attempt.run).await, "waiting");
    for sql in [
        "alter table bot_approvals add column memory_id text, add column memory_sha256 bytea",
        "delete from _sqlx_migrations where version in (8, 9)",
    ] {
        db.sql(sql).await;
    }

    db.store
        .migrate()
        .await
        .expect("0008 and 0009 must apply in place");

    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals where nonce = 'old-card'")
            .await,
        0,
        "legacy share approval voided (0008) and cleaned up with unstarted call (0009)"
    );
    assert_eq!(
        db.scalar::<String>(&format!(
            "select visibility from bot_memories where id = '{}'",
            id.0
        ))
        .await,
        "private",
        "memory must remain private"
    );
    assert_eq!(
        state_of_run(&db, &cli.attempt.run).await,
        "ready",
        "waiting run wakes up to re-request approval"
    );
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(
        resumed.attempt.generation, cli.attempt.generation,
        "resumes in the same generation"
    );
    let new_hash = memory_share_hash(&id, &Sha256::digest("legacy card note".as_bytes()).into());
    calls(&db)
        .begin(
            resumed.attempt.clone(),
            call("share"),
            tool("memory.share"),
            new_hash,
        )
        .await
        .expect("reusing call ID with new content-bound digest must succeed");
    let card = Approval {
        tool: tool("memory.share"),
        args_sha256: new_hash,
        ..binding(&resumed.attempt, "share", "new-card")
    };
    calls(&db).open_approval(open_approval(card)).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select state from bot_approvals where nonce = 'new-card'")
            .await,
        "pending"
    );
    assert_eq!(state_of_run(&db, &cli.attempt.run).await, "waiting");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_calls where state in ('started', 'done')")
            .await,
        0
    );
}

#[tokio::test]
async fn an_old_form_held_call_with_started_steps_shows_judgeable_holds_after_the_upgrade() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Simulate pre-0010 state: held call with unreceipted `started` steps hidden from admin holds.
    let run = leased(&db, "old-hold").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    for name in ["upload", "notify"] {
        effects(&db)
            .request(EffectRequest {
                attempt: run.attempt.clone(),
                call: call("c1"),
                step: name.into(),
                args: JsonPayload("{}".into()),
            })
            .await
            .unwrap();
    }
    calls(&db)
        .finish(FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    db.sql("update bot_effects set state = 'started', reason = null")
        .await;
    db.sql("delete from _sqlx_migrations where version = 10")
        .await;
    let viewer = Viewer {
        principal: PrincipalId("local:admin".into()),
        operator: true,
    };
    assert!(
        AdminRead::holds(&db.store, &viewer, None)
            .await
            .unwrap()
            .is_empty(),
        "pre-0010 layout shows 0 holds while the run waits"
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");

    db.store.migrate().await.expect("0010 must apply in place");
    let holds = AdminRead::holds(&db.store, &viewer, None).await.unwrap();
    assert_eq!(
        holds.len(),
        1,
        "unresolved held call appears in admin holds"
    );
    assert_eq!(holds[0].steps.len(), 2);
    assert!(holds[0].steps.iter().all(|s| {
        s.state == bot_core::state::EffectState::Held && s.reason.as_deref() == Some("timeout")
    }));
}

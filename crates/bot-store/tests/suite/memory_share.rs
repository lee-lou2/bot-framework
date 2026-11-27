//! Memory sharing (PG): `preview` returns exact body hash, and `share` atomically checks revision, lease, owner, and body hash before consuming approval and promoting visibility.

use crate::support::*;
use bot_core::{
    command::*,
    id::*,
    ledger::*,
    port::*,
    profile::ActiveRevisions,
    state::{Approval, ConversationKind, RunContext, Visibility},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};

fn fingerprint(id: &MemoryId, body: &str) -> [u8; 32] {
    memory_share_hash(id, &Sha256::digest(body.as_bytes()).into())
}

async fn approved_share(
    db: &TestDb,
    run: &RunContext,
    id: &MemoryId,
    body: &str,
    nonce: &str,
) -> RunContext {
    let hash = fingerprint(id, body);
    calls(db)
        .begin(
            run.attempt.clone(),
            call("share"),
            tool("memory.share"),
            hash,
        )
        .await
        .unwrap();
    let approval = Approval {
        tool: tool("memory.share"),
        args_sha256: hash,
        ..binding(&run.attempt, "share", nonce)
    };
    calls(db)
        .open_approval(open_approval(approval))
        .await
        .unwrap();
    let decision = Decision {
        nonce: ApprovalId(nonce.into()),
        actor: PrincipalId(OWNER.into()),
        allow: true,
    };
    calls(db).decide(decision).await.unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

fn share_start(run: &RunContext, body_id: &MemoryId, body: &str, nonce: &str) -> StartCall {
    let expected = Approval {
        tool: tool("memory.share"),
        args_sha256: fingerprint(body_id, body),
        ..binding(&run.attempt, "share", nonce)
    };
    start_call(
        &run.attempt,
        "share",
        Some((expected, vec!["irreversible".into()])),
    )
}

async fn visibility(db: &TestDb, id: &MemoryId) -> String {
    db.scalar::<String>(&format!(
        "select visibility from bot_memories where id = '{}'",
        id.0
    ))
    .await
}

async fn approval_state(db: &TestDb, nonce: &str) -> String {
    db.scalar::<String>(&format!(
        "select state from bot_approvals where nonce = '{nonce}'"
    ))
    .await
}

async fn edit(db: &TestDb, run: &RunContext, id: &MemoryId, text: &str) {
    memories(db)
        .change(
            run.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: text.into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn preview_returns_one_visible_item_with_the_exact_body_hash() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "  shared note\nsecond line  ";
    let id = create(&db, &cli, body).await;
    assert!(!flag(&db, &cli).await);
    let shown = memories(&db)
        .preview(cli.attempt.clone(), id.clone())
        .await
        .unwrap();
    assert_eq!(shown.text, body, "body must preserve exact whitespace");
    assert_eq!(
        shown.sha256,
        <[u8; 32]>::from(Sha256::digest(body.as_bytes()))
    );
    assert_eq!(shown.visibility, Visibility::Private);
    assert!(
        flag(&db, &cli).await,
        "private recall flag must be raised before returning private body"
    );
    assert!(matches!(
        memories(&db)
            .preview(cli.attempt.clone(), MemoryId("none".into()))
            .await,
        Err(Error::Missing)
    ));

    let guest = run_in(&db, "guest", GUEST, ConversationKind::Direct).await;
    assert!(matches!(
        memories(&db).preview(guest.attempt.clone(), id).await,
        Err(Error::Missing)
    ));
    assert!(!flag(&db, &guest).await);
}

#[tokio::test]
async fn sharing_commits_approval_visibility_call_and_audit_in_one_step() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "shared note";
    let id = create(&db, &cli, body).await;
    let other = create(&db, &cli, "other note").await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    assert_eq!(visibility(&db, &id).await, "private");

    let share = |start: StartCall| memories(&db).share(start, id.clone());
    assert_eq!(
        share(share_start(&resumed, &id, body, "n1")).await.unwrap(),
        id
    );
    assert_eq!(visibility(&db, &id).await, "profile");
    assert_eq!(approval_state(&db, "n1").await, "consumed");
    assert_eq!(
        db.scalar::<String>("select state || ':' || result from bot_calls")
            .await,
        format!(r#"done:{{"id":"{}"}}"#, id.0),
        "call result records only the memory ID"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1
    );

    // Replaying a completed share call returns the receipt without re-sharing after an edit.
    edit(&db, &resumed, &id, "edited text").await;
    assert_eq!(visibility(&db, &id).await, "private");
    assert_eq!(
        share(share_start(&resumed, &id, body, "n1")).await.unwrap(),
        id
    );
    assert_eq!(visibility(&db, &id).await, "private", "must not re-share");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1
    );
    assert!(matches!(
        memories(&db)
            .share(share_start(&resumed, &id, body, "n1"), other)
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_shared_item_is_visible_to_others_until_it_is_edited() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "shared note";
    let id = create(&db, &cli, body).await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    memories(&db)
        .share(share_start(&resumed, &id, body, "n1"), id.clone())
        .await
        .unwrap();
    let guest = run_in(&db, "guest", GUEST, ConversationKind::Channel).await;
    assert_eq!(texts(&db, &guest, "").await, [body]);
    assert!(
        !flag(&db, &guest).await,
        "profile-shared memory must not raise private recall flag"
    );
    let shown = memories(&db)
        .preview(guest.attempt.clone(), id.clone())
        .await
        .unwrap();
    assert_eq!(shown.visibility, Visibility::Profile);
    // Editing a shared memory reverts visibility to private, requiring fresh approval.
    edit(&db, &resumed, &id, "updated body").await;
    assert_eq!(visibility(&db, &id).await, "private");
}

#[tokio::test]
async fn a_changed_body_conflicts_and_leaves_the_approval_unconsumed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "initial content").await;
    let resumed = approved_share(&db, &cli, &id, "initial content", "n1").await;
    edit(&db, &resumed, &id, "silently modified").await;
    let start = || share_start(&resumed, &id, "initial content", "n1");
    assert!(matches!(
        memories(&db).share(start(), id.clone()).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        approval_state(&db, "n1").await,
        "approved",
        "approval must remain unconsumed"
    );
    assert_eq!(visibility(&db, &id).await, "private");
    assert_eq!(
        db.scalar::<String>("select state from bot_calls").await,
        "open"
    );
    // Matching is by body fingerprint rather than timestamp, so restoring the exact text succeeds.
    edit(&db, &resumed, &id, "initial content").await;
    memories(&db).share(start(), id.clone()).await.unwrap();
    assert_eq!(visibility(&db, &id).await, "profile");
}

#[tokio::test]
async fn the_generic_start_refuses_the_share_tool_and_a_share_needs_the_owners_own_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "owner note";
    let id = create(&db, &cli, body).await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    assert!(matches!(
        calls(&db)
            .start(share_start(&resumed, &id, body, "n1"))
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(approval_state(&db, "n1").await, "approved");
    let mut bare = share_start(&resumed, &id, body, "n1");
    bare.approval = None;
    assert!(matches!(
        memories(&db).share(bare, id.clone()).await,
        Err(Error::Denied)
    ));
    let guest = run_in(&db, "guest", GUEST, ConversationKind::Direct).await;
    let hash = fingerprint(&id, body);
    calls(&db)
        .begin(guest.attempt.clone(), call("g"), tool("memory.share"), hash)
        .await
        .unwrap();
    let theirs = Approval {
        tool: tool("memory.share"),
        args_sha256: hash,
        requester: PrincipalId(GUEST.into()),
        approver: PrincipalId(GUEST.into()),
        ..binding(&guest.attempt, "g", "n2")
    };
    calls(&db)
        .open_approval(open_approval(theirs.clone()))
        .await
        .unwrap();
    let decision = Decision {
        nonce: ApprovalId("n2".into()),
        actor: PrincipalId(GUEST.into()),
        allow: true,
    };
    calls(&db).decide(decision).await.unwrap();
    let guest = runs(&db).claim().await.unwrap().unwrap();
    let theirs = Approval {
        attempt: guest.attempt.clone(),
        ..theirs
    };
    let start = start_call(
        &guest.attempt,
        "g",
        Some((theirs, vec!["irreversible".into()])),
    );
    assert!(matches!(
        memories(&db).share(start, id.clone()).await,
        Err(Error::Denied)
    ));
    assert_eq!(approval_state(&db, "n2").await, "approved");
    assert_eq!(visibility(&db, &id).await, "private");
}

#[tokio::test]
async fn a_replaced_active_revision_refuses_the_share_before_consuming_the_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "card shown before revision swap";
    let id = create(&db, &cli, body).await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([
                (profile_id(), Revision("r2".into())),
                (ProfileId("work".into()), Revision("r1".into())),
            ]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        memories(&db)
            .share(share_start(&resumed, &id, body, "n1"), id.clone())
            .await,
        Err(Error::Stale)
    ));
    assert_eq!(approval_state(&db, "n1").await, "approved");
    assert_eq!(visibility(&db, &id).await, "private");
}

#[tokio::test]
async fn an_edit_racing_the_share_wins_when_it_commits_first() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "body before race";
    let id = create(&db, &cli, body).await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    let mut connection = db.raw().await;
    let mut editing = sqlx::Connection::begin(&mut connection).await.unwrap();
    let writer: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *editing)
        .await
        .unwrap();
    sqlx::query("update bot_memories set body = 'body changed mid-race' where id = $1")
        .bind(&id.0)
        .execute(&mut *editing)
        .await
        .unwrap();
    let peer = db.peer("racer").await;
    let (start, target) = (share_start(&resumed, &id, body, "n1"), id.clone());
    let sharing = tokio::spawn(async move { MemoryStore::share(&peer, start, target).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            )
            .bind(writer)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
            if waiting {
                break;
            }
            assert!(!sharing.is_finished(), "share finished before edit committed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("share must wait on the locked memory row");
    editing.commit().await.unwrap();
    assert!(matches!(sharing.await.unwrap(), Err(Error::Conflict)));
    assert_eq!(approval_state(&db, "n1").await, "approved");
    assert_eq!(visibility(&db, &id).await, "private");
}

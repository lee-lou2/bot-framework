//! 기억 공유(PG): 카드는 `preview` 한 번의 본문으로 만들고, 승인 인자 해시는 {id, 본문 SHA-256}의 지문이다.
//! `share`가 활성 판본·리스·소유자·본문 지문을 확인하고 승인 소비·가시성 변경·호출 완료·감사를 한 커밋으로 한다.

mod support;

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
use support::*;

fn fingerprint(id: &MemoryId, body: &str) -> [u8; 32] {
    memory_share_hash(id, &Sha256::digest(body.as_bytes()).into())
}

/// 공유 호출(memory.share)의 승인까지 받은 뒤 같은 세대로 재개한 attempt. 호출·승인의 해시는 카드에 보인 본문의 지문이다.
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
    let body = "  공용으로 쓸 메모\n두 번째 줄  ";
    let id = create(&db, &cli, body).await;
    assert!(!flag(&db, &cli).await);
    let shown = memories(&db)
        .preview(cli.attempt.clone(), id.clone())
        .await
        .unwrap();
    assert_eq!(shown.text, body, "공백을 지우지 않은 본문 그대로");
    assert_eq!(
        shown.sha256,
        <[u8; 32]>::from(Sha256::digest(body.as_bytes()))
    );
    assert_eq!(shown.visibility, Visibility::Private);
    assert!(
        flag(&db, &cli).await,
        "private 본문이 나가기 전에 플래그를 세운다"
    );
    assert!(matches!(
        memories(&db)
            .preview(cli.attempt.clone(), MemoryId("none".into()))
            .await,
        Err(Error::Missing)
    ));

    // 남의 private은 보이지 않는다(없는 것과 같다)
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
    let body = "공용으로 쓸 메모";
    let id = create(&db, &cli, body).await;
    let other = create(&db, &cli, "다른 메모").await;
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
        "호출 결과에는 ID만 남는다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1
    );

    // 완료한 호출의 재접수는 다시 공유하지 않는다(그 사이 private으로 돌아갔어도 그대로)
    edit(&db, &resumed, &id, "고친 내용").await;
    assert_eq!(visibility(&db, &id).await, "private");
    assert_eq!(
        share(share_start(&resumed, &id, body, "n1")).await.unwrap(),
        id
    );
    assert_eq!(visibility(&db, &id).await, "private", "재공유 0");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1
    );
    // 같은 호출을 다른 기억으로 완료할 수는 없다
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
    let body = "공용으로 쓸 메모";
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
        "profile 기억은 private 플래그를 세우지 않는다"
    );
    let shown = memories(&db)
        .preview(guest.attempt.clone(), id.clone())
        .await
        .unwrap();
    assert_eq!(shown.visibility, Visibility::Profile);
    // 공유한 기억을 고치면 다시 private이 되어 새 승인을 받아야 한다
    edit(&db, &resumed, &id, "내용 바꿈").await;
    assert_eq!(visibility(&db, &id).await, "private");
}

#[tokio::test]
async fn a_changed_body_conflicts_and_leaves_the_approval_unconsumed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "처음 내용").await;
    let resumed = approved_share(&db, &cli, &id, "처음 내용", "n1").await;
    // 카드를 본 뒤 본문이 바뀌었다
    edit(&db, &resumed, &id, "몰래 바꾼 내용").await;
    let start = || share_start(&resumed, &id, "처음 내용", "n1");
    assert!(matches!(
        memories(&db).share(start(), id.clone()).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        approval_state(&db, "n1").await,
        "approved",
        "승인은 소비되지 않는다"
    );
    assert_eq!(visibility(&db, &id).await, "private");
    assert_eq!(
        db.scalar::<String>("select state from bot_calls").await,
        "open"
    );
    // 본문이 카드에 보인 것과 같아지면 승인한 내용이다(시각이 아니라 본문 지문으로 대조한다)
    edit(&db, &resumed, &id, "처음 내용").await;
    memories(&db).share(start(), id.clone()).await.unwrap();
    assert_eq!(visibility(&db, &id).await, "profile");
}

#[tokio::test]
async fn the_generic_start_refuses_the_share_tool_and_a_share_needs_the_owners_own_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let body = "소유자의 메모";
    let id = create(&db, &cli, body).await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    assert!(matches!(
        calls(&db)
            .start(share_start(&resumed, &id, body, "n1"))
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(approval_state(&db, "n1").await, "approved");
    // 승인 없이는 공유하지 못한다
    let mut bare = share_start(&resumed, &id, body, "n1");
    bare.approval = None;
    assert!(matches!(
        memories(&db).share(bare, id.clone()).await,
        Err(Error::Denied)
    ));
    // 다른 사용자는 남의 기억을 공유하지 못한다
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
    let body = "판본 교체 전에 본 카드";
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
    let body = "경합 전의 본문";
    let id = create(&db, &cli, body).await;
    let resumed = approved_share(&db, &cli, &id, body, "n1").await;
    // 다른 쪽이 본문을 고치는 트랜잭션을 쥐고 있는 동안 공유가 들어온다
    let mut connection = db.raw().await;
    let mut editing = sqlx::Connection::begin(&mut connection).await.unwrap();
    let writer: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *editing)
        .await
        .unwrap();
    sqlx::query("update bot_memories set body = '경합 중에 바뀐 본문' where id = $1")
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
            assert!(!sharing.is_finished(), "공유가 편집보다 먼저 끝났다");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("공유가 기억 행의 편집을 기다려야 한다");
    editing.commit().await.unwrap();
    assert!(matches!(sharing.await.unwrap(), Err(Error::Conflict)));
    assert_eq!(approval_state(&db, "n1").await, "approved");
    assert_eq!(visibility(&db, &id).await, "private");
}

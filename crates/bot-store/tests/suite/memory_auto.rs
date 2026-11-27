//! PostgreSQL integration tests for host-owned automatic private memory persistence (`auto_save`/`auto_forget`).

use crate::support::*;
use bot_core::{
    builtin::{MEMORY_SAVE_TOOL, REVISION},
    id::*,
    ledger::{CallLedger, StartCall},
    memory::{DEFAULT_EMBEDDING_DIMENSIONS, MemoryEmbedding, MemoryKind, MemorySave, save_seal},
    port::*,
    state::*,
};

const MODEL: &str = "gemini-embedding-2";
const DIM: usize = DEFAULT_EMBEDDING_DIMENSIONS as usize;

fn basis_vec(axes: &[(usize, f32)]) -> MemoryEmbedding {
    let mut vals = vec![0.0f32; DIM];
    for &(axis, weight) in axes {
        vals[axis % DIM] += weight;
    }
    MemoryEmbedding::normalize(MODEL, vals).expect("non-zero unit vector")
}

#[tokio::test]
async fn auto_save_and_auto_forget_enforce_owner_scope_skip_explicit_calls_and_protect_shared() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rev = Revision("r1".into());
    let dm = run_in(&db, "auto-dm", "slack:home/U1", ConversationKind::Direct).await;
    let cli = run_in(&db, "auto-cli", OWNER, ConversationKind::Cli).await;
    assert!(
        memories(&db)
            .auto_save(
                cli.attempt.clone(),
                rev.clone(),
                MemorySave {
                    text: "CLI preference".into(),
                    kind: MemoryKind::Core,
                    supersedes: None,
                    embedding: None,
                },
            )
            .await
            .unwrap()
            .is_some()
    );

    // 1) Auto-save creates a private memory, deduplicates repeats, and supports non-destructive supersedes.
    let first = memories(&db)
        .auto_save(
            dm.attempt.clone(),
            rev.clone(),
            MemorySave {
                text: "Lives near Gangnam station".into(),
                kind: MemoryKind::Fact,
                supersedes: None,
                embedding: Some(basis_vec(&[(11, 1.0)])),
            },
        )
        .await
        .unwrap()
        .expect("saved");
    assert!(!first.deduplicated);

    let dup = memories(&db)
        .auto_save(
            dm.attempt.clone(),
            rev.clone(),
            MemorySave {
                text: "Lives near Gangnam station".into(),
                kind: MemoryKind::Fact,
                supersedes: None,
                embedding: Some(basis_vec(&[(11, 1.0)])),
            },
        )
        .await
        .unwrap()
        .expect("dedup");
    assert!(dup.deduplicated);
    assert_eq!(dup.id, first.id);

    let moved = memories(&db)
        .auto_save(
            dm.attempt.clone(),
            rev.clone(),
            MemorySave {
                text: "Moved to Pangyo".into(),
                kind: MemoryKind::Fact,
                supersedes: Some(first.id.clone()),
                embedding: Some(basis_vec(&[(12, 1.0)])),
            },
        )
        .await
        .unwrap()
        .expect("superseded");
    assert_ne!(moved.id, first.id);

    // 2) Auto-forget deletes owned private rows, but ignores profile-shared rows.
    let shared_id = db
        .scalar::<String>(
            "insert into bot_memories (profile, agent, owner, visibility, body) values ('personal', 'main', 'slack:home/U1', 'profile', 'Shared note') returning id",
        )
        .await;
    assert!(
        !memories(&db)
            .auto_forget(dm.attempt.clone(), rev.clone(), MemoryId(shared_id))
            .await
            .unwrap(),
        "auto-forget must never delete profile-shared rows"
    );
    assert!(
        memories(&db)
            .auto_forget(dm.attempt.clone(), rev.clone(), moved.id.clone())
            .await
            .unwrap()
    );

    // 3) Public channel is denied.
    let chan = run_in(&db, "auto-ch", "slack:home/U1", ConversationKind::Channel).await;
    let denied = memories(&db)
        .auto_save(
            chan.attempt.clone(),
            rev.clone(),
            MemorySave {
                text: "Channel note".into(),
                kind: MemoryKind::Fact,
                supersedes: None,
                embedding: None,
            },
        )
        .await;
    assert!(matches!(denied, Err(Error::Denied)));

    // 4) Once an explicit `memory.save` tool call completed in the same run, auto_save skips cleanly.
    let explicit_run = run_in(&db, "auto-skip", "slack:home/U1", ConversationKind::Direct).await;
    let spec = MemorySave {
        text: "Explicit save".into(),
        kind: MemoryKind::Core,
        supersedes: None,
        embedding: None,
    };
    let (_, sha) = save_seal(&spec.text, spec.kind, None).unwrap();
    let ledger: &dyn CallLedger = &db.store;
    ledger
        .begin(
            explicit_run.attempt.clone(),
            CallId("explicit-1".into()),
            ToolId(MEMORY_SAVE_TOOL.into()),
            sha,
        )
        .await
        .unwrap();
    memories(&db)
        .save(
            StartCall {
                checked_revision: rev.clone(),
                risk: Risk::Write,
                tool_revision: Revision(REVISION.into()),
                attempt: explicit_run.attempt.clone(),
                call: CallId("explicit-1".into()),
                approval: None,
                private_recall: false,
                owner: "test.explicit-1".into(),
            },
            spec,
        )
        .await
        .unwrap();
    let skipped = memories(&db)
        .auto_save(
            explicit_run.attempt.clone(),
            rev,
            MemorySave {
                text: "Should be skipped".into(),
                kind: MemoryKind::Fact,
                supersedes: None,
                embedding: None,
            },
        )
        .await
        .unwrap();
    assert!(skipped.is_none());
}

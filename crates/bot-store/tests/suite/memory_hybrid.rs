//! PostgreSQL integration tests for hybrid 3072d f16 memory recall, anti-false-taint gating,
//! atomic `memory.save` (dedup, supersede, quota), `memory.forget`, and lock-free embedding backfill.

use crate::support::*;
use bot_core::{
    builtin::{MEMORY_FORGET_TOOL, MEMORY_SAVE_TOOL, REVISION},
    command::*,
    id::*,
    ledger::{CallLedger, StartCall},
    memory::{
        DEFAULT_EMBEDDING_DIMENSIONS, MAX_ACTIVE_MEMORIES_PER_OWNER, MemoryEmbedding, MemoryKind,
        MemoryQuery, MemorySave, forget_seal, save_seal,
    },
    port::*,
    state::*,
};
use sha2::{Digest, Sha256};

const MODEL: &str = "gemini-embedding-2";
const DIM: usize = DEFAULT_EMBEDDING_DIMENSIONS as usize;

/// Builds a deterministic normalized 3072d vector concentrated along specified basis axes.
fn basis_vec(axes: &[(usize, f32)]) -> MemoryEmbedding {
    let mut vals = vec![0.0f32; DIM];
    for &(axis, weight) in axes {
        vals[axis % DIM] += weight;
    }
    MemoryEmbedding::normalize(MODEL, vals).expect("non-zero unit vector")
}

fn start_for(run: &RunContext, call: &str) -> StartCall {
    StartCall {
        checked_revision: Revision("r1".into()),
        risk: Risk::Write,
        tool_revision: Revision(REVISION.into()),
        attempt: run.attempt.clone(),
        call: CallId(call.into()),
        approval: None,
        private_recall: run.private_memory_read,
        owner: format!("test.{call}"),
    }
}

async fn save_via_ledger(
    db: &TestDb,
    run: &RunContext,
    call: &str,
    spec: MemorySave,
) -> Result<bot_core::memory::MemorySaved, Error> {
    let (_, sha) = save_seal(&spec.text, spec.kind, spec.supersedes.as_ref()).unwrap();
    let ledger: &dyn CallLedger = &db.store;
    ledger
        .begin(
            run.attempt.clone(),
            CallId(call.into()),
            ToolId(MEMORY_SAVE_TOOL.into()),
            sha,
        )
        .await?;
    memories(db).save(start_for(run, call), spec).await
}

async fn forget_via_ledger(
    db: &TestDb,
    run: &RunContext,
    call: &str,
    id: MemoryId,
) -> Result<MemoryId, Error> {
    let (_, sha) = forget_seal(&id).unwrap();
    let ledger: &dyn CallLedger = &db.store;
    ledger
        .begin(
            run.attempt.clone(),
            CallId(call.into()),
            ToolId(MEMORY_FORGET_TOOL.into()),
            sha,
        )
        .await?;
    memories(db).forget(start_for(run, call), id).await
}

#[tokio::test]
async fn hybrid_3072d_recall_handles_semantics_exact_ids_korean_and_avoids_false_taint() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli-setup", OWNER, ConversationKind::Cli).await;

    // Seed 7 private memories on orthogonal 3072d semantic clusters so z-score sample >= 6.
    let items = [
        (
            "평일 점심은 돈코츠 라멘을 선호한다.",
            MemoryKind::Fact,
            vec![(10, 1.0)],
        ),
        (
            "프로덕션 배포 중 ERR_LEASE_4092 오류를 lock_timeout 조정으로 해결했다.",
            MemoryKind::Fact,
            vec![(20, 1.0)],
        ),
        (
            "매주 수요일 저녁 클라이밍 짐에 간다.",
            MemoryKind::Fact,
            vec![(30, 1.0)],
        ),
        (
            "외부 이메일 초안 끝인사는 항상 '감사합니다, 민수 드림'으로 작성한다.",
            MemoryKind::Core,
            vec![(40, 1.0)],
        ),
        (
            "Docker Compose 로컬 PostgreSQL 포트는 55432이다.",
            MemoryKind::Fact,
            vec![(50, 1.0)],
        ),
        (
            "제주도 애월 여름 휴가는 8월 12일부터 16일까지다.",
            MemoryKind::Fact,
            vec![(60, 1.0)],
        ),
        (
            "커피는 오트밀크 아이스 라떼 디카페인만 마신다.",
            MemoryKind::Fact,
            vec![(70, 1.0)],
        ),
    ];
    for (idx, (text, kind, axes)) in items.iter().enumerate() {
        save_via_ledger(
            &db,
            &cli,
            &format!("s-{idx}"),
            MemorySave {
                text: (*text).into(),
                kind: *kind,
                supersedes: None,
                embedding: Some(basis_vec(axes)),
            },
        )
        .await
        .unwrap();
    }

    // 1) Pure semantic match (zero lexical overlap) ranks target #1 and taints the run.
    let sem_run = run_in(&db, "r-sem", OWNER, ConversationKind::Direct).await;
    let sem_hits = memories(&db)
        .search_hybrid(
            sem_run.attempt.clone(),
            MemoryQuery {
                text: "오늘 점심 메뉴 추천해줘".into(),
                embedding: Some(basis_vec(&[(10, 0.9), (99, 0.43)])),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert_eq!(sem_hits.len(), 1);
    assert!(sem_hits[0].text.contains("라멘"));
    assert!(flag(&db, &sem_run).await, "private hit must commit taint");

    // 2) Exact error code matches even when embedding points away from the error cluster.
    let code_run = run_in(&db, "r-code", OWNER, ConversationKind::Direct).await;
    let code_hits = memories(&db)
        .search_hybrid(
            code_run.attempt.clone(),
            MemoryQuery {
                text: "ERR_LEASE_4092".into(),
                embedding: Some(basis_vec(&[(500, 1.0)])),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert_eq!(code_hits.len(), 1);
    assert!(code_hits[0].text.contains("ERR_LEASE_4092"));

    // 3) Unrelated query (orthogonal vector + no specific code) returns empty AND keeps private_memory_read false!
    let weather_run = run_in(&db, "r-weather", OWNER, ConversationKind::Direct).await;
    let weather_hits = memories(&db)
        .search_hybrid(
            weather_run.attempt.clone(),
            MemoryQuery {
                text: "오늘 서울 날씨 알려줘".into(),
                embedding: Some(basis_vec(&[(900, 1.0)])),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert!(
        weather_hits.is_empty(),
        "unrelated query must abstain: {weather_hits:?}"
    );
    assert!(
        !flag(&db, &weather_run).await,
        "unrelated query must not trigger false private_memory_read taint"
    );

    // 4) Lexical-only fallback strips Korean particles ('클라이밍은' -> '클라이밍').
    let ko_run = run_in(&db, "r-ko", OWNER, ConversationKind::Direct).await;
    let ko_hits = memories(&db)
        .search_hybrid(ko_run.attempt.clone(), MemoryQuery::lexical("클라이밍은"))
        .await
        .unwrap();
    assert_eq!(ko_hits.len(), 1);
    assert!(ko_hits[0].text.contains("클라이밍"));

    // 5) Channel context never leaks private memories even with an exact vector match.
    let chan = run_in(&db, "r-chan", OWNER, ConversationKind::Channel).await;
    let chan_hits = memories(&db)
        .search_hybrid(
            chan.attempt.clone(),
            MemoryQuery {
                text: "ERR_LEASE_4092".into(),
                embedding: Some(basis_vec(&[(20, 1.0)])),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert!(chan_hits.is_empty());
    assert!(!flag(&db, &chan).await);
}

#[tokio::test]
async fn atomic_save_deduplicates_supersedes_and_forgets_cleanly() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut dm = run_in(&db, "dm", OWNER, ConversationKind::Direct).await;

    // Initial save.
    let first = save_via_ledger(
        &db,
        &dm,
        "c1",
        MemorySave {
            text: "Lives near Gangnam station".into(),
            kind: MemoryKind::Fact,
            supersedes: None,
            embedding: Some(basis_vec(&[(1, 1.0)])),
        },
    )
    .await
    .unwrap();
    assert!(!first.deduplicated);

    // Replay of same call ID returns cached receipt idempotently.
    let replay = memories(&db)
        .save(
            start_for(&dm, "c1"),
            MemorySave {
                text: "Lives near Gangnam station".into(),
                kind: MemoryKind::Fact,
                supersedes: None,
                embedding: Some(basis_vec(&[(1, 1.0)])),
            },
        )
        .await
        .unwrap();
    assert_eq!(replay, first);

    // Near-duplicate embedding (cos >= 0.94) deduplicates without inserting a second row.
    let near_dup = save_via_ledger(
        &db,
        &dm,
        "c2",
        MemorySave {
            text: "Currently living right by Gangnam station".into(),
            kind: MemoryKind::Fact,
            supersedes: None,
            embedding: Some(basis_vec(&[(1, 0.98), (2, 0.2)])),
        },
    )
    .await
    .unwrap();
    assert_eq!(near_dup.id, first.id);
    assert!(near_dup.deduplicated);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories").await,
        1
    );

    // Non-destructive supersede hides old row from both legacy and hybrid search while keeping old body in DB.
    let moved = save_via_ledger(
        &db,
        &dm,
        "c3",
        MemorySave {
            text: "Moved to Pangyo".into(),
            kind: MemoryKind::Fact,
            supersedes: Some(first.id.clone()),
            embedding: Some(basis_vec(&[(1, 0.8), (3, 0.6)])),
        },
    )
    .await
    .unwrap();
    assert!(!moved.deduplicated);
    assert_ne!(moved.id, first.id);
    assert_eq!(
        db.scalar::<String>(&format!(
            "select superseded_by from bot_memories where id = '{}'",
            first.id.0
        ))
        .await,
        moved.id.0
    );
    assert_eq!(texts(&db, &dm, "Gangnam").await.len(), 0);
    assert_eq!(texts(&db, &dm, "Pangyo").await, ["Moved to Pangyo"]);
    assert!(flag(&db, &dm).await);

    // Forget refuses stale private_recall=false once private memory has been read, then succeeds with fresh flag.
    let stale_forget = forget_via_ledger(&db, &dm, "c4", moved.id.clone()).await;
    assert!(matches!(stale_forget, Err(Error::Conflict)));
    dm.private_memory_read = true;
    let gone = memories(&db)
        .forget(start_for(&dm, "c4"), moved.id.clone())
        .await
        .unwrap();
    assert_eq!(gone, moved.id);
    assert_eq!(texts(&db, &dm, "Pangyo").await.len(), 0);
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_memories where id = '{}' and body = '' and embedding_f16 is null and deleted_ms is not null",
            moved.id.0
        ))
        .await,
        1
    );

    // Public channel cannot save.
    let chan = run_in(&db, "ch", OWNER, ConversationKind::Channel).await;
    let denied = save_via_ledger(
        &db,
        &chan,
        "c5",
        MemorySave {
            text: "nope".into(),
            kind: MemoryKind::Fact,
            supersedes: None,
            embedding: None,
        },
    )
    .await;
    assert!(matches!(denied, Err(Error::Denied)));
}

#[tokio::test]
async fn backfill_embeds_seeds_and_skips_if_body_changed_concurrently() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "Seed note needing embedding").await;

    let pending = memories(&db)
        .unembedded(
            ProfileId("personal".into()),
            MODEL.into(),
            DEFAULT_EMBEDDING_DIMENSIONS,
            10,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, id);

    // Mismatched SHA-256 is silently skipped (race safety against concurrent edits).
    memories(&db)
        .store_embedding(
            ProfileId("personal".into()),
            id.clone(),
            [0u8; 32],
            basis_vec(&[(42, 1.0)]),
        )
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories where embedding_f16 is not null")
            .await,
        0
    );

    // Matching SHA-256 stores the 3072d f16 vector (6144 bytes).
    let digest: [u8; 32] = Sha256::digest(b"Seed note needing embedding").into();
    memories(&db)
        .store_embedding(
            ProfileId("personal".into()),
            id.clone(),
            digest,
            basis_vec(&[(42, 1.0)]),
        )
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i32>(&format!(
            "select octet_length(embedding_f16) from bot_memories where id = '{}'",
            id.0
        ))
        .await,
        (DIM * 2) as i32
    );
    assert!(
        memories(&db)
            .unembedded(
                ProfileId("personal".into()),
                MODEL.into(),
                DEFAULT_EMBEDDING_DIMENSIONS,
                10,
            )
            .await
            .unwrap()
            .is_empty()
    );

    // Editing the body clears the stale embedding so backfill re-embeds it.
    memories(&db)
        .change(
            cli.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "Edited seed note".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        memories(&db)
            .unembedded(
                ProfileId("personal".into()),
                MODEL.into(),
                DEFAULT_EMBEDDING_DIMENSIONS,
                10,
            )
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn owner_quota_blocks_saves_past_limit_while_allowing_supersedes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    // Bulk-insert MAX_ACTIVE_MEMORIES_PER_OWNER rows directly for fast quota verification.
    let mut last_id = String::new();
    for i in 0..MAX_ACTIVE_MEMORIES_PER_OWNER {
        last_id = db
            .scalar::<String>(&format!(
                "insert into bot_memories (profile, agent, owner, body) values ('personal', 'main', '{OWNER}', 'quota item {i}') returning id"
            ))
            .await;
    }
    // New save beyond quota is denied.
    let over = save_via_ledger(
        &db,
        &cli,
        "over",
        MemorySave {
            text: "one too many".into(),
            kind: MemoryKind::Fact,
            supersedes: None,
            embedding: None,
        },
    )
    .await;
    assert!(matches!(over, Err(Error::Denied)));

    // Superseding an existing entry stays within quota and succeeds.
    let replaced = save_via_ledger(
        &db,
        &cli,
        "replace",
        MemorySave {
            text: "replaced entry".into(),
            kind: MemoryKind::Fact,
            supersedes: Some(MemoryId(last_id)),
            embedding: None,
        },
    )
    .await
    .unwrap();
    assert!(!replaced.deduplicated);
}

#[tokio::test]
async fn save_cannot_supersede_shared_profile_memory_and_backfill_fences_stale_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let shared_id = db
        .scalar::<String>(&format!(
            "insert into bot_memories (profile, agent, owner, visibility, body) values ('personal', 'main', '{OWNER}', 'profile', 'shared policy') returning id"
        ))
        .await;
    let denied = save_via_ledger(
        &db,
        &cli,
        "supersede-shared",
        MemorySave {
            text: "private replacement".into(),
            kind: MemoryKind::Fact,
            supersedes: Some(MemoryId(shared_id.clone())),
            embedding: None,
        },
    )
    .await;
    assert!(matches!(denied, Err(Error::Denied)));
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_memories where id = '{shared_id}' and visibility = 'profile' and superseded_by is null"
        ))
        .await,
        1
    );

    let old = db
        .peer("old-worker")
        .await
        .for_activation(Revision("set-1".into()));
    let profiles = std::collections::BTreeMap::from([
        (ProfileId("personal".into()), Revision("r1".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    bot_core::profile::ActiveRevisions::replace(
        &db.store,
        Some(Revision("set-1".into())),
        Revision("set-2".into()),
        profiles,
        vec![],
        std::collections::BTreeMap::new(),
    )
    .await
    .unwrap();
    assert!(matches!(
        MemoryStore::unembedded(
            &old,
            ProfileId("personal".into()),
            MODEL.into(),
            DEFAULT_EMBEDDING_DIMENSIONS,
            10,
        )
        .await,
        Err(Error::Stale)
    ));
    let digest: [u8; 32] = Sha256::digest(b"shared policy").into();
    assert!(matches!(
        MemoryStore::store_embedding(
            &old,
            ProfileId("personal".into()),
            MemoryId(shared_id),
            digest,
            basis_vec(&[(7, 1.0)]),
        )
        .await,
        Err(Error::Stale)
    ));
}

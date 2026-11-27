//! Worker tests for automatic System One memory capture (save, supersede, forget, channel skip, and fail-open).
use super::fakes::*;
use crate::worker::Outcome;
use bot_core::{
    command::{JsonPayload, MemoryHit},
    id::{MemoryId, ProfileId},
    memory::{
        DEFAULT_EMBEDDING_DIMENSIONS, Embedder, MemoryCandidate, MemoryDecision,
        MemoryDecisionItem, MemoryEmbedding, MemoryJudge, MemoryKind,
    },
    port::{Error, Reply},
    state::{ConversationKind, Visibility},
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct ScriptedJudge {
    calls: AtomicUsize,
    seen_candidates: Mutex<Vec<Vec<MemoryCandidate>>>,
    outcome: Mutex<Result<MemoryDecision, Error>>,
}

impl ScriptedJudge {
    fn returning(decision: Result<MemoryDecision, Error>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen_candidates: Mutex::new(Vec::new()),
            outcome: Mutex::new(decision),
        })
    }
}

impl MemoryJudge for ScriptedJudge {
    fn model(&self) -> &str {
        "d1.2-ultra"
    }
    fn judge<'a>(
        &'a self,
        _: &'a ProfileId,
        _: &'a str,
        existing: &'a [MemoryCandidate],
    ) -> Reply<'a, MemoryDecision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.seen_candidates).push(existing.to_vec());
        let res = match &*lock(&self.outcome) {
            Ok(d) => Ok(d.clone()),
            Err(_) => Err(Error::Unavailable),
        };
        Box::pin(async move { res })
    }
}

struct StubEmbedder;
impl Embedder for StubEmbedder {
    fn model(&self) -> &str {
        "gemini-embedding-2"
    }
    fn dimensions(&self) -> u32 {
        DEFAULT_EMBEDDING_DIMENSIONS
    }
    fn embed<'a>(&'a self, _: &'a ProfileId, _: &'a str) -> Reply<'a, Option<MemoryEmbedding>> {
        let mut vals = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS as usize];
        vals[0] = 1.0;
        let emb = MemoryEmbedding::normalize("gemini-embedding-2", vals);
        Box::pin(async move { Ok(emb) })
    }
}

fn run_with(id: &str, kind: ConversationKind, text: &str) -> bot_core::state::RunContext {
    let mut run = agent_run(id);
    run.conversation.kind = kind;
    run.work.input = JsonPayload(json!({ "text": text }).to_string());
    run
}

#[tokio::test]
async fn auto_captures_dm_saves_with_embedding_and_supersedes_private_candidates_only() {
    let r = rig(ScriptedModel::new(vec![say("Noted!")]));
    lock(&r.memory.hits).extend([
        MemoryHit {
            id: MemoryId("priv-1".into()),
            visibility: Visibility::Private,
            text: "Lives in Gangnam".into(),
        },
        MemoryHit {
            id: MemoryId("shared-1".into()),
            visibility: Visibility::Profile,
            text: "Team deploy window".into(),
        },
    ]);
    let judge = ScriptedJudge::returning(Ok(MemoryDecision::Save {
        items: vec![MemoryDecisionItem {
            text: "Moved to Pangyo".into(),
            kind: MemoryKind::Fact,
        }],
        supersedes: Some(MemoryId("priv-1".into())),
    }));
    let worker = r
        .worker
        .with_embedder(Arc::new(StubEmbedder))
        .with_memory_judge(judge.clone());
    let run = run_with("r-dm", ConversationKind::Direct, "Moved to Pangyo");
    assert_eq!(worker.execute(run).await, Outcome::Succeeded);

    // Only private recalled memories are passed to the judge (never profile-shared notes).
    let seen = lock(&judge.seen_candidates);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].len(), 1);
    assert_eq!(seen[0][0].id, MemoryId("priv-1".into()));

    let saved = lock(&r.memory.auto_saved);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].text, "Moved to Pangyo");
    assert_eq!(saved[0].supersedes, Some(MemoryId("priv-1".into())));
    assert_eq!(
        saved[0].embedding.as_ref().map(|e| e.dim()),
        Some(DEFAULT_EMBEDDING_DIMENSIONS)
    );
}

#[tokio::test]
async fn skips_channels_handles_forget_and_fails_open_on_judge_errors() {
    // 1) Public channel never calls the memory judge.
    let ch = rig(ScriptedModel::new(vec![say("Channel reply")]));
    let ch_judge = ScriptedJudge::returning(Ok(MemoryDecision::Save {
        items: vec![MemoryDecisionItem {
            text: "I live in Seoul".into(),
            kind: MemoryKind::Fact,
        }],
        supersedes: None,
    }));
    let outcome = ch
        .worker
        .with_memory_judge(ch_judge.clone())
        .execute(run_with(
            "r-ch",
            ConversationKind::Channel,
            "I live in Seoul",
        ))
        .await;
    assert_eq!(outcome, Outcome::Succeeded);
    assert_eq!(ch_judge.calls.load(Ordering::SeqCst), 0);
    assert!(lock(&ch.memory.auto_saved).is_empty());

    // 2) Forget decision calls `auto_forget`.
    let fg = rig(ScriptedModel::new(vec![say("Forgotten")]));
    let fg_judge = ScriptedJudge::returning(Ok(MemoryDecision::Forget(MemoryId("m-3".into()))));
    assert_eq!(
        fg.worker
            .with_memory_judge(fg_judge)
            .execute(run_with(
                "r-fg",
                ConversationKind::Cli,
                "Forget my climbing schedule"
            ))
            .await,
        Outcome::Succeeded
    );
    assert_eq!(*lock(&fg.memory.auto_forgotten), [MemoryId("m-3".into())]);

    // 3) Transient judge failure still finishes the user's reply cleanly.
    let err = rig(ScriptedModel::new(vec![say("Still answered")]));
    let err_judge = ScriptedJudge::returning(Err(Error::Unavailable));
    assert_eq!(
        err.worker
            .with_memory_judge(err_judge)
            .execute(run_with(
                "r-err",
                ConversationKind::Direct,
                "Prefer oat milk"
            ))
            .await,
        Outcome::Succeeded
    );
    assert!(lock(&err.memory.auto_saved).is_empty());
}

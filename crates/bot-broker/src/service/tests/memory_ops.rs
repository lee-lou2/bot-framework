//! Broker gate tests for built-in `memory.search`, `memory.save`, and `memory.forget`.

use super::*;
use bot_core::{
    memory::{DEFAULT_EMBEDDING_DIMENSIONS, Embedder, MemoryEmbedding},
    port::Reply,
    state::{ConversationKind, Visibility},
};
use std::sync::atomic::{AtomicUsize, Ordering};

const WITH_MEM: [&str; 10] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
    "memory.search",
    "memory.save",
    "memory.forget",
];
const OWNER: &str = "local:owner";

struct CountingEmbedder(AtomicUsize);

impl CountingEmbedder {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicUsize::new(0)))
    }
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn model(&self) -> &str {
        "gemini-embedding-2"
    }
    fn dimensions(&self) -> u32 {
        DEFAULT_EMBEDDING_DIMENSIONS
    }
    fn embed<'a>(&'a self, _: &'a ProfileId, _: &'a str) -> Reply<'a, Option<MemoryEmbedding>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let mut vals = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS as usize];
        vals[0] = 1.0;
        let emb = MemoryEmbedding::normalize("gemini-embedding-2", vals);
        Box::pin(async move { Ok(emb) })
    }
}

fn mem_world() -> (World, Arc<CountingEmbedder>) {
    let w = World::new();
    let embedder = CountingEmbedder::new();
    w.publish(
        base("r1")
            .role("owner", &WITH_MEM)
            .policy_scopes(&WITH_MEM)
            .build(),
    );
    w.ledger
        .add_run(run_context("personal", "r1", 1, OWNER, "main", &WITH_MEM));
    w.memory
        .add("m1", OWNER, Visibility::Private, "Likes tonkotsu ramen");
    w.memory.add(
        "p1",
        "local:admin",
        Visibility::Profile,
        "Deploy freeze on Fridays",
    );
    let broker = w.reboot().with_embedder(embedder.clone());
    (World { broker, ..w }, embedder)
}

fn tainted(w: &World, run: &str) -> bool {
    use bot_core::ledger::CallLedger;
    block_on(
        w.ledger
            .context(w.tool_call(run, 1, "x", "x", "{}").attempt),
    )
    .unwrap()
    .private_memory_read
}

#[test]
fn memory_search_taints_only_when_private_hits_return_and_calls_embedder() {
    let (w, embedder) = mem_world();
    // Searching a profile-only note does not set private taint.
    let profile_res = done(w.call("c1", "memory.search", r#"{"query":"freeze"}"#));
    assert!(profile_res.contains("Deploy freeze"));
    assert_eq!(embedder.count(), 1);
    assert!(!tainted(&w, "r1"));

    // Searching a private note returns it and commits private taint on the run.
    let priv_res = done(w.call("c2", "memory.search", r#"{"query":"ramen","limit":5}"#));
    assert!(priv_res.contains("tonkotsu ramen"));
    assert_eq!(embedder.count(), 2);
    assert!(tainted(&w, "r1"));

    // Replay returns cached result without calling the embedder again.
    let replay = done(w.call("c2", "memory.search", r#"{"query":"ramen","limit":5}"#));
    assert_eq!(replay, priv_res);
    assert_eq!(embedder.count(), 2);
}

#[test]
fn memory_save_runs_directly_when_untainted_and_gates_embedding_behind_approval_when_tainted() {
    let (w, embedder) = mem_world();

    // Clean 1:1 CLI run saves immediately without approval.
    let saved = done(w.call(
        "c1",
        "memory.save",
        r#"{"text":"Prefers decaf oat latte","kind":"core"}"#,
    ));
    assert!(saved.contains("\"deduplicated\":false"), "{saved}");
    assert_eq!(embedder.count(), 1);

    // Public channel is denied before calling the embedder.
    let mut chan = run_context("personal", "r-chan", 1, OWNER, "main", &WITH_MEM);
    chan.conversation.kind = ConversationKind::Channel;
    w.ledger.add_run(chan);
    let err = w.call_on(
        &w.broker,
        "r-chan",
        1,
        "c2",
        "memory.save",
        r#"{"text":"secret"}"#,
    );
    assert!(matches!(err, Err(Error::Denied)));
    assert_eq!(embedder.count(), 1);

    // Once private memory has been read, `memory.save` requires owner approval AND does not invoke
    // the embedding API before the owner approves.
    w.ledger.set_private_recall("r1");
    let nonce = awaiting(w.call(
        "c3",
        "memory.save",
        r#"{"text":"Moved to Pangyo","supersedes":"m1"}"#,
    ));
    assert_eq!(
        embedder.count(),
        1,
        "must not send tainted text to embedding API before human approval"
    );
    w.decide(&nonce, OWNER, true).unwrap();
    let after = done(w.call(
        "c3",
        "memory.save",
        r#"{"text":"Moved to Pangyo","supersedes":"m1"}"#,
    ));
    assert!(after.contains("\"deduplicated\":false"), "{after}");
    assert_eq!(embedder.count(), 2);
    assert!(!w.memory.contains("m1"), "superseded entry replaced");
}

#[test]
fn memory_forget_requires_owner_dm_or_cli_and_honors_policy_approval() {
    let (w, _) = mem_world();
    // Policy-gated `memory.forget` opens an approval card and re-verifies in `Broker::decide`.
    w.publish(
        base("r1")
            .role("owner", &WITH_MEM)
            .policy_scopes(&WITH_MEM)
            .approve(&["memory.forget"])
            .build(),
    );
    let nonce = awaiting(w.call("c1", "memory.forget", r#"{"id":"m1"}"#));
    assert!(w.memory.contains("m1"));
    w.decide(&nonce, OWNER, true).unwrap();
    let res = done(w.call("c1", "memory.forget", r#"{"id":"m1"}"#));
    assert_eq!(res, r#"{"id":"m1"}"#);
    assert!(!w.memory.contains("m1"));

    // Replay succeeds idempotently even though the memory is already removed.
    assert_eq!(done(w.call("c1", "memory.forget", r#"{"id":"m1"}"#)), res);
}

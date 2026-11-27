//! Test database helpers. Skips tests if `BOT_TEST_DATABASE_URL` is unset (fails when `BOT_TEST_REQUIRE_DB=1`).
//! Each test creates an isolated `store_pg_<pid>_<n>` schema and drops it on `Drop` (including panics).
#![allow(dead_code)]

pub mod admin;
pub mod api;
pub mod cards;
pub mod delivery;
pub mod human;
pub mod pass;
pub mod preapproval;
pub mod private;
pub mod schedule;

use bot_core::{command::*, id::*, ledger::*, state::*};
use bot_store::{Config, Store};

use serde_json::json;
use sqlx::{ConnectOptions, Connection, postgres::PgConnectOptions};
use std::{
    collections::BTreeSet,
    str::FromStr,
    sync::atomic::{AtomicU32, Ordering},
};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct TestDb {
    pub store: Store,
    pub schema: String,
    url: String,
}

fn url() -> Option<String> {
    match std::env::var("BOT_TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            assert!(
                std::env::var("BOT_TEST_REQUIRE_DB").is_err(),
                "BOT_TEST_DATABASE_URL is required"
            );
            eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
            None
        }
    }
}

impl TestDb {
    /// Store with active revisions (`personal`, `work` at `r1`) already published.
    pub async fn new() -> Option<TestDb> {
        Self::with(Config::default()).await
    }

    pub async fn with(cfg: Config) -> Option<TestDb> {
        let db = Self::bare_with(cfg).await?;
        db.publish().await;
        Some(db)
    }

    /// Store with no active revisions published.
    pub async fn bare() -> Option<TestDb> {
        Self::bare_with(Config::default()).await
    }

    async fn bare_with(cfg: Config) -> Option<TestDb> {
        let url = url()?;
        let schema = format!(
            "store_pg_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let mut admin = PgConnectOptions::from_str(&url)
            .unwrap()
            .connect()
            .await
            .unwrap();
        sqlx::query(&format!("create schema \"{schema}\""))
            .execute(&mut admin)
            .await
            .unwrap();
        admin.close().await.unwrap();
        let store = Self::connect(&url, &schema, cfg).await;
        store.migrate().await.unwrap();
        Some(TestDb { store, schema, url })
    }

    async fn connect(url: &str, schema: &str, cfg: Config) -> Store {
        // Tag connections with the schema name so Drop terminates only this test's sessions.
        let options = PgConnectOptions::from_str(url)
            .unwrap()
            .options([("search_path", schema)])
            .application_name(schema);
        let pool = Store::pool_options()
            .max_connections(12)
            .connect_with(options)
            .await
            .unwrap();
        Store::from_pool(pool, cfg)
    }

    /// Opens a fresh connection pool after DDL/migration changes to clear cached prepared plans.
    pub async fn reconnect(&mut self) {
        let cfg = self.store.config().clone();
        self.store = Self::connect(&self.url, &self.schema, cfg).await;
    }

    pub async fn peer(&self, worker: &str) -> Store {
        let cfg = Config {
            worker: worker.into(),
            ..self.store.config().clone()
        };
        Self::connect(&self.url, &self.schema, cfg).await
    }

    pub async fn sql(&self, sql: &str) {
        sqlx::query(sql).execute(self.store.pool()).await.unwrap();
    }

    pub async fn scalar<T>(&self, sql: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
    {
        sqlx::query_scalar(sql)
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }

    pub async fn waiter_blocked_by(&self, blocker: i32) -> i32 {
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                if let Some(pid) = sqlx::query_scalar(
                    "select pid from pg_stat_activity where application_name = $1 and $2 = any(pg_blocking_pids(pid))",
                )
                .bind(&self.schema)
                .bind(blocker)
                .fetch_optional(self.store.pool())
                .await
                .unwrap()
                {
                    return pid;
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("expected a connection waiting on the lock")
    }

    pub async fn expire_lease(&self, run: &RunId) {
        self.sql(&format!(
            "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
            run.0
        ))
        .await;
    }

    pub async fn make_due(&self, run: &RunId) {
        self.sql(&format!(
            "update bot_runs set not_before_ms = 0 where id = '{}'",
            run.0
        ))
        .await;
    }
}

impl Drop for TestDb {
    /// Terminates this test's sessions and drops its schema on a separate thread without blocking the runtime.
    fn drop(&mut self) {
        let (url, schema) = (self.url.clone(), self.schema.clone());
        let cleanup = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let Ok(mut conn) = PgConnectOptions::from_str(&url).unwrap().connect().await else { return };
                let _ = sqlx::query("select pg_terminate_backend(pid) from pg_stat_activity where application_name = $1 and pid <> pg_backend_pid()")
                    .bind(&schema)
                    .execute(&mut conn)
                    .await;
                let _ = sqlx::query(&format!("drop schema if exists \"{schema}\" cascade")).execute(&mut conn).await;
            });
        });
        let _ = cleanup.join();
    }
}

// Disambiguate overlapping port trait methods on Store by using trait objects.
pub fn runs(db: &TestDb) -> &dyn bot_core::port::RunStore {
    &db.store
}

pub fn calls(db: &TestDb) -> &dyn bot_core::ledger::CallLedger {
    &db.store
}

pub fn effects(db: &TestDb) -> &dyn bot_core::port::EffectJournal {
    &db.store
}

pub fn outbox(db: &TestDb) -> &dyn bot_core::intake::Outbox {
    &db.store
}

pub fn scopes(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

pub fn run_scopes() -> BTreeSet<String> {
    scopes(&["run", "calendar.read"])
}

pub fn accept(profile: &str, request: &str, text: &str) -> Accept {
    accept_as(
        profile,
        request,
        "local:owner",
        ConversationKind::Cli,
        "owner",
        text,
    )
}

pub fn accept_as(
    profile: &str,
    request: &str,
    principal: &str,
    kind: ConversationKind,
    conv: &str,
    text: &str,
) -> Accept {
    let http = (kind == ConversationKind::Http).then(|| HttpRoute {
        method: "POST".into(),
        path: "/agenda".into(),
        response: bot_core::profile::ResponseMode::Accepted,
    });
    Accept {
        reply_transport: matches!(kind, ConversationKind::Direct | ConversationKind::Channel)
            .then_some(bot_core::intake::PushTransportId::SLACK),
        event: Envelope {
            profile: ProfileId(profile.into()),
            request: RequestId(request.into()),
            binding: BindingId("desk".into()),
            principal: PrincipalId(principal.into()),
            conversation: Conversation {
                kind,
                id: conv.into(),
                thread: None,
            },
            http,
            attachments: vec![],
            body: JsonPayload(json!({"text": text}).to_string()),
        },
        approver: None,
        revision: Revision("r1".into()),
        target: Target::Agent {
            agent: AgentId("main".into()),
            instruction: text.into(),
        },
        scopes: run_scopes(),
    }
}

pub fn tool_accept(profile: &str, request: &str, tool: &str, args: &str) -> Accept {
    let mut a = accept(profile, request, "direct");
    a.target = Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId(tool.into()),
        args: JsonPayload(args.into()),
    };
    a
}

pub fn done(text: &str) -> Finish {
    Finish {
        result: JsonPayload(json!({"text": text}).to_string()),
        answers: vec![JsonPayload(json!({"text": text}).to_string())],
    }
}

pub fn occurrence_accept(
    profile: &str,
    trigger: &str,
    at_ms: i64,
    target: Target,
    scopes: &[&str],
) -> Accept {
    Accept {
        reply_transport: None,
        event: Envelope {
            profile: ProfileId(profile.into()),
            request: RequestId(format!("cron:{trigger}:{at_ms}")),
            binding: BindingId("@cron".into()),
            principal: PrincipalId("service:morning".into()),
            conversation: Conversation {
                kind: ConversationKind::Scheduled,
                id: trigger.into(),
                thread: None,
            },
            http: None,
            attachments: vec![],
            body: JsonPayload("{}".into()),
        },
        approver: None,
        revision: Revision("r1".into()),
        target,
        scopes: self::scopes(scopes),
    }
}

pub fn trigger_spec(trigger: &str, cron: &str, beat: bool) -> TriggerSpec {
    TriggerSpec {
        approver: None,
        trigger: trigger.into(),
        cron: cron.into(),
        timezone: "UTC".into(),
        beat,
        principal: PrincipalId("service:morning".into()),
        scopes: scopes(&["calendar.read"]),
        target: Target::Tool {
            agent: AgentId("main".into()),
            tool: ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        },
    }
}

pub fn schedule_set(profile: &str, revision: &str, triggers: Vec<TriggerSpec>) -> ScheduleSet {
    ScheduleSet {
        profile: ProfileId(profile.into()),
        revision: Revision(revision.into()),
        triggers,
    }
}

impl TestDb {
    /// Shifts schedule segments and cursors back one hour so past occurrences can be accepted.
    pub async fn backdate_schedule(&self) {
        self.sql("update bot_schedule_segments set from_ms = from_ms - 3600000")
            .await;
        self.sql("update bot_schedule_cursors set cursor_ms = cursor_ms - 3600000")
            .await;
    }

    pub async fn now_ms(&self) -> i64 {
        self.scalar::<i64>("select bot_now_ms()").await
    }
}

pub fn profile_id() -> ProfileId {
    ProfileId("personal".into())
}

pub fn call(id: &str) -> CallId {
    CallId(id.into())
}

pub fn tool(id: &str) -> ToolId {
    ToolId(id.into())
}

pub async fn leased(db: &TestDb, request: &str) -> RunContext {
    db.publish().await;
    runs(db)
        .accept(accept("personal", request, "hi"))
        .await
        .unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

pub async fn leased_in(db: &TestDb, request: &str, kind: ConversationKind) -> RunContext {
    db.publish().await;
    runs(db)
        .accept(accept_as(
            "personal",
            request,
            "local:owner",
            kind,
            "c1",
            "hi",
        ))
        .await
        .unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

pub async fn begin_call(db: &TestDb, at: &Attempt, id: &str) -> CallRecord {
    calls(db)
        .begin(at.clone(), call(id), tool("web.search"), [1; 32])
        .await
        .unwrap()
}

pub fn start_call(at: &Attempt, id: &str, approval: Option<(Approval, Vec<String>)>) -> StartCall {
    StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Read,
        tool_revision: Revision("test".into()),
        attempt: at.clone(),
        call: call(id),
        approval,
        private_recall: false,
        owner: format!("{}.{}.{}", at.run.0, at.generation.0, id),
    }
}

pub fn binding(at: &Attempt, id: &str, nonce: &str) -> Approval {
    Approval {
        nonce: ApprovalId(nonce.into()),
        attempt: at.clone(),
        call: call(id),
        requester: PrincipalId("local:owner".into()),
        approver: PrincipalId("local:owner".into()),
        risk: Risk::Write,
        policy_revision: Revision("p1".into()),
        tool: tool("web.search"),
        tool_revision: Revision("t1".into()),
        args_sha256: [1; 32],
        expires_at: std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
    }
}

pub fn open_approval(approval: Approval) -> OpenApproval {
    // schedule.create cards must carry sealed canonical args matching the approval digest.
    assert_ne!(
        approval.tool.0, "schedule.create",
        "build schedule.create cards with support::schedule::open_create"
    );
    let card = format!(
        r#"{{"approval":{{"nonce":"{}","title":"Delete approval","lines":["1 event"]}}}}"#,
        approval.nonce.0
    );
    OpenApproval {
        approval,
        reasons: vec!["irreversible".into()],
        card: JsonPayload(card),
        delivery: None,
    }
}

pub async fn state_of_run(db: &TestDb, run: &RunId) -> String {
    db.scalar::<String>(&format!(
        "select state from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

impl TestDb {
    pub async fn publish_schedule(&self, set: ScheduleSet) -> Result<(), bot_core::port::Error> {
        use bot_core::profile::ActiveRevisions;
        use std::collections::BTreeMap;
        let expected = self.store.current().await?;
        let next = Revision(format!("schedule-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let profiles = BTreeMap::from([
            (ProfileId("work".into()), Revision("r1".into())),
            (set.profile.clone(), set.revision),
        ]);
        self.store
            .replace(
                expected,
                next,
                profiles,
                vec![],
                BTreeMap::from([(set.profile, set.triggers)]),
            )
            .await
    }

    pub async fn publish(&self) {
        if self.scalar::<i64>("select count(*) from bot_active").await == 0 {
            let set = std::collections::BTreeMap::from([
                (ProfileId("personal".into()), Revision("r1".into())),
                (ProfileId("work".into()), Revision("r1".into())),
            ]);
            bot_core::profile::ActiveRevisions::replace(
                &self.store,
                None,
                Revision("set-1".into()),
                set,
                Vec::new(),
                Default::default(),
            )
            .await
            .unwrap();
        }
    }

    pub async fn raw(&self) -> sqlx::PgConnection {
        let options = PgConnectOptions::from_str(&self.url)
            .unwrap()
            .options([("search_path", self.schema.as_str())])
            .application_name(&self.schema);
        options.connect().await.unwrap()
    }

    pub async fn takeover(&self, run: &RunId) -> RunContext {
        self.expire_lease(run).await;
        runs(self).claim().await.unwrap().unwrap()
    }
}

pub fn assert_resumed(before: &Attempt, after: &Attempt) {
    assert_eq!(
        (&after.run, after.generation),
        (&before.run, before.generation),
        "normal resume must keep the same generation"
    );
    assert!(
        after.claim.map(|c| c.0) > before.claim.map(|c| c.0),
        "claim token must advance on each resume: {before:?} -> {after:?}"
    );
}

pub fn memories(db: &TestDb) -> &dyn bot_core::port::MemoryStore {
    &db.store
}

pub const OWNER: &str = "local:owner";
pub const GUEST: &str = "slack:home/U2";

pub async fn run_in(
    db: &TestDb,
    request: &str,
    principal: &str,
    kind: ConversationKind,
) -> RunContext {
    db.publish().await;
    runs(db)
        .accept(accept_as("personal", request, principal, kind, "c1", "hi"))
        .await
        .unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

pub async fn create(db: &TestDb, run: &RunContext, text: &str) -> MemoryId {
    memories(db)
        .change(
            run.attempt.clone(),
            MemoryChange::Create { text: text.into() },
        )
        .await
        .unwrap()
}

pub async fn texts(db: &TestDb, run: &RunContext, query: &str) -> Vec<String> {
    memories(db)
        .search(run.attempt.clone(), query.into())
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.text)
        .collect()
}

pub async fn flag(db: &TestDb, run: &RunContext) -> bool {
    db.scalar::<bool>(&format!(
        "select private_memory_read from bot_runs where id = '{}'",
        run.root.0
    ))
    .await
}

//! Shared PostgreSQL + BrokerService integration test harness for `bot-runtime`.
//! Isolates each test in its own `runtime_pg_<pid>_<n>` schema and drops it on `Drop`.
#![allow(dead_code)]

pub mod api;
pub mod cli;
pub mod model_request;
pub mod sweep;
pub mod turns;
pub mod upstream;
pub mod view;

use bot_broker::{
    service::{BrokerService, Config as BrokerConfig, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot, tool},
};
use bot_core::{
    command::*,
    id::*,
    intake::{PushTransportId, RequestKey},
    port::RunStore,
    profile::{ActiveRevisions, Snapshot},
    state::{Conversation, ConversationKind, Risk},
};
use bot_runtime::{Deps, Models, NoRelays, Worker, WorkerConfig};
use bot_store::{Config as StoreConfig, Store};
use sqlx::{ConnectOptions, Connection, postgres::PgConnectOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub const OWNER: &str = "local:owner";
pub const EVENTS: &str = r#"{"events":[{"id":"e1","title":"Walk"}]}"#;

pub struct Pg {
    pub store: Arc<Store>,
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

async fn connect(url: &str, schema: &str, worker: &str) -> Arc<Store> {
    let options = PgConnectOptions::from_str(url)
        .unwrap()
        .options([("search_path", schema)])
        .application_name(schema);
    let pool = Store::pool_options()
        .max_connections(12)
        .connect_with(options)
        .await
        .unwrap();
    Arc::new(Store::from_pool(
        pool,
        StoreConfig {
            worker: worker.into(),
            ..StoreConfig::default()
        },
    ))
}

impl Pg {
    pub async fn new() -> Option<Pg> {
        let url = url()?;
        let schema = format!(
            "runtime_pg_{}_{}",
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
        let store = connect(&url, &schema, "worker-1").await;
        store.migrate().await.unwrap();
        Some(Pg { store, schema, url })
    }

    pub async fn peer(&self, worker: &str) -> Arc<Store> {
        connect(&self.url, &self.schema, worker).await
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

    pub async fn state_of(&self, run: &RunId) -> String {
        self.scalar::<String>(&format!(
            "select state from bot_runs where id = '{}'",
            run.0
        ))
        .await
    }

    pub async fn assert_state(&self, run: &RunId, expected: &str) {
        let row: (String, Option<String>, i32, i64, i64, Option<i64>, i64) = sqlx::query_as(&format!(
            "select state, wait_kind, attempts, generation, claim_epoch, lease_until_ms, bot_now_ms() from bot_runs where id = '{}'",
            run.0
        ))
        .fetch_one(self.store.pool())
        .await
        .unwrap();
        assert_eq!(
            row.0, expected,
            "run {}: state={} wait_kind={:?} attempts={} generation={} claim_epoch={} lease_until_ms={:?} now_ms={}",
            run.0, row.0, row.1, row.2, row.3, row.4, row.5, row.6
        );
    }
}

impl Drop for Pg {
    /// Terminates this test's schema connections and drops the schema on a dedicated thread to avoid blocking the runtime.
    fn drop(&mut self) {
        let (url, schema) = (self.url.clone(), self.schema.clone());
        let cleanup = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let Ok(mut conn) = PgConnectOptions::from_str(&url).unwrap().connect().await else {
                    return;
                };
                let _ = sqlx::query("select pg_terminate_backend(pid) from pg_stat_activity where application_name = $1 and pid <> pg_backend_pid()")
                    .bind(&schema)
                    .execute(&mut conn)
                    .await;
                let _ = sqlx::query(&format!("drop schema if exists \"{schema}\" cascade"))
                    .execute(&mut conn)
                    .await;
            });
        });
        let _ = cleanup.join();
    }
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

pub fn profile() -> Arc<Snapshot> {
    profile_at("r1", false)
}

/// Revision r2 adds `agenda.extra`, absent from runs pinned to r1.
pub fn profile_next() -> Arc<Snapshot> {
    profile_at("r2", true)
}

fn profile_at(revision: &str, extra: bool) -> Arc<Snapshot> {
    let mut moved = tool(Risk::Read, &["calendar.read"]);
    moved.input = JsonPayload(
        r#"{"type":"object","required":["event"],"properties":{"event":{"type":"string","pattern":"^[a-z0-9-]+$"}},"additionalProperties":false}"#.into(),
    );
    let mut builder = snapshot("personal", revision)
        .role("owner", &["run", "calendar.read", "calendar.delete"])
        .role("reader", &["run", "calendar.read"])
        .member(OWNER, &["owner"])
        .member("service:morning", &["reader"])
        .policy_scopes(&["run", "calendar.read", "calendar.delete"])
        .tool("main", "agenda.list", tool(Risk::Read, &["calendar.read"]))
        .tool(
            "main",
            "agenda.delete",
            tool(Risk::Irreversible, &["calendar.delete"]),
        )
        .tool("main", "agenda.move", moved);
    if extra {
        builder = builder.tool("main", "agenda.extra", tool(Risk::Read, &["calendar.read"]));
    }
    builder
        .agent("main/research")
        .cron_trigger("main", "morning", "service:morning", None)
        .build()
}

pub struct World {
    pub pg: Pg,
    pub broker: Arc<BrokerService>,
    pub runner: Arc<FakeRunner>,
    pub catalog: Arc<FakeCatalog>,
    pub spans: Arc<RecordingTracer>,
}

pub async fn world() -> Option<World> {
    world_with(Arc::new(FakeSealer)).await
}

pub async fn world_with(sealer: Arc<dyn bot_broker::ports::Sealer>) -> Option<World> {
    world_with_routes(sealer, None).await
}

pub async fn world_with_routes(
    sealer: Arc<dyn bot_broker::ports::Sealer>,
    routes: Option<Arc<dyn bot_core::schedule::ScheduleRoutes>>,
) -> Option<World> {
    let mut pg = Pg::new().await?;
    pg.store
        .replace(
            None,
            Revision("set-1".into()),
            BTreeMap::from([(ProfileId("personal".into()), Revision("r1".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(profile());
    let runner = Arc::new(FakeRunner::new());
    runner.set_default_result(EVENTS);
    let spans = Arc::new(RecordingTracer::new());
    if routes.is_some() {
        pg.store = Arc::new(pg.store.for_activation(Revision("set-1".into())));
    }
    let store = pg.store.clone();
    let broker = BrokerService::new(
        Parts {
            catalog: catalog.clone(),
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store.clone(),
            runner: runner.clone(),
            sealer,
            tracer: spans.clone(),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        BrokerConfig::default(),
    )
    .with_schedules(store.clone());
    let broker = Arc::new(match routes {
        Some(routes) => broker.with_routes(routes).with_replies(store),
        None => broker,
    });
    Some(World {
        pg,
        broker,
        runner,
        catalog,
        spans,
    })
}

pub fn config() -> WorkerConfig {
    WorkerConfig {
        concurrency: 2,
        poll: Duration::from_millis(10),
        heartbeat: Duration::from_millis(40),
        // Tolerate brief CPU/DB contention across parallel tests without losing leases.
        heartbeat_misses: 12,
        grace: Duration::from_secs(1),
        ..WorkerConfig::default()
    }
}

impl World {
    pub fn worker(&self, store: &Arc<Store>, models: Arc<dyn Models>, cfg: WorkerConfig) -> Worker {
        Worker::new(
            Deps {
                runs: store.clone(),
                memory: store.clone(),
                delegation: store.clone(),
                broker: self.broker.clone(),
                ledger: store.clone(),
                catalog: self.catalog.clone(),
                models,
                relays: Arc::new(NoRelays),
                tracer: self.spans.clone(),
                gateway: None,
            },
            cfg,
        )
    }

    pub async fn accept(&self, request: &str, text: &str) -> RunId {
        self.pg
            .store
            .accept(command(request, text))
            .await
            .expect("accept")
    }

    pub async fn accept_slack_dm(&self, request: &str, text: &str) -> RunId {
        let mut cmd = command(request, text);
        cmd.reply_transport = Some(PushTransportId::SLACK);
        cmd.event.conversation.kind = ConversationKind::Direct;
        self.pg.store.accept(cmd).await.expect("accept")
    }

    pub async fn status(&self, request: &str) -> bot_core::intake::RequestState {
        self.pg
            .store
            .status(key(request))
            .await
            .unwrap()
            .expect("accepted request")
    }
}

pub fn key(request: &str) -> RequestKey {
    RequestKey {
        profile: ProfileId("personal".into()),
        binding: BindingId("desk".into()),
        principal: PrincipalId(OWNER.into()),
        request: RequestId(request.into()),
    }
}

pub fn command(request: &str, text: &str) -> Accept {
    Accept {
        reply_transport: None,
        event: Envelope {
            profile: ProfileId("personal".into()),
            request: RequestId(request.into()),
            binding: BindingId("desk".into()),
            principal: PrincipalId(OWNER.into()),
            conversation: Conversation {
                kind: ConversationKind::Cli,
                id: "owner".into(),
                thread: None,
            },
            http: None,
            attachments: vec![],
            body: JsonPayload(serde_json::json!({"text": text}).to_string()),
        },
        approver: Some(PrincipalId(OWNER.into())),
        revision: Revision("r1".into()),
        target: Target::Agent {
            agent: AgentId("main".into()),
            instruction: text.into(),
        },
        scopes: set(&["run", "calendar.read", "calendar.delete"]),
    }
}

pub async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let started = std::time::Instant::now();
    while !ready().await {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

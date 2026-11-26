//! 통합 시험 도구: 실제 PG 저장소(시험 전용 스키마)와 실제 BrokerService(가짜 실행기) 위에서 워커·스케줄러를 돌린다.
//! DB는 팀 시험 서버(`BOT_TEST_DATABASE_URL`)만 쓰고, 없으면 시험을 건너뛴다(`BOT_TEST_REQUIRE_DB=1`이면 실패).
//! 시험마다 `runtime_pg_<pid>_<번호>` 스키마를 만들고 끝나면(패닉 포함) 자기 것만 지운다.
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
pub const EVENTS: &str = r#"{"events":[{"id":"e1","title":"산책"}]}"#;

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

    /// 같은 스키마를 다른 소유자 이름으로 여는 저장소(두 번째 워커·스케줄러).
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

    /// run의 상태가 `expected`인지 확인한다. 다르면 그 시점의 run 행(대기 종류·시도·세대·claim·리스·현재 시각)을 메시지에 담아 원인을 가린다
    /// (일시적인 어긋남이 한 번 나도 어느 경로였는지 남도록).
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
    /// 이 시험의 연결만 끊고 자기 스키마를 지운다. 풀을 닫지 않는다(같은 런타임을 막지 않으려고).
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

/// 시험용 프로필 판본 r1: 에이전트 main(스킬 agenda: list·delete, 알림 notify.send)과 자식 main/research, 소유자와 서비스 주체.
pub fn profile() -> Arc<Snapshot> {
    profile_at("r1", false)
}

/// 같은 프로필의 다음 판본 r2: r1에 도구 `agenda.extra`가 더해졌다(r1에 고정된 실행에는 없는 도구다).
pub fn profile_next() -> Arc<Snapshot> {
    profile_at("r2", true)
}

fn profile_at(revision: &str, extra: bool) -> Arc<Snapshot> {
    // 일정 이동 도구는 고정된 입력 스키마가 있다: 이벤트 ID 하나(소문자·숫자·`-`)만, 다른 필드는 받지 않는다
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

/// PG·활성 집합(personal=r1)·브로커를 갖춘 세계. DB가 없으면 `None`(시험 건너뜀).
pub async fn world() -> Option<World> {
    world_with(Arc::new(FakeSealer)).await
}

/// `world`와 같되 인자 봉인기를 고른다(실제 스키마 검증을 보려면 `JcsSealer`).
pub async fn world_with(sealer: Arc<dyn bot_broker::ports::Sealer>) -> Option<World> {
    world_with_routes(sealer, None).await
}

/// 명시 목적지 예약을 쓰는 호스트는 운영 조립과 같이 같은 활성 집합에 고정한 저장소와 등록부를 준다.
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
        // 다른 시험·빌드가 같은 DB와 CPU를 쓰는 동안에도 하트비트 몇 번 늦었다고 리스를 잃은 것으로 보지 않는다
        heartbeat_misses: 12,
        grace: Duration::from_secs(1),
        ..WorkerConfig::default()
    }
}

impl World {
    /// 지정한 저장소 핸들(워커 이름)과 모델 구성으로 워커를 만든다.
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

    /// CLI 대화로 접수한다(요청자 소유자·승인자 소유자·대상 main).
    pub async fn accept(&self, request: &str, text: &str) -> RunId {
        self.pg
            .store
            .accept(command(request, text))
            .await
            .expect("접수")
    }

    /// Slack DM(push) 대화로 접수한다(바인딩 `desk`·발송자 키 `slack`).
    pub async fn accept_slack_dm(&self, request: &str, text: &str) -> RunId {
        let mut cmd = command(request, text);
        cmd.reply_transport = Some(PushTransportId::SLACK);
        cmd.event.conversation.kind = ConversationKind::Direct;
        self.pg.store.accept(cmd).await.expect("접수")
    }

    pub async fn status(&self, request: &str) -> bot_core::intake::RequestState {
        self.pg
            .store
            .status(key(request))
            .await
            .unwrap()
            .expect("접수한 요청")
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

/// 조건이 될 때까지 기다린다(최대 10초).
pub async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let started = std::time::Instant::now();
    while !ready().await {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

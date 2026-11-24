//! 시험 DB 도구. BOT_TEST_DATABASE_URL(팀 시험 서버)이 없으면 시험을 건너뛴다(BOT_TEST_REQUIRE_DB=1이면 실패).
//! 시험마다 `store_pg_<pid>_<번호>` 스키마를 만들고 끝나면(패닉 포함) 자기 것만 지운다.
#![allow(dead_code)]

pub mod admin;
pub mod cards;
pub mod delivery;
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
    /// 활성 판본 집합(personal·work = r1)이 이미 게시된 저장소. 새 외부 접수·시작·발송이 이 판본과 대조되기 때문이다.
    pub async fn new() -> Option<TestDb> {
        Self::with(Config::default()).await
    }

    pub async fn with(cfg: Config) -> Option<TestDb> {
        let db = Self::bare_with(cfg).await?;
        db.publish().await;
        Some(db)
    }

    /// 아무것도 게시하지 않은 저장소(게시·교체를 직접 시험할 때).
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
        // 연결에 시험 이름표를 붙여 두면 정리 때 이 시험의 연결만 끊을 수 있다
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

    /// 표 모양을 DDL로 바꾼 뒤(판본 올리기 시험) 연결 풀을 새로 연다: 풀의 연결들이 옛 모양으로 준비해 둔 문장(`select r.*` 같은 결과 모양이 바뀐 것)을
    /// 다시 쓰면 PG가 `cached plan must not change result type`로 거절하는데, 어느 연결이 걸리는지는 그때그때 달라 시험이 가끔 실패한다.
    pub async fn reconnect(&mut self) {
        let cfg = self.store.config().clone();
        self.store = Self::connect(&self.url, &self.schema, cfg).await;
    }

    /// 같은 스키마를 다른 소유자 이름·설정으로 여는 두 번째 저장소(두 워커·두 스케줄러 흉내).
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

    /// 리스를 이미 끝난 것으로 만든다(잠을 자지 않고 만료를 흉내 낸다).
    pub async fn expire_lease(&self, run: &RunId) {
        self.sql(&format!(
            "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
            run.0
        ))
        .await;
    }

    /// 재시도 대기를 풀어 바로 집을 수 있게 한다.
    pub async fn make_due(&self, run: &RunId) {
        self.sql(&format!(
            "update bot_runs set not_before_ms = 0 where id = '{}'",
            run.0
        ))
        .await;
    }
}

impl Drop for TestDb {
    /// 시험이 끝나면(패닉 포함) 이 시험의 연결만 끊고 자기 스키마를 지운다. 풀을 닫지 않는다(같은 런타임을 막지 않으려고).
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

// Store는 여러 포트를 한 타입으로 구현해 같은 이름의 메서드(claim·begin·finish·status)가 겹친다.
// 시험은 다른 크레이트처럼 트레이트 객체로 경계를 그대로 쓴다.
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

/// CLI 대화의 접수 명령: 프로필 personal, 바인딩 desk, 요청자 local:owner, 대상 main.
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
    // HTTP 대화는 인증된 어댑터의 경로 정보가 함께 온다
    let http = (kind == ConversationKind::Http).then(|| HttpRoute {
        method: "POST".into(),
        path: "/agenda".into(),
        response: bot_core::profile::ResponseMode::Accepted,
    });
    Accept {
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

/// 예약 발생용 접수 명령: 대화 종류 Scheduled, 요청자 service:morning.
pub fn occurrence_accept(
    profile: &str,
    trigger: &str,
    at_ms: i64,
    target: Target,
    scopes: &[&str],
) -> Accept {
    Accept {
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
    /// 방금 등록한 트리거를 한 시간 전부터 있던 것처럼 되돌려, 과거 발생을 접수할 수 있게 한다.
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

// ---- 호출·승인 원장 시험 도구 ----

pub fn profile_id() -> ProfileId {
    ProfileId("personal".into())
}

pub fn call(id: &str) -> CallId {
    CallId(id.into())
}

pub fn tool(id: &str) -> ToolId {
    ToolId(id.into())
}

/// 접수·claim을 거친 실행 중 run(CLI 대화, 요청자 local:owner, 승인자 미지정).
pub async fn leased(db: &TestDb, request: &str) -> RunContext {
    db.publish().await;
    runs(db)
        .accept(accept("personal", request, "hi"))
        .await
        .unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

/// 지정 대화 종류로 접수·claim한 실행 중 run.
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

/// 승인 결합값(브로커가 지금 상태로 계산하는 값 모양). 만료는 DB 시각보다 한 시간 뒤다.
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
    // 예약 생성 카드는 봉인한 정규 인자를 `인자` 행에 싣고 그 SHA-256이 승인·호출 지문과 같아야 열린다: 지문에 맞는 카드는 `schedule::open_create`로 만든다
    assert_ne!(
        approval.tool.0, "schedule.create",
        "schedule.create 카드는 support::schedule::open_create로 만든다"
    );
    let card = format!(
        r#"{{"approval":{{"nonce":"{}","title":"삭제 승인","lines":["일정 1건"]}}}}"#,
        approval.nonce.0
    );
    OpenApproval {
        approval,
        reasons: vec!["irreversible".into()],
        card: JsonPayload(card),
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
    /// 이 시험의 정적 예약도 실제 활성 집합 게시 트랜잭션으로 설치한다.
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

    /// 시작·발송·새 접수 검사가 읽는 활성 판본 집합을 게시한다(personal·work = r1). 이미 게시됐으면 아무것도 하지 않는다.
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

    /// 이 시험의 스키마에 붙는 별도 연결(교체·플래그 변경이 진행 중인 상황을 흉내 낼 때 트랜잭션을 직접 쥔다).
    pub async fn raw(&self) -> sqlx::PgConnection {
        let options = PgConnectOptions::from_str(&self.url)
            .unwrap()
            .options([("search_path", self.schema.as_str())])
            .application_name(&self.schema);
        options.connect().await.unwrap()
    }

    /// 리스를 끝내고 다른 소유자가 인계한다(세대가 오른다).
    pub async fn takeover(&self, run: &RunId) -> RunContext {
        self.expire_lease(run).await;
        runs(self).claim().await.unwrap().unwrap()
    }
}

/// 자발적 대기에서 정상 재개한 attempt: 같은 run·같은 세대이고 claim 토큰만 올라간다.
pub fn assert_resumed(before: &Attempt, after: &Attempt) {
    assert_eq!(
        (&after.run, after.generation),
        (&before.run, before.generation),
        "정상 재개는 같은 세대다"
    );
    assert!(
        after.claim.map(|c| c.0) > before.claim.map(|c| c.0),
        "claim 토큰은 재개마다 오른다: {before:?} → {after:?}"
    );
}

pub fn memories(db: &TestDb) -> &dyn bot_core::port::MemoryStore {
    &db.store
}

// ---- 기억 시험 공용 도구 ----

pub const OWNER: &str = "local:owner";
pub const GUEST: &str = "slack:home/U2";

/// 접수·claim한 실행 중 run. 요청자와 대화 종류를 고른다.
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

//! 실제 브로커+실제 PG(고정 핸들)로 확인하는 수신자 지정 생성의 첫 카드 경합: 브로커가 **마지막으로** 등록부를 확인한 뒤 카드를 기록하기 전에 다른 프로세스의 집합 교체가
//! 커밋되면, 카드를 기록하는 커밋이 활성 집합 잠금 아래에서 낡은 집합을 보고 거절한다(카드 0·예약 0). 교체가 없으면 같은 흐름이 카드를 연다.

mod support;

use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot},
};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    port::{Broker, Error},
    schedule::{DeliveryTarget, ScheduleRoutes},
    state::{Conversation, ConversationKind},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use support::*;

const OWNER: &str = "local:owner";
const TOOL: &str = "schedule.create";
const ARGS: &str = r##"{"at":"2030-01-01T12:00:00Z","instruction":"보고","destination":{"binding":"slack","conversation":{"kind":"channel","id":"#general","thread":"t1"}}}"##;

/// 한 곳의 등록부: `#general`을 정규 경로 `T1/C9`로 확정한다. `race_at`번째 확인에 답한 **직후** 집합을 교체한다(그 뒤 카드를 기록한다).
struct RacingRoutes {
    asked: AtomicUsize,
    race_at: Option<usize>,
    replace: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl ScheduleRoutes for RacingRoutes {
    fn resolve(&self, _: &ProfileId, target: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        let nth = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let answer = DeliveryTarget {
            binding: target.binding.clone(),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: "T1/C9".into(),
                thread: target.conversation.thread.clone(),
            },
        };
        if Some(nth) == self.race_at
            && let Some(replace) = self.replace.lock().unwrap().as_ref()
        {
            replace(); // 다른 프로세스가 새 집합을 게시했다(이 확인에는 이미 답했다)
        }
        Ok(answer)
    }
}

fn broker(db: &TestDb, routes: Arc<RacingRoutes>) -> BrokerService {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", TOOL])
            .member(OWNER, &["owner"])
            .policy_scopes(&["run", TOOL])
            .agent("main")
            .build(),
    );
    let pg = Arc::new(db.store.for_activation(Revision("set-1".into())));
    BrokerService::new(
        Parts {
            catalog,
            ledger: pg.clone(),
            effects: pg.clone(),
            memory: pg.clone(),
            audit: pg.clone(),
            runner: Arc::new(FakeRunner::new()),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(pg)
    .with_routes(routes)
}

async fn owner_attempt(db: &TestDb) -> Attempt {
    db.publish().await;
    // 1:1 대화(Slack DM 같은 입구)에서 온 소유자: 수신자를 지정할 수 있는 입구다
    let mut command = accept_as(
        "personal",
        "morning",
        OWNER,
        ConversationKind::Direct,
        "owner",
        "보고 예약",
    );
    command.scopes = scopes(&["run", TOOL]);
    command.approver = Some(PrincipalId(OWNER.into()));
    runs(db).accept(command).await.unwrap();
    runs(db).claim().await.unwrap().unwrap().attempt
}

fn request(attempt: &Attempt) -> ToolCall {
    ToolCall {
        attempt: attempt.clone(),
        call: call("c1"),
        tool: tool(TOOL),
        args: JsonPayload(ARGS.into()),
    }
}

async fn counts(db: &TestDb) -> (i64, i64) {
    (
        db.scalar("select count(*) from bot_approvals").await,
        db.scalar("select count(*) from bot_user_schedules").await,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_between_the_last_registry_check_and_the_card_leaves_no_card_and_no_schedule()
{
    let Some(db) = TestDb::new().await else {
        return;
    };
    let routes = Arc::new(RacingRoutes {
        asked: AtomicUsize::new(0),
        // 1번째 확인은 호출을 읽을 때, 2번째가 카드를 열기 직전의 마지막 확인이다
        race_at: Some(2),
        replace: Mutex::new(None),
    });
    let service = broker(&db, routes.clone());
    let attempt = owner_attempt(&db).await;
    let pool = db.store.pool().clone();
    let handle = tokio::runtime::Handle::current();
    *routes.replace.lock().unwrap() = Some(Box::new(move || {
        let pool = pool.clone();
        tokio::task::block_in_place(|| {
            handle.block_on(async move {
                sqlx::query("update bot_active set revision = 'set-2'")
                    .execute(&pool)
                    .await
                    .unwrap();
            })
        });
    }));
    let result = service.call(request(&attempt)).await;
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!(routes.asked.load(Ordering::SeqCst), 2);
    assert_eq!(counts(&db).await, (0, 0), "카드 0·예약 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_replacement_the_same_flow_opens_the_routed_card_for_the_canonical_recipient() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let routes = Arc::new(RacingRoutes {
        asked: AtomicUsize::new(0),
        race_at: None,
        replace: Mutex::new(None),
    });
    let service = broker(&db, routes.clone());
    let attempt = owner_attempt(&db).await;
    let result = service.call(request(&attempt)).await.unwrap();
    let CallResult::Awaiting(nonce) = result else {
        panic!("카드가 열려야 한다: {result:?}")
    };
    assert_eq!(counts(&db).await, (1, 0), "카드 1·예약은 승인 전이라 0");
    let stored = calls(&db).approval(nonce).await.unwrap();
    // 카드는 모델이 쓴 이름이 아니라 등록부가 확정한 정규 경로를 보여 준다
    assert!(stored.card.0.contains("T1/C9"), "{}", stored.card.0);
    assert!(!stored.card.0.contains("#general"), "{}", stored.card.0);
}

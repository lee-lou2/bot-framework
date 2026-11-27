//! Schedule-create approval card activation-fence race against real broker and PostgreSQL.

use crate::support::*;
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

const OWNER: &str = "local:owner";
const TOOL: &str = "schedule.create";
const ARGS: &str = r##"{"at":"2030-01-01T12:00:00Z","instruction":"report","destination":{"binding":"slack","conversation":{"kind":"channel","id":"#general","thread":"t1"}}}"##;

/// Resolves `#general` to canonical `T1/C9`, swapping the active set right after the `race_at`-th lookup.
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
            replace();
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
    let mut command = accept_as(
        "personal",
        "morning",
        OWNER,
        ConversationKind::Direct,
        "owner",
        "schedule report",
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
        // Lookup 1 occurs while reading the call; lookup 2 is the final check right before opening the card.
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
    assert_eq!(
        counts(&db).await,
        (0, 0),
        "expected no card and no schedule"
    );
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
        panic!("expected approval card: {result:?}")
    };
    assert_eq!(
        counts(&db).await,
        (1, 0),
        "expected 1 card and 0 schedules before approval"
    );
    let stored = calls(&db).approval(nonce).await.unwrap();
    // Card must show the canonical resolved route, not the model-supplied alias.
    assert!(stored.card.0.contains("T1/C9"), "{}", stored.card.0);
    assert!(!stored.card.0.contains("#general"), "{}", stored.card.0);
}

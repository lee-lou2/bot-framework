//! Approval expiry or replacement right before delivery consume is not a lost claim (real broker + real PG).

use crate::support::{delivery::*, *};
use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot},
};
use bot_core::{
    command::{CallResult, Finish},
    id::*,
    ledger::{ApprovalState, Decision, StartCall},
    port::{Error, Reply},
    schedule::{
        DeliveryDraft, DeliveryFence, DeliveryTarget, ScheduleDelivery, ScheduleRoutes,
        ScheduledReplies, UnsentReason,
    },
    state::*,
};
use bot_store::Store;
use std::sync::{Arc, Mutex};

const SCOPE: &str = "schedule.create";

struct SameRoute;

impl ScheduleRoutes for SameRoute {
    fn resolve(&self, _: &ProfileId, target: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        Ok(target.clone())
    }
}

/// Injects one SQL mutation immediately before `enqueue` to simulate a concurrent card change.
struct Racing {
    inner: Arc<Store>,
    pool: sqlx::PgPool,
    before_enqueue: Mutex<Option<&'static str>>,
}

impl Racing {
    fn arm(&self, sql: &'static str) {
        *self.before_enqueue.lock().unwrap() = Some(sql);
    }
}

impl ScheduledReplies for Racing {
    fn receipt(&self, a: Attempt) -> Reply<'_, Option<bot_core::command::JsonPayload>> {
        ScheduledReplies::receipt(&*self.inner, a)
    }
    fn draft(&self, a: Attempt, done: Option<Finish>) -> Reply<'_, Option<DeliveryDraft>> {
        ScheduledReplies::draft(&*self.inner, a, done)
    }
    fn target(&self, a: Attempt) -> Reply<'_, Option<DeliveryTarget>> {
        ScheduledReplies::target(&*self.inner, a)
    }
    fn finish_unsent(
        &self,
        fence: DeliveryFence,
        target: DeliveryTarget,
        reason: UnsentReason,
    ) -> Reply<'_, bot_core::command::JsonPayload> {
        ScheduledReplies::finish_unsent(&*self.inner, fence, target, reason)
    }
    fn enqueue(&self, approval: StartCall) -> Reply<'_, ()> {
        Box::pin(async move {
            let sql = self.before_enqueue.lock().unwrap().take();
            if let Some(sql) = sql {
                sqlx::query(sql).execute(&self.pool).await.unwrap();
            }
            ScheduledReplies::enqueue(&*self.inner, approval).await
        })
    }
}

fn broker(pg: &Arc<Store>, racing: Arc<Racing>) -> BrokerService {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", SCOPE])
            .member(OWNER, &["owner"])
            .policy_scopes(&["run", SCOPE])
            .agent("main")
            .build(),
    );
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
    .with_schedules(pg.clone())
    .with_replies(racing)
    .with_routes(Arc::new(SameRoute))
}

struct Setup {
    db: TestDb,
    pg: Arc<Store>,
    racing: Arc<Racing>,
    service: BrokerService,
    resumed: Attempt,
    first_nonce: ApprovalId,
}

async fn approved_and_resumed() -> Option<Setup> {
    let db = TestDb::new().await?;
    let pg = Arc::new(db.store.for_activation(Revision("set-1".into())));
    let racing = Arc::new(Racing {
        inner: pg.clone(),
        pool: db.store.pool().clone(),
        before_enqueue: Mutex::new(None),
    });
    let service = broker(&pg, racing.clone());
    let mut command = accept_as(
        "personal",
        "morning",
        OWNER,
        ConversationKind::Direct,
        "owner",
        "scheduled report",
    );
    command.scopes = scopes(&["run", SCOPE]);
    command.approver = Some(PrincipalId(OWNER.into()));
    runs(&db).accept(command).await.unwrap();
    let attempt = runs(&db).claim().await.unwrap().unwrap().attempt;
    db.sql(&format!(
        "insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id, dest_thread)
         values ('personal', '{}', 'slack', 'direct', 'D9', 't1')",
        attempt.run.0
    ))
    .await;
    ScheduledReplies::draft(&*pg, attempt.clone(), Some(answers(&["morning summary"])))
        .await
        .unwrap()
        .expect("draft");
    let CallResult::Awaiting(first_nonce) = service.release(attempt.clone()).await.unwrap() else {
        panic!("expected Awaiting approval");
    };
    calls(&db)
        .decide(Decision {
            nonce: first_nonce.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().expect("resumed").attempt;
    Some(Setup {
        db,
        pg,
        racing,
        service,
        resumed,
        first_nonce,
    })
}

async fn approval_state(db: &TestDb, nonce: &ApprovalId) -> ApprovalState {
    calls(db).approval(nonce.clone()).await.unwrap().state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_that_expires_right_before_the_consume_is_not_a_lost_claim_and_ends_as_an_expired_approval()
 {
    let Some(s) = approved_and_resumed().await else {
        return;
    };
    // Expire by DB clock right before consume after the broker saw the approval as live.
    s.racing
        .arm("update bot_approvals set expires_ms = bot_now_ms() - 1");
    let got = s.service.release(s.resumed.clone()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    // Claim and active-set fence remain valid: Stale came from the card, not ownership loss.
    assert!(
        ScheduledReplies::target(&*s.pg, s.resumed.clone())
            .await
            .unwrap()
            .is_some()
    );
    // Store closed the expired approval so re-evaluation sees Expired even if host clock lags.
    assert_eq!(
        approval_state(&s.db, &s.first_nonce).await,
        ApprovalState::Expired
    );
    assert_eq!(outbox_rows(&s.db, "D9").await, 0);
    let again = s.service.release(s.resumed.clone()).await;
    assert!(matches!(again, Err(Error::Denied)), "{again:?}");
    assert_eq!(
        s.db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'call.deny' and verdict = 'approval_expired'"
        )
        .await,
        1,
        "recorded as approval_expired"
    );
    assert_eq!(outbox_rows(&s.db, "D9").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_replaced_right_before_the_consume_is_judged_again_on_the_same_claim_and_a_new_card_opens()
 {
    let Some(s) = approved_and_resumed().await else {
        return;
    };
    s.racing
        .arm("update bot_approvals set state = 'superseded'");
    let got = s.service.release(s.resumed.clone()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert!(
        ScheduledReplies::target(&*s.pg, s.resumed.clone())
            .await
            .unwrap()
            .is_some(),
        "claim is still valid"
    );
    let again = s.service.release(s.resumed.clone()).await.unwrap();
    let CallResult::Awaiting(second) = again else {
        panic!("{again:?}");
    };
    assert_ne!(second, s.first_nonce);
    assert_eq!(
        approval_state(&s.db, &s.first_nonce).await,
        ApprovalState::Superseded
    );
    assert_eq!(approval_state(&s.db, &second).await, ApprovalState::Pending);
    assert_eq!(state_of_run(&s.db, &s.resumed.run).await, "waiting");
    assert_eq!(outbox_rows(&s.db, "D9").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_really_lost_claim_or_activation_set_is_stale_for_the_fence_too() {
    let Some(s) = approved_and_resumed().await else {
        return;
    };
    s.db.expire_lease(&s.resumed.run).await;
    let new = s.db.takeover(&s.resumed.run).await;
    assert_ne!(new.attempt.claim, s.resumed.claim);
    let got = s.service.release(s.resumed.clone()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    let fence = ScheduledReplies::target(&*s.pg, s.resumed.clone()).await;
    assert!(matches!(fence, Err(Error::Stale)));
    assert!(
        ScheduledReplies::target(&*s.pg, new.attempt.clone())
            .await
            .is_ok()
    );
    s.db.sql("update bot_active set revision = 'set-2' where singleton")
        .await;
    let fence = ScheduledReplies::target(&*s.pg, new.attempt.clone()).await;
    assert!(matches!(fence, Err(Error::Stale)));
}

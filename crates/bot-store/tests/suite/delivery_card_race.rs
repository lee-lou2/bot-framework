//! 소비 직전의 카드 변화는 소유권 상실이 아니다(실제 브로커+실제 PG): 소유자가 승인한 뒤 브로커가 승인을 소비하려는 순간 그 승인이 만료됐거나 다른 카드로 대체되면 저장소는
//! `Stale`이다(커밋 없음). 워커는 이 `Stale`로 물러나면 안 된다: 같은 펜스(현재 claim·활성 집합)를 고정 수신자 읽기(`target`)로 확인해 **펜스가 유효하면** 같은 claim에서
//! 다시 판정한다 — 만료한 승인은 승인 만료로 닫히고(`Denied`), 대체된 카드는 새 카드를 열어 승인을 다시 기다린다(`Awaiting`). 진짜 claim·활성 집합을 잃었으면 `target`도 `Stale`이다.

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

/// 물은 경로를 그대로 확정하는 등록부(이 시험은 수신자 철회를 보지 않는다).
struct SameRoute;

impl ScheduleRoutes for SameRoute {
    fn resolve(&self, _: &ProfileId, target: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        Ok(target.clone())
    }
}

/// 실제 저장소 포트에 얹는 경합: `enqueue` 직전에 한 번 SQL을 실행한다(다른 프로세스가 그 사이 카드를 바꿨다). 나머지는 그대로 저장소다.
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
    /// 소유자가 카드를 승인해 재개한 시도
    resumed: Attempt,
    first_nonce: ApprovalId,
}

/// 예약 발화 run 하나: 초안 동결 → 브로커가 카드를 열고(승인 대기) → 소유자가 승인 → 재개한 시도. 이 시점에서 승인은 `Approved`이고 아직 소비되지 않았다.
async fn approved_and_resumed() -> Option<Setup> {
    let db = TestDb::new().await?;
    // 활성 집합을 고정한 핸들만 브로커의 이 입구를 쓴다(운영 호스트와 같다)
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
        "보고 예약",
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
    ScheduledReplies::draft(&*pg, attempt.clone(), Some(answers(&["아침 요약"])))
        .await
        .unwrap()
        .expect("초안");
    let CallResult::Awaiting(first_nonce) = service.release(attempt.clone()).await.unwrap() else {
        panic!("카드를 열고 승인을 기다려야 한다");
    };
    calls(&db)
        .decide(Decision {
            nonce: first_nonce.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().expect("재개").attempt;
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
    // 브로커가 승인을 살아 있다고 본 뒤 소비하기 직전에 DB 시계로 만료됐다
    s.racing
        .arm("update bot_approvals set expires_ms = bot_now_ms() - 1");
    let got = s.service.release(s.resumed.clone()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    // 펜스(현재 claim·활성 집합)는 유효하다: `Stale`은 카드 때문이다
    assert!(
        ScheduledReplies::target(&*s.pg, s.resumed.clone())
            .await
            .unwrap()
            .is_some()
    );
    // 저장소는 만료한 승인을 닫아 두었다(호스트 시계가 여전히 살아 있다고 보더라도 다시 판정하는 쪽이 만료를 읽는다)
    assert_eq!(
        approval_state(&s.db, &s.first_nonce).await,
        ApprovalState::Expired
    );
    // 소비도 발송도 없었다
    assert_eq!(outbox_rows(&s.db, "D9").await, 0);
    // 같은 claim에서 다시 판정하면 승인 만료로 끝난다(`Denied`): 소유권 상실이 아니다
    let again = s.service.release(s.resumed.clone()).await;
    assert!(matches!(again, Err(Error::Denied)), "{again:?}");
    assert_eq!(
        s.db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'call.deny' and verdict = 'approval_expired'"
        )
        .await,
        1,
        "승인 만료로 기록한다"
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
        "claim은 유효하다"
    );
    // 같은 claim에서 다시 판정하면 새 카드를 열고 승인을 다시 기다린다
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
    // 소유자 시도는 리스 만료 뒤 다른 워커가 이어받았다: 옛 claim은 입구도 펜스도 `Stale`이다
    s.db.expire_lease(&s.resumed.run).await;
    let new = s.db.takeover(&s.resumed.run).await;
    assert_ne!(new.attempt.claim, s.resumed.claim);
    let got = s.service.release(s.resumed.clone()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    let fence = ScheduledReplies::target(&*s.pg, s.resumed.clone()).await;
    assert!(matches!(fence, Err(Error::Stale)));
    // 새 claim의 펜스는 유효하다
    assert!(
        ScheduledReplies::target(&*s.pg, new.attempt.clone())
            .await
            .is_ok()
    );
    // 활성 집합을 잃은 핸들도 펜스가 `Stale`이다(고정한 집합이 더는 현재가 아니다)
    s.db.sql("update bot_active set revision = 'set-2' where singleton")
        .await;
    let fence = ScheduledReplies::target(&*s.pg, new.attempt.clone()).await;
    assert!(matches!(fence, Err(Error::Stale)));
}

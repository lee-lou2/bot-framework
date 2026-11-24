//! 다른 수신자에게 보낼 예약 회신을 실제 브로커로 승인하고 PG에 등록한다.
mod support;

use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, MemRoutes, RecordingTracer, snapshot},
};
use bot_core::{
    command::{CallResult, Finish, JsonPayload},
    id::{Attempt, PrincipalId, Revision},
    ledger::StartCall,
    port::{Broker, Error, Reply},
    schedule::{DeliveryDraft, ScheduleDelivery, ScheduledReplies},
    state::ConversationKind,
};
use sqlx::query;
use std::sync::Arc;
use support::{delivery::outbox_rows, *};

fn broker(db: &TestDb, activation: &str) -> BrokerService {
    let catalog = Arc::new(FakeCatalog::new());
    let grants = ["run", "schedule.create"];
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &grants)
            .member(OWNER, &["owner"])
            .policy_scopes(&grants)
            .agent("main")
            .build(),
    );
    let pg = Arc::new(db.store.for_activation(Revision(activation.into())));
    // 같은 프로필의 호스트 등록부: 발화가 고정한 목적지(Slack 채널의 정규 경로 `팀ID/채널ID`)가 등록돼 있다. 등록부가 없으면 브로커는 전송 카드를 열지 않는다.
    let routes = Arc::new(MemRoutes::new());
    routes.register(
        "personal",
        "slack",
        ConversationKind::Channel,
        "T1/C9",
        "T1/C9",
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
    .with_replies(pg)
    .with_routes(routes)
}

fn receipt(result: Result<CallResult, Error>) -> String {
    match result {
        Ok(CallResult::Done(payload)) => payload.0,
        Ok(_) => panic!("승인된 초안의 등록 영수증이 필요하다"),
        Err(error) => panic!("승인된 초안의 등록 영수증 조회 실패: {error:?}"),
    }
}

async fn approve(db: &TestDb, service: &BrokerService) -> (Attempt, Attempt) {
    let mut request = accept("personal", "delivery", "예약 회신");
    request.scopes = scopes(&["run", "schedule.create"]);
    request.approver = Some(PrincipalId(OWNER.into()));
    let id = runs(db).accept(request).await.unwrap();
    let first = runs(db).claim().await.unwrap().unwrap().attempt;
    // 발화 시 저장하는 목적지만 준비한다. 초안·승인·발송 등록은 모두 실제 포트를 거친다.
    query("insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id) values ('personal', $1, 'slack', 'channel', 'T1/C9')")
        .bind(&id.0)
        .execute(db.store.pool())
        .await
        .unwrap();
    delivery::replies(db)
        .draft(first.clone(), Some(done("승인한 답")))
        .await
        .unwrap()
        .unwrap();
    let Ok(CallResult::Awaiting(nonce)) = service.release(first.clone()).await else {
        panic!("소유자의 승인이 필요하다")
    };
    assert_eq!(outbox_rows(db, "T1/C9").await, 0);
    service
        .decide(nonce, PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let last = runs(db).claim().await.unwrap().unwrap().attempt;
    (first, last)
}

#[tokio::test]
async fn the_final_claim_can_recover_the_committed_receipt_after_the_run_ends() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let service = broker(&db, "set-1");
    let (first, last) = approve(&db, &service).await;
    let issued = receipt(service.release(last.clone()).await);
    assert_eq!(state_of_run(&db, &last.run).await, "succeeded");
    assert!(matches!(
        calls(&db).context(last.clone()).await,
        Err(Error::Stale)
    ));
    let recorded = db
        .scalar::<String>(
            "select result from bot_calls where call = 'schedule-deliver' and state = 'done'",
        )
        .await;
    assert_eq!(issued, recorded);

    // 응답을 잃은 호스트는 새 브로커로 같은 영수증만 얻는다.
    let restarted = broker(&db, "set-1");
    assert_eq!(receipt(restarted.release(last.clone()).await), recorded);
    for other in [
        first,
        Attempt {
            claim: None,
            ..last.clone()
        },
    ] {
        assert!(matches!(
            restarted.release(other).await,
            Err(Error::Stale | Error::Denied)
        ));
    }
    db.sql("update bot_active set revision = 'set-2'").await;
    for changed in [restarted, broker(&db, "set-2")] {
        assert!(matches!(
            changed.release(last.clone()).await,
            Err(Error::Stale | Error::Conflict | Error::Denied)
        ));
    }
    assert_eq!(outbox_rows(&db, "T1/C9").await, 1);
}

/// PG 커밋까지 실행한 뒤 호출자가 응답을 받지 못하게 한다.
struct LostEnqueueReply(bot_store::Store);

impl ScheduledReplies for LostEnqueueReply {
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<JsonPayload>> {
        self.0.receipt(attempt)
    }

    fn draft(&self, attempt: Attempt, finish: Option<Finish>) -> Reply<'_, Option<DeliveryDraft>> {
        self.0.draft(attempt, finish)
    }

    fn enqueue(&self, approval: StartCall) -> Reply<'_, ()> {
        Box::pin(async move {
            self.0.enqueue(approval).await?;
            Err(Error::Unavailable)
        })
    }
}

#[tokio::test]
async fn a_lost_enqueue_response_is_recovered_without_a_second_queue_entry() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let handle = db.store.for_activation(Revision("set-1".into()));
    let service = broker(&db, "set-1").with_replies(Arc::new(LostEnqueueReply(handle)));
    let (_, last) = approve(&db, &service).await;
    let reply = service.release(last.clone()).await;
    assert!(matches!(
        reply,
        Err(Error::Unavailable) | Ok(CallResult::Done(_))
    ));
    assert_eq!(state_of_run(&db, &last.run).await, "succeeded");
    let recorded = db
        .scalar::<String>("select result from bot_calls where call = 'schedule-deliver'")
        .await;
    let restarted = broker(&db, "set-1");
    assert_eq!(receipt(restarted.release(last).await), recorded);
    assert_eq!(outbox_rows(&db, "T1/C9").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.enqueue'")
            .await,
        1
    );
}

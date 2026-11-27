use super::*;
use bot_broker::{
    ports::{Ran, ToolJob, ToolRunner},
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeSealer, RecordingTracer, snapshot, tool as declaration},
};
use bot_core::{
    command::{CallResult, EffectRequest, JsonPayload, ToolCall},
    port::{Broker, EffectJournal, Reply},
    state::{EffectState, Risk},
};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

/// Writes to a local TCP sink when granted an effect permit and records the returned receipt;
/// on a second run it checks the same step then attempts a second step name.
struct PostingTool {
    store: bot_store::Store,
    addr: SocketAddr,
    calls: AtomicUsize,
    recorded: Notify,
    interrupted: bool,
}
impl ToolRunner for PostingTool {
    fn installed(&self, _: &str) -> Option<String> {
        Some("img1".into())
    }
    fn reap(&self, _: String) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn run(&self, job: ToolJob) -> Reply<'_, Ran> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            for name in if n == 0 {
                &["post"][..]
            } else {
                &["post", "post-again"][..]
            } {
                let permit = self
                    .store
                    .request(EffectRequest {
                        attempt: job.attempt.clone(),
                        call: job.call.clone(),
                        step: (*name).into(),
                        args: JsonPayload("{}".into()),
                    })
                    .await?;
                if permit.send {
                    let mut conn = TcpStream::connect(self.addr).await.unwrap();
                    conn.write_u8(1).await.unwrap();
                    let receipt = conn.read_u8().await.unwrap();
                    let mut status = permit.status;
                    status.state = EffectState::Confirmed;
                    status.receipt = Some(format!("post-{receipt}"));
                    self.store
                        .record(job.attempt.clone(), job.call.clone(), status)
                        .await?;
                }
            }
            self.recorded.notify_one();
            if self.interrupted && n == 0 {
                std::future::pending::<()>().await;
            }
            Ok(Ran::Unknown("response_unknown"))
        })
    }
}

fn broker(db: &TestDb, runner: Arc<PostingTool>) -> BrokerService {
    let catalog = Arc::new(FakeCatalog::new());
    let mut tool = declaration(Risk::Write, &["run"]);
    tool.effects = true;
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run"])
            .member(OWNER, &["owner"])
            .policy_scopes(&["run"])
            .tool("main", "fixture.post", tool)
            .build(),
    );
    let pg = Arc::new(db.store.clone());
    BrokerService::new(
        Parts {
            catalog,
            ledger: pg.clone(),
            effects: pg.clone(),
            memory: pg.clone(),
            audit: pg,
            runner,
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
}

fn post(at: &Attempt) -> ToolCall {
    ToolCall {
        attempt: at.clone(),
        call: call("c1"),
        tool: tool("fixture.post"),
        args: JsonPayload("{}".into()),
    }
}

async fn exercise(interrupted: bool) {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let delivered = Arc::new(AtomicUsize::new(0));
    let count = delivered.clone();
    let receiving = tokio::spawn(async move {
        loop {
            let (mut c, _) = listener.accept().await.unwrap();
            assert_eq!(c.read_u8().await.unwrap(), 1);
            c.write_u8((count.fetch_add(1, Ordering::SeqCst) + 1) as u8)
                .await
                .unwrap();
        }
    });
    let runner = Arc::new(PostingTool {
        store: db.store.clone(),
        addr,
        calls: AtomicUsize::new(0),
        recorded: Notify::new(),
        interrupted,
    });
    let at = leased(&db, "final").await.attempt;
    let service = broker(&db, runner.clone());
    let command = post(&at);
    let sending = tokio::spawn(async move { service.call(command).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        runner.recorded.notified(),
    )
    .await
    .unwrap();
    if interrupted {
        sending.abort();
        assert!(sending.await.unwrap_err().is_cancelled());
        let next = db.takeover(&at.run).await;
        let release = FinishCall {
            attempt: next.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Released,
        };
        assert!(matches!(
            calls(&db).finish(release).await,
            Err(Error::Conflict)
        ));
        assert!(matches!(
            broker(&db, runner.clone())
                .call(post(&next.attempt))
                .await
                .unwrap(),
            CallResult::Held(_)
        ));
    } else {
        assert!(matches!(
            sending.await.unwrap().unwrap(),
            CallResult::Held(_)
        ));
    }
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        1,
        "must not re-send after losing the final result"
    );
    assert_eq!(state_of_run(&db, &at.run).await, "waiting");
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), None).await;
    let views = db.store.holds(&operator(), None).await.unwrap();
    assert_eq!(views[0].steps[0].receipt.as_deref(), Some("post-1"));
    assert!(matches!(
        db.store
            .resolve_hold(
                operator().principal,
                target.clone(),
                HoldVerdict::NotExecuted {
                    evidence: "no record".into()
                }
            )
            .await,
        Err(Error::Invalid)
    ));
    assert_eq!(
        confirm(&db, target.clone()).await.unwrap(),
        HoldDecision::Applied
    );
    let next = runs(&db).claim().await.unwrap().unwrap();
    let restart = broker(&db, runner.clone());
    for _ in 0..2 {
        let CallResult::Done(actual) = restart.call(post(&next.attempt)).await.unwrap() else {
            panic!("expected final receipt")
        };
        assert_eq!(actual.0, r#"{"receipt":"receipt-1"}"#);
    }
    assert_eq!(confirm(&db, target).await.unwrap(), HoldDecision::Repeated);
    assert_eq!(
        db.scalar::<String>("select receipt from bot_effects").await,
        "post-1"
    );
    assert_eq!(delivered.load(Ordering::SeqCst), 1);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    receiving.abort();
}

#[tokio::test]
async fn confirmed_steps_with_lost_final_result_have_a_resolvable_call_hold() {
    exercise(false).await;
}

#[tokio::test]
async fn an_interrupted_write_with_a_confirmed_step_cannot_send_again_under_a_new_step_name() {
    exercise(true).await;
}

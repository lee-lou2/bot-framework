//! 운영자 API, 조건부 전이, 감사 재전송을 실제 PostgreSQL에서 확인한다.
mod support;

use bot_admin::{AdminConfig, AdminDeps, StaticTokens, fake, router};
use bot_core::{
    command::{Completion, Delegate, Failure},
    id::{AgentId, Attempt, Revision, RunId},
    ledger::{FinishCall, Outcome},
    port::DelegationStore,
    state::ConversationKind,
};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{admin::*, *};

const OPERATOR: &str = "test-operator";
const MEMBER: &str = "test-member";

struct Http {
    client: Client,
    root: String,
    csrf: String,
    member_csrf: String,
    checks: Arc<AtomicUsize>,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Http {
    async fn start(db: &TestDb) -> Self {
        let listener = bot_admin::bind("127.0.0.1:0".parse().unwrap(), false)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let root = format!("http://{addr}/v1/admin");
        let checks = Arc::new(AtomicUsize::new(0));
        let counter = checks.clone();
        let cfg = AdminConfig::new(addr).fence(move || {
            let left =
                counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
            Box::pin(async move { left != Ok(1) })
        });
        let store = Arc::new(db.store.for_activation(Revision("set-1".into())));
        let app = router(
            cfg,
            AdminDeps {
                read: store.clone(),
                act: store,
                broker: Arc::new(fake::Fake::demo()),
                profiles: Arc::new(fake::demo_catalog()),
                auth: Arc::new(
                    StaticTokens::new()
                        .with(OPERATOR, operator())
                        .with(MEMBER, owner()),
                ),
            },
        )
        .unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(bot_admin::serve(listener, app, async {
            let _ = stopped.await;
        }));
        let client = Client::builder().no_proxy().build().unwrap();
        let csrf_for = async |who| {
            Self::get(&client, &format!("{root}/me"), who).await["csrf"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let (csrf, member_csrf) = (csrf_for(OPERATOR).await, csrf_for(MEMBER).await);
        Self {
            client,
            root,
            csrf,
            member_csrf,
            checks,
            stop,
            server,
        }
    }

    async fn get(client: &Client, url: &str, token: &str) -> Value {
        client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    fn run_url(&self, run: &RunId) -> String {
        format!("{}/runs/personal/{}", self.root, run.0)
    }

    async fn detail(&self, run: &RunId) -> Value {
        Self::get(&self.client, &self.run_url(run), OPERATOR).await
    }

    async fn verify_audit(&self) -> Value {
        Self::get(
            &self.client,
            &format!("{}/audit/verify", self.root),
            OPERATOR,
        )
        .await
    }

    async fn post(
        &self,
        run: &RunId,
        action: &str,
        who: &str,
        csrf: Option<&str>,
        command: &Value,
    ) -> Response {
        let mut req = self
            .client
            .post(format!("{}/{action}", self.run_url(run)))
            .bearer_auth(who)
            .json(command);
        if let Some(csrf) = csrf {
            req = req.header("X-Bot-Csrf", csrf);
        }
        req.send().await.unwrap()
    }

    async fn apply(&self, run: &RunId, action: &str, command: &Value) -> Response {
        self.post(run, action, OPERATOR, Some(&self.csrf), command)
            .await
    }

    async fn close(self) {
        self.stop.send(()).unwrap();
        self.server.await.unwrap().unwrap();
    }
}

fn command(detail: &Value, id: &str) -> Value {
    json!({
        "command_id": id,
        "state": detail["run"]["state"],
        "generation": detail["run"]["generation"],
        "claim_epoch": detail["control"]["claim_epoch"],
    })
}

async fn status(response: Response, expected: StatusCode, body: Value) {
    let actual_status = response.status();
    let actual = response.json::<Value>().await.unwrap();
    assert_eq!(actual_status, expected, "{actual}");
    assert_eq!(actual, body);
}

async fn fork(db: &TestDb, parent: &Attempt) -> RunId {
    db.store
        .delegate(Delegate {
            parent: parent.clone(),
            checked_revision: Revision("r1".into()),
            step: "d1".into(),
            child: AgentId("main/research".into()),
            instruction: "조사해 주세요".into(),
            scopes: scopes(&["run"]),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_checked_resume_does_not_requeue_an_undecodable_target() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = Http::start(&db).await;
    let first = leased(&db, "undecodable-target").await;
    let run = first.attempt.run.clone();
    runs(&db)
        .fail(first.attempt, Failure::Permanent("fixture"))
        .await
        .unwrap();
    let resume = command(&http.detail(&run).await, "resume-target");
    sqlx::query("update bot_runs set target = $2 where profile = 'personal' and id = $1")
        .bind(&run.0)
        .bind(json!({"kind":"agent", "agent":"main"}))
        .execute(db.store.pool())
        .await
        .unwrap();

    status(
        http.apply(&run, "resume", &resume).await,
        StatusCode::BAD_REQUEST,
        json!({"error":"invalid"}),
    )
    .await;
    assert_eq!(state_of_run(&db, &run).await, "dead");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.resume'")
            .await,
        0
    );
    http.close().await;
}

#[tokio::test]
async fn the_old_resume_repeats_after_a_later_dead_generation_without_reopening_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = Http::start(&db).await;
    let first = leased(&db, "resume").await;
    let run = first.attempt.run.clone();
    let live = command(&http.detail(&run).await, "live");
    status(
        http.apply(&run, "resume", &live).await,
        StatusCode::CONFLICT,
        json!({"error":"conflict"}),
    )
    .await;

    runs(&db)
        .fail(first.attempt, Failure::Permanent("fixture"))
        .await
        .unwrap();
    let old = command(&http.detail(&run).await, "one");
    status(
        http.post(&run, "resume", MEMBER, Some(&http.member_csrf), &old)
            .await,
        StatusCode::FORBIDDEN,
        json!({"error":"denied"}),
    )
    .await;
    assert_eq!(
        http.post(&run, "resume", OPERATOR, None, &old)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(state_of_run(&db, &run).await, "dead");
    assert!(Http::get(&http.client, &http.run_url(&run), MEMBER).await["control"].is_null());

    http.checks.store(1, Ordering::SeqCst);
    assert_eq!(
        http.apply(&run, "resume", &old).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(state_of_run(&db, &run).await, "dead");
    http.checks.store(2, Ordering::SeqCst);
    assert_eq!(
        http.apply(&run, "resume", &old).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(state_of_run(&db, &run).await, "ready");
    let second = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(second.attempt.run, run);
    runs(&db)
        .fail(second.attempt, Failure::Permanent("fixture"))
        .await
        .unwrap();
    status(
        http.apply(&run, "resume", &old).await,
        StatusCode::OK,
        json!({"ok":true}),
    )
    .await;
    assert_eq!(state_of_run(&db, &run).await, "dead");
    let mut stale = old.clone();
    stale["command_id"] = json!("different-id");
    status(
        http.apply(&run, "resume", &stale).await,
        StatusCode::CONFLICT,
        json!({"error":"stale"}),
    )
    .await;
    let reused = command(&http.detail(&run).await, "one");
    status(
        http.apply(&run, "resume", &reused).await,
        StatusCode::CONFLICT,
        json!({"error":"conflict"}),
    )
    .await;
    assert_eq!(db.scalar::<i64>("select count(*) from bot_audit where action = 'run.resume' and actor = 'local:admin' and verdict = 'operator_requested'").await, 1);
    assert!(
        bot_core::port::AuditLog::verify(&db.store)
            .await
            .unwrap()
            .first_bad
            .is_none()
    );
    http.close().await;
}

#[tokio::test]
async fn cancellation_checks_the_claim_epoch_even_when_the_generation_is_unchanged() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = Http::start(&db).await;
    let first = leased_in(&db, "epoch", ConversationKind::Direct).await;
    let run = first.attempt.run.clone();
    let old = command(&http.detail(&run).await, "old");
    let child = fork(&db, &first.attempt).await;
    let picked = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(picked.attempt.run, child);
    runs(&db)
        .finish(picked.attempt, done("조사 결과"))
        .await
        .unwrap();
    let next = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(next.attempt.generation, first.attempt.generation);
    assert_ne!(next.attempt.claim, first.attempt.claim);
    status(
        http.apply(&run, "cancel", &old).await,
        StatusCode::CONFLICT,
        json!({"error":"stale"}),
    )
    .await;
    let current = command(&http.detail(&run).await, "latest");
    let (a, b) = tokio::join!(
        http.apply(&run, "cancel", &current),
        http.apply(&run, "cancel", &current)
    );
    for response in [a, b] {
        status(response, StatusCode::OK, json!({"ok":true})).await;
    }
    assert_eq!(
        runs(&db)
            .finish(next.attempt, done("too late"))
            .await
            .unwrap(),
        Completion::Cancelled
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.cancel'")
            .await,
        1
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);
    http.close().await;
}

#[tokio::test]
async fn a_child_hold_blocks_root_cancellation_until_decided_and_the_tree_is_cancelled() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = Http::start(&db).await;
    let parent = leased(&db, "tree").await;
    let root = parent.attempt.run.clone();
    let child = fork(&db, &parent.attempt).await;
    let picked = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(picked.attempt.run, child);
    let child_detail = http.detail(&child).await;
    assert!(child_detail["control"].is_null());
    let for_child = json!({"command_id":"child","state":"leased","generation":picked.attempt.generation.0,"claim_epoch":picked.attempt.claim.unwrap().0});
    status(
        http.apply(&child, "cancel", &for_child).await,
        StatusCode::CONFLICT,
        json!({"error":"conflict"}),
    )
    .await;
    begin_call(&db, &picked.attempt, "c1").await;
    calls(&db)
        .start(start_call(&picked.attempt, "c1", None))
        .await
        .unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: picked.attempt,
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    let root_detail = http.detail(&root).await;
    assert_eq!(root_detail["control"]["held"], true);
    status(
        http.apply(&root, "cancel", &command(&root_detail, "hold"))
            .await,
        StatusCode::CONFLICT,
        json!({"error":"held"}),
    )
    .await;
    assert_eq!(state_of_run(&db, &root).await, "waiting");
    use bot_core::admin::{AdminAct, HoldKind, HoldVerdict};
    let hold = hold_ref(&db, HoldKind::Tool, &child, &call("c1"), None).await;
    db.store
        .resolve_hold(
            operator().principal,
            hold,
            HoldVerdict::Confirmed {
                receipt: "checked".into(),
            },
        )
        .await
        .unwrap();
    let root_detail = http.detail(&root).await;
    status(
        http.apply(&root, "cancel", &command(&root_detail, "stop"))
            .await,
        StatusCode::OK,
        json!({"ok":true}),
    )
    .await;
    assert_eq!(state_of_run(&db, &root).await, "cancelled");
    assert_eq!(state_of_run(&db, &child).await, "cancelled");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.cancel'")
            .await,
        1
    );
    http.close().await;
}

#[tokio::test]
async fn a_dead_child_cannot_be_resumed_by_an_operator() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = Http::start(&db).await;
    let parent = leased(&db, "child-dead").await;
    let child = fork(&db, &parent.attempt).await;
    let picked = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(picked.attempt.run, child);
    let cmd = json!({"command_id":"child-resume","state":"dead","generation":picked.attempt.generation.0,"claim_epoch":picked.attempt.claim.unwrap().0});
    runs(&db)
        .fail(picked.attempt, Failure::Permanent("fixture"))
        .await
        .unwrap();
    status(
        http.apply(&child, "resume", &cmd).await,
        StatusCode::CONFLICT,
        json!({"error":"conflict"}),
    )
    .await;
    assert_eq!(state_of_run(&db, &child).await, "dead");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.resume'")
            .await,
        0
    );
    http.close().await;
}

#[tokio::test]
async fn the_command_index_upgrade_preserves_the_existing_audit_chain() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.sql("alter table bot_audit drop column run_command_id")
        .await;
    db.sql("delete from _sqlx_migrations where version = 30")
        .await;
    let first = leased(&db, "old-audit").await;
    let run = first.attempt.run.clone();
    runs(&db)
        .fail(first.attempt, Failure::Permanent("fixture"))
        .await
        .unwrap();
    let http = Http::start(&db).await;
    let before = http.verify_audit().await;
    assert_eq!(before["ok"], true);
    assert!(before["entries"].as_u64().unwrap() > 0);

    db.store.migrate().await.unwrap();
    assert_eq!(http.verify_audit().await, before);
    let cmd = command(&http.detail(&run).await, "upgraded");
    for _ in 0..2 {
        status(
            http.apply(&run, "resume", &cmd).await,
            StatusCode::OK,
            json!({"ok":true}),
        )
        .await;
    }
    let after = http.verify_audit().await;
    assert_eq!(after["ok"], true);
    assert_eq!(after["entries"], before["entries"].as_u64().unwrap() + 1);
    assert_ne!(after["head"], before["head"]);
    http.close().await;
}

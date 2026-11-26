//! 판정 커밋 후 HTTP 응답을 잃어도 첫 회신의 판정이 두 번째 보류를 풀 수 없다.
mod support;

use bot_admin::{AdminConfig, AdminDeps, StaticTokens, fake, router};
use bot_core::{id::Revision, intake::DeliveryOutcome, state::ConversationKind};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{TestDb, admin::operator, done, leased_in, outbox, runs};

const TOKEN: &str = "test-operator";

async fn hold_next_delivery(db: &TestDb) {
    let outbox = outbox(db);
    let deliveries = outbox
        .claim(
            &bot_core::intake::DeliveryScope::all(bot_core::intake::PushTransportId::SLACK),
            1,
        )
        .await
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    let key = deliveries.into_iter().next().unwrap().key;
    outbox
        .begin(key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    outbox.settle(key, DeliveryOutcome::Held).await.unwrap();
}

async fn get(client: &Client, url: &str) -> Value {
    client
        .get(url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn losing_the_http_response_and_reholding_the_same_reply_does_not_reapply_an_old_verdict() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased_in(&db, "hold-response", ConversationKind::Direct).await;
    runs(&db).finish(run.attempt, done("reply")).await.unwrap();
    hold_next_delivery(&db).await;

    let listener = bot_admin::bind("127.0.0.1:0".parse().unwrap(), false)
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let root = format!("http://{addr}/v1/admin");
    let count = Arc::new(AtomicUsize::new(0));
    let checks = count.clone();
    let config = AdminConfig::new(addr).fence(move || {
        let before = checks.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        Box::pin(async move { before != Ok(1) })
    });
    let store = Arc::new(db.store.clone());
    let deps = AdminDeps {
        read: store.clone(),
        act: store,
        auth: Arc::new(StaticTokens::new().with(TOKEN, operator())),
        broker: Arc::new(fake::Fake::demo()),
        profiles: Arc::new(fake::demo_catalog()),
    };
    let app = router(config, deps).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(bot_admin::serve(listener, app, async {
        let _ = stopped.await;
    }));
    let client = Client::builder().no_proxy().build().unwrap();
    let first = get(&client, &format!("{root}/holds")).await["items"][0].clone();
    let decision_url = format!(
        "{root}/holds/{}/{}/{}/resolve",
        first["profile"].as_str().unwrap(),
        first["run"].as_str().unwrap(),
        first["call"].as_str().unwrap()
    );
    let csrf = get(&client, &format!("{root}/me")).await["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let post = |body: Value| {
        client
            .post(&decision_url)
            .bearer_auth(TOKEN)
            .header("X-Bot-Csrf", &csrf)
            .json(&body)
            .send()
    };
    let verdict = |id: &Value| json!({"hold_id":id,"verdict":"not_executed","evidence":"checked at destination"});
    let old_id = &first["hold_id"];
    assert!(old_id.is_string());
    // 변경 전 검사는 통과시키고, 커밋 뒤 응답 직전의 검사만 실패시킨다.
    count.store(2, Ordering::SeqCst);
    assert_eq!(
        post(verdict(old_id)).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let audits = "select count(*) from bot_audit where action = 'hold.resolve'";
    assert_eq!(db.scalar::<i64>(audits).await, 1);
    hold_next_delivery(&db).await;
    let second = get(&client, &format!("{root}/holds")).await["items"][0].clone();
    let new_id = &second["hold_id"];
    assert!(new_id.is_string());
    assert_ne!(old_id, new_id);
    assert_eq!(first["call"], second["call"]);

    let replay = post(verdict(old_id)).await.unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(replay.json::<Value>().await.unwrap()["repeated"], true);
    let wrong = json!({"hold_id":old_id,"verdict":"confirmed","receipt":"different"});
    assert_eq!(post(wrong).await.unwrap().status(), StatusCode::CONFLICT);
    assert_eq!(
        get(&client, &format!("{root}/holds")).await["items"][0]["hold_id"],
        *new_id
    );
    assert_eq!(db.scalar::<i64>(audits).await, 1);

    let confirm = json!({"hold_id":new_id,"verdict":"confirmed","receipt":"second-delivery"});
    assert_eq!(post(confirm).await.unwrap().status(), StatusCode::OK);
    assert_eq!(db.scalar::<i64>(audits).await, 2);
    assert_eq!(
        db.scalar::<String>("select receipt from bot_outbox").await,
        "second-delivery"
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

mod support;

use bot_core::{command::*, id::*, intake::RequestKey, port::*, state::*};
use support::*;

fn is(result: Result<RunId, Error>, want: fn(&Error) -> bool) -> bool {
    matches!(result, Err(ref e) if want(e))
}

#[tokio::test]
async fn accepts_duplicate_request_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = db
        .store
        .accept(accept("personal", "req-1", "오늘 일정 알려줘"))
        .await
        .unwrap();
    let again = db
        .store
        .accept(accept("personal", "req-1", "오늘 일정 알려줘"))
        .await
        .unwrap();
    assert_eq!(first, again, "같은 키·같은 내용은 기존 접수");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );

    // 같은 키·다른 내용은 충돌이고 아무것도 늘지 않는다
    let changed = db
        .store
        .accept(accept("personal", "req-1", "다른 내용"))
        .await;
    assert!(is(changed, |e| matches!(e, Error::Conflict)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    // 키의 한 칸이라도 다르면 별개 접수다: 주체·프로필
    let other_principal = accept_as(
        "personal",
        "req-1",
        "local:guest",
        ConversationKind::Cli,
        "owner",
        "오늘 일정 알려줘",
    );
    let other_profile = accept("work", "req-1", "오늘 일정 알려줘");
    assert_ne!(db.store.accept(other_principal).await.unwrap(), first);
    assert_ne!(db.store.accept(other_profile).await.unwrap(), first);
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicate_accepts_converge_on_one_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut tasks = Vec::new();
    for n in 0..12 {
        let store = if n % 2 == 0 {
            db.store.clone()
        } else {
            db.peer("peer").await
        };
        tasks.push(tokio::spawn(async move {
            store
                .accept(accept("personal", "same", "hi"))
                .await
                .unwrap()
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "모든 접수가 같은 run으로 수렴");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );
}

#[tokio::test]
async fn same_body_in_another_json_layout_is_the_same_request() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut a = accept("personal", "req-json", "x");
    a.event.body = JsonPayload(r#"{"b":1,  "a":[1,2]}"#.into());
    let mut b = accept("personal", "req-json", "x");
    b.event.body = JsonPayload(r#"{"a":[1,2],"b":1}"#.into());
    let first = db.store.accept(a).await.unwrap();
    assert_eq!(db.store.accept(b).await.unwrap(), first);
}

#[tokio::test]
async fn status_reads_the_same_request_without_new_effects() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let key = RequestKey {
        profile: ProfileId("personal".into()),
        binding: BindingId("desk".into()),
        principal: PrincipalId("local:owner".into()),
        request: RequestId("req-s".into()),
    };
    assert!(runs(&db).status(key.clone()).await.unwrap().is_none());
    let run = db
        .store
        .accept(accept("personal", "req-s", "hi"))
        .await
        .unwrap();
    let state = runs(&db).status(key.clone()).await.unwrap().unwrap();
    assert_eq!((state.run, state.state), (run.clone(), RunState::Ready));
    let before = db.scalar::<i64>("select count(*) from bot_audit").await;
    runs(&db).status(key.clone()).await.unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        before
    );

    // 다른 프로필의 같은 키는 조회되지 않는다
    let foreign = RequestKey {
        profile: ProfileId("work".into()),
        ..key
    };
    assert!(runs(&db).status(foreign).await.unwrap().is_none());
}

#[tokio::test]
async fn rejects_malformed_and_oversized_input() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut empty_request = accept("personal", "", "hi");
    empty_request.event.request = RequestId(String::new());
    let mut bad_body = accept("personal", "r1", "hi");
    bad_body.event.body = JsonPayload("{not json".into());
    let mut control = accept("personal", "r2", "hi");
    control.event.principal = PrincipalId("local:ow\u{0}ner".into());
    let mut huge = accept("personal", "r3", "hi");
    huge.event.body = JsonPayload(format!("\"{}\"", "x".repeat(2 << 20)));
    let bad_args = tool_accept("personal", "r4", "agenda.list", "[1,2]");
    for bad in [empty_request, bad_body, control, huge, bad_args] {
        let result = db.store.accept(bad).await;
        assert!(is(result, |e| matches!(e, Error::Invalid)));
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 0);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );
}

#[tokio::test]
async fn a_routing_change_after_the_first_accept_returns_the_original_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = runs(&db)
        .accept(accept("personal", "req-route", "일정 알려줘"))
        .await
        .unwrap();
    // 프로필 판본이 바뀌어 같은 외부 요청의 라우팅 결과(대상·판본)가 달라졌다
    let mut routed = accept("personal", "req-route", "일정 알려줘");
    routed.target = Target::Agent {
        agent: AgentId("main/research".into()),
        instruction: "다른 지침".into(),
    };
    routed.revision = Revision("r2".into());
    assert_eq!(
        runs(&db).accept(routed).await.unwrap(),
        first,
        "충돌이 아니라 최초 접수의 실행 계획으로 수렴한다"
    );
    assert_eq!(
        db.scalar::<String>("select agent || ':' || revision from bot_runs")
            .await,
        "main:r1"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // 외부 입력(본문·대화)이 달라지면 그때는 충돌이다
    let changed = runs(&db)
        .accept(accept("personal", "req-route", "다른 요청"))
        .await;
    assert!(matches!(changed, Err(Error::Conflict)));
}

#[tokio::test]
async fn an_http_request_id_reused_on_another_route_conflicts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = |method: &str, path: &str| {
        let mut a = accept_as(
            "personal",
            "req-http",
            "http:inbox/sub",
            ConversationKind::Http,
            "inbox",
            "x",
        );
        a.event.http = Some(HttpRoute {
            method: method.into(),
            path: path.into(),
            response: bot_core::profile::ResponseMode::Accepted,
        });
        a
    };
    let first = runs(&db).accept(http("POST", "/agenda")).await.unwrap();
    assert_eq!(
        runs(&db).accept(http("POST", "/agenda")).await.unwrap(),
        first,
        "같은 경로·메서드의 재전달은 같은 run"
    );
    for (method, path) in [("PUT", "/agenda"), ("POST", "/agenda/cancel")] {
        let other = runs(&db).accept(http(method, path)).await;
        assert!(matches!(other, Err(Error::Conflict)), "{method} {path}");
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // HTTP가 아닌 입구의 해시 바이트는 그대로다: http가 없는 요청은 같은 키로 http 요청과 충돌하지 않고 기존 접수와 같은 해시다
    let cli = accept("personal", "req-cli", "x");
    let again = accept("personal", "req-cli", "x");
    assert_eq!(
        runs(&db).accept(cli).await.unwrap(),
        runs(&db).accept(again).await.unwrap()
    );
}

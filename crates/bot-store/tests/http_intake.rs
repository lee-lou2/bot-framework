//! HTTP 입구의 접수: 대화 종류와 경로 정보는 함께만 오고, 최초 응답 방식은 저장돼 이후 조회가 그대로 돌려준다.
//! 새 외부 접수만 활성 판본과 대조한다(재전달은 기존 run으로 수렴).

mod support;

use bot_core::{
    command::*,
    id::*,
    intake::RequestKey,
    port::*,
    profile::{ActiveRevisions, ResponseMode},
    state::*,
};
use std::collections::BTreeMap;
use support::*;

fn http(request: &str, mode: ResponseMode) -> Accept {
    let mut a = accept_as(
        "personal",
        request,
        "http:inbox/sub",
        ConversationKind::Http,
        "inbox",
        "x",
    );
    a.event.http = Some(HttpRoute {
        method: "POST".into(),
        path: "/agenda".into(),
        response: mode,
    });
    a
}

fn key_of(a: &Accept) -> RequestKey {
    let e = &a.event;
    RequestKey {
        profile: e.profile.clone(),
        binding: e.binding.clone(),
        principal: e.principal.clone(),
        request: e.request.clone(),
    }
}

#[tokio::test]
async fn an_http_conversation_needs_a_valid_route_and_other_conversations_must_not_have_one() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut missing = http("a", ResponseMode::Accepted);
    missing.event.http = None;
    let mut on_cli = accept("personal", "b", "x");
    on_cli.event.http = Some(HttpRoute {
        method: "POST".into(),
        path: "/agenda".into(),
        response: ResponseMode::Sync,
    });
    let mut bad = Vec::new();
    for (method, path) in [
        ("post", "/agenda"),
        ("", "/agenda"),
        ("POST", "agenda"),
        ("POST", "/agenda?x=1"),
        ("POST", "/agen da"),
    ] {
        let mut a = http("c", ResponseMode::Accepted);
        a.event.http = Some(HttpRoute {
            method: method.into(),
            path: path.into(),
            response: ResponseMode::Accepted,
        });
        bad.push(a);
    }
    for a in [missing, on_cli].into_iter().chain(bad) {
        assert!(matches!(runs(&db).accept(a).await, Err(Error::Invalid)));
    }
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );
}

#[tokio::test]
async fn the_first_response_mode_is_stored_and_returned_and_is_not_part_of_the_hash() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = runs(&db)
        .accept(http("h1", ResponseMode::Sync))
        .await
        .unwrap();
    let again = runs(&db)
        .accept(http("h1", ResponseMode::Accepted))
        .await
        .unwrap();
    assert_eq!(run, again, "응답 방식이 달라도 같은 요청이다");
    let state = runs(&db)
        .status(key_of(&http("h1", ResponseMode::Sync)))
        .await
        .unwrap()
        .unwrap();
    let route = state.http.unwrap();
    assert_eq!(
        (route.method.as_str(), route.path.as_str(), route.response),
        ("POST", "/agenda", ResponseMode::Sync),
        "최초 접수의 응답 방식이 그대로 남는다"
    );
    // 메서드·경로가 다르면 다른 요청이다
    let mut other = http("h1", ResponseMode::Sync);
    other.event.http.as_mut().unwrap().path = "/other".into();
    assert!(matches!(
        runs(&db).accept(other).await,
        Err(Error::Conflict)
    ));
    // HTTP가 아닌 입구는 경로 정보가 없다
    let cli = accept("personal", "c1", "x");
    runs(&db).accept(cli.clone()).await.unwrap();
    assert!(
        runs(&db)
            .status(key_of(&cli))
            .await
            .unwrap()
            .unwrap()
            .http
            .is_none()
    );
}

#[tokio::test]
async fn a_new_request_is_checked_against_the_active_revision_but_a_redelivery_is_not() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    // 아무것도 게시되지 않았으면 새 외부 접수는 받지 않는다(접수 행도 남지 않는다)
    assert!(matches!(
        runs(&db).accept(http("n0", ResponseMode::Accepted)).await,
        Err(Error::Stale)
    ));
    db.publish().await;
    let mut old = http("n1", ResponseMode::Accepted);
    old.revision = Revision("r0".into());
    assert!(matches!(runs(&db).accept(old).await, Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );
    let first = runs(&db)
        .accept(http("n1", ResponseMode::Accepted))
        .await
        .unwrap();
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([(profile_id(), Revision("r2".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    // 판본이 바뀐 뒤에도 같은 요청의 재전달은 최초 접수로 수렴하고, 새 요청은 새 판본으로만 받는다
    assert_eq!(
        runs(&db)
            .accept(http("n1", ResponseMode::Accepted))
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        runs(&db).accept(http("n2", ResponseMode::Accepted)).await,
        Err(Error::Stale)
    ));
    let mut fresh = http("n2", ResponseMode::Accepted);
    fresh.revision = Revision("r2".into());
    runs(&db).accept(fresh).await.unwrap();
}

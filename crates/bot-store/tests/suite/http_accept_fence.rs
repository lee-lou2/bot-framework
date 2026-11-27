use crate::support::*;
use bot_core::{
    command::HttpRoute,
    id::Revision,
    intake::RequestKey,
    port::{Error, RunStore},
    profile::ResponseMode,
    state::ConversationKind,
};
use sqlx::Connection;
use std::time::Duration;

#[tokio::test]
async fn a_new_http_request_waits_for_profile_replacement_and_keeps_its_first_response_mode() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let mut request = accept_as(
        "personal",
        "request",
        "http:inbox/caller",
        ConversationKind::Http,
        "inbox",
        "hello",
    );
    request.event.http = Some(HttpRoute {
        method: "POST".into(),
        path: "/submit".into(),
        response: ResponseMode::Sync,
    });
    let mut connection = db.raw().await;
    let mut change = connection.begin().await.unwrap();
    let writer: i32 =
        sqlx::query_scalar("select pg_backend_pid() from bot_active where singleton for update")
            .fetch_one(&mut *change)
            .await
            .unwrap();
    sqlx::query("update bot_active_profiles set revision = 'r2' where profile = 'personal'")
        .execute(&mut *change)
        .await
        .unwrap();
    sqlx::query("update bot_active set revision = 'set-2' where singleton")
        .execute(&mut *change)
        .await
        .unwrap();

    let peer = db.peer("gateway").await;
    let old = request.clone();
    let submission = tokio::spawn(async move { peer.accept(old).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))"
            ).bind(writer).fetch_one(db.store.pool()).await.unwrap();
            if waiting { break }
            assert!(!submission.is_finished(), "accept finished before revision replacement");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("accept must wait for revision replacement");
    change.commit().await.unwrap();
    assert!(matches!(submission.await.unwrap(), Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );

    request.revision = Revision("r2".into());
    let run = runs(&db).accept(request.clone()).await.unwrap();
    request.event.http.as_mut().unwrap().response = ResponseMode::Accepted;
    assert_eq!(runs(&db).accept(request.clone()).await.unwrap(), run);
    let e = request.event;
    let key = RequestKey {
        profile: e.profile,
        binding: e.binding,
        principal: e.principal,
        request: e.request,
    };
    let stored = runs(&db).status(key).await.unwrap().unwrap();
    assert_eq!(stored.http.unwrap().response, ResponseMode::Sync);
}

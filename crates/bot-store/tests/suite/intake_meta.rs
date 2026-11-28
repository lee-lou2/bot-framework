use crate::support::*;
use bot_core::{
    command::{AcceptExtras, Delegate, HttpRoute},
    id::{AgentId, Revision},
    port::{DelegationStore, RunStore},
    profile::ResponseMode,
    slack::{SlackSource, SourceKind},
    state::ConversationKind,
};

#[tokio::test]
async fn slack_message_ts_and_attachments_load_on_root_and_delegated_children() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut cmd = accept_as(
        "personal",
        "slack-msg",
        "slack:work/U1",
        ConversationKind::Channel,
        "C12345678",
        "delete my posts here",
    );
    cmd.event.conversation.thread = Some("1700000000.000100".into());
    cmd.event.attachments = vec!["slack:work/F001".into()];
    let extras = AcceptExtras {
        source: Some(SlackSource {
            kind: SourceKind::Message,
            team: "T1".into(),
            channel: "C12345678".into(),
            ts: "1700000000.000250".into(),
            thread_ts: Some("1700000000.000100".into()),
            event_id: "Ev1".into(),
            reaction: None,
        }),
        chain: None,
    };
    let root = db.store.accept_with(cmd, extras).await.unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    assert_eq!(parent.attempt.run, root);
    assert_eq!(parent.conversation.id, "C12345678");
    assert_eq!(
        parent.conversation.thread.as_deref(),
        Some("1700000000.000100")
    );
    assert_eq!(
        parent.intake.message_ts.as_deref(),
        Some("1700000000.000250")
    );
    assert!(parent.intake.http_route.is_none());
    assert_eq!(parent.work.attachments, vec!["slack:work/F001".to_owned()]);

    let child_id = db
        .store
        .delegate(Delegate {
            parent: parent.attempt,
            checked_revision: Revision("r1".into()),
            step: "delegate-1".into(),
            child: AgentId("main/research".into()),
            instruction: "inspect thread".into(),
            scopes: ["run".to_owned()].into(),
        })
        .await
        .unwrap();
    let child = db.store.claim().await.unwrap().unwrap();
    assert_eq!(child.attempt.run, child_id);
    assert_eq!(child.conversation.id, "C12345678");
    assert_eq!(
        child.conversation.thread.as_deref(),
        Some("1700000000.000100")
    );
    assert_eq!(
        child.intake.message_ts.as_deref(),
        Some("1700000000.000250")
    );
    assert_eq!(child.work.attachments, vec!["slack:work/F001".to_owned()]);
    db.store.finish(child.attempt, done("ok")).await.unwrap();

    // Reaction events do not carry a message_ts in slack_key.
    let mut react = accept_as(
        "personal",
        "slack-react",
        "slack:work/U1",
        ConversationKind::Channel,
        "C12345678",
        "react",
    );
    react.event.conversation.thread = None;
    let react_extras = AcceptExtras {
        source: Some(SlackSource {
            kind: SourceKind::Reaction,
            team: "T1".into(),
            channel: "C12345678".into(),
            ts: "1700000000.000300".into(),
            thread_ts: None,
            event_id: "Ev2".into(),
            reaction: Some("eyes".into()),
        }),
        chain: None,
    };
    let react_run = db.store.accept_with(react, react_extras).await.unwrap();
    // Claim resumes parent first; finish parent, then claim reaction run.
    let resumed = db.store.claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.run, root);
    db.store.finish(resumed.attempt, done("ok")).await.unwrap();
    let claimed_react = db.store.claim().await.unwrap().unwrap();
    assert_eq!(claimed_react.attempt.run, react_run);
    assert!(claimed_react.intake.message_ts.is_none());
}

#[tokio::test]
async fn http_webhook_route_loads_and_inherits_while_openai_api_keeps_route_separate() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut hook = accept_as(
        "personal",
        "hook-1",
        "http:inbox/client",
        ConversationKind::Http,
        "inbox",
        "sync",
    );
    hook.event.http = Some(HttpRoute {
        method: "POST".into(),
        path: "/agenda".into(),
        response: ResponseMode::Accepted,
    });
    let hook_run = db.store.accept(hook).await.unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    assert_eq!(parent.attempt.run, hook_run);
    assert_eq!(
        parent.intake.http_route,
        Some(("POST".to_owned(), "/agenda".to_owned()))
    );
    assert!(parent.intake.message_ts.is_none());

    let child_id = db
        .store
        .delegate(Delegate {
            parent: parent.attempt,
            checked_revision: Revision("r1".into()),
            step: "delegate-hook".into(),
            child: AgentId("main/research".into()),
            instruction: "handle webhook".into(),
            scopes: ["run".to_owned()].into(),
        })
        .await
        .unwrap();
    let child = db.store.claim().await.unwrap().unwrap();
    assert_eq!(child.attempt.run, child_id);
    assert_eq!(
        child.intake.http_route,
        Some(("POST".to_owned(), "/agenda".to_owned()))
    );
    db.store.finish(child.attempt, done("ok")).await.unwrap();
    let resumed = db.store.claim().await.unwrap().unwrap();
    db.store.finish(resumed.attempt, done("ok")).await.unwrap();

    // OpenAI-compatible API runs populate `api` and keep `intake.http_route` empty.
    let _ = crate::support::api::accepted(
        crate::support::api::admit(
            &db,
            crate::support::api::request("api-1", "sdk", "main", 1, 4),
        )
        .await,
    );
    let api_ctx = crate::support::api::claimed(&db).await;
    assert!(api_ctx.api.is_some());
    assert!(api_ctx.intake.http_route.is_none());
    assert!(api_ctx.intake.message_ts.is_none());
}

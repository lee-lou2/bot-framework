use super::*;
use crate::{entrance::Real, testing::*};
use bot_core::{
    command::HttpRoute,
    id::{ProfileId, RequestId},
    profile::{Adapter, ResponseMode, Trigger},
    state::{Conversation, ConversationKind},
};
use serde_json::json;
use std::time::Duration;

const HOST: &str = r#"{
    "cli": {"accounts": {"owner": {"uid": 501}}},
    "slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token",
        "channels": {"planning": "C1"}}}},
    "http": {"endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "k"}}},
    "limits": {"per_entrance_per_min": 100, "per_principal_per_min": 3, "body_bytes": 2048, "attachments": 2}
}"#;

struct Rig {
    ingress: Ingress,
    store: Arc<MemoryStore>,
    private: Arc<MemoryPrivate>,
    catalog: Arc<TestCatalog>,
}

fn rig(fixture: Fixture) -> Rig {
    rig_with(fixture, |_| {})
}

fn rig_with(fixture: Fixture, tune: impl FnOnce(&mut HostConfig)) -> Rig {
    let store = MemoryStore::new();
    let private = MemoryPrivate::new();
    let catalog = TestCatalog::new([fixture.build()]);
    let mut host = HostConfig::from_json(HOST).unwrap();
    tune(&mut host);
    let ingress = Ingress::new(
        catalog.clone(),
        Arc::new(host),
        store.clone(),
        private.clone(),
    )
    .with_uid_resolver(|account| Ok(account.uid.unwrap()));
    Rig {
        ingress,
        store,
        private,
        catalog,
    }
}

fn base() -> Fixture {
    profile("personal")
        .binding(
            "desk",
            Adapter::Cli {
                account: "owner".into(),
            },
            "main",
        )
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("local:owner", &["owner"])
        .member("slack:home/U1", &["owner"])
        .member("slack:home/U2", &["svc"])
        .member("http:inbox/svc", &["svc"])
        .role("owner", &["run", "calendar.read"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["run", "calendar.read", "calendar.delete"])
}

fn cli_chat(request: &str, text: &str) -> Inbound {
    Inbound {
        real: Real::Cli {
            uid: 501,
            profile: ProfileId("personal".into()),
        },
        principal: PrincipalId("local:owner".into()),
        request: RequestId(request.into()),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: Some(request.into()),
        },
        attachments: vec![],
        body: json!({"text": text}),
        event: Event::Message,
    }
}

fn slack_dm(user: &str, request: &str, text: &str) -> Inbound {
    Inbound {
        real: Real::SlackDm { team: "T1".into() },
        principal: PrincipalId(format!("slack:home/{user}")),
        request: RequestId(request.into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "D1".into(),
            thread: Some("1.1".into()),
        },
        attachments: vec![],
        body: json!({"text": text}),
        event: Event::Message,
    }
}

fn http_post(principal: &str, request: &str, path: &str, body: Value) -> Inbound {
    Inbound {
        real: Real::Http {
            endpoint: "inbox".into(),
        },
        principal: PrincipalId(principal.into()),
        request: RequestId(request.into()),
        conversation: Conversation {
            kind: ConversationKind::Http,
            id: "inbox".into(),
            thread: Some(request.into()),
        },
        attachments: vec![],
        body,
        event: Event::Http {
            method: "POST".into(),
            path: path.into(),
        },
    }
}

fn accepted(run: &str) -> Admission {
    Admission::Accepted {
        run: RunId(run.into()),
        respond: Respond::Accepted,
    }
}

#[tokio::test]
async fn chat_goes_to_the_bound_agent_with_the_requesters_scope_snapshot() {
    let rig = rig(base());
    assert_eq!(
        rig.ingress
            .admit(cli_chat("r1", "Show today's agenda"))
            .await,
        accepted("run-1")
    );
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].target, "agent main Show today's agenda");
    assert_eq!(records[0].scopes, ["calendar.read", "run"]);
    assert_eq!(records[0].approver.as_deref(), Some("local:owner"));
    assert_eq!(records[0].revision, "personal-r1");
    assert_eq!(records[0].body, r#"{"text":"Show today's agenda"}"#);
    assert_eq!(records[0].reply_transport, None);
}

#[tokio::test]
async fn the_same_request_is_accepted_once_and_a_changed_one_conflicts() {
    let rig = rig(base());
    let first = rig.ingress.admit(cli_chat("r1", "a")).await;
    assert_eq!(rig.ingress.admit(cli_chat("r1", "a")).await, first);
    assert_eq!(rig.store.accepted().len(), 1);
    assert_eq!(
        rig.ingress.admit(cli_chat("r1", "different")).await,
        Admission::Rejected(Rejection::Conflict)
    );
    assert!(Admission::Rejected(Rejection::Conflict).settled());
}

#[tokio::test]
async fn unregistered_unknown_and_unauthorised_inputs_never_reach_the_store() {
    let rig = rig(base());
    assert_eq!(
        rig.ingress.admit(slack_dm("U9", "e1", "hi")).await,
        Admission::Rejected(Rejection::Unregistered)
    );
    let mut stranger = cli_chat("r1", "hi");
    stranger.real = Real::Cli {
        uid: 999,
        profile: ProfileId("personal".into()),
    };
    assert_eq!(rig.ingress.admit(stranger).await, Admission::Ignored);
    // Member lacks `run` scope, so agent conversation is forbidden.
    assert_eq!(
        rig.ingress.admit(slack_dm("U2", "e2", "hi")).await,
        Admission::Rejected(Rejection::Forbidden)
    );
    assert_eq!(
        rig.ingress.admit(cli_chat("r3", "   ")).await,
        Admission::Rejected(Rejection::Invalid)
    );
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn limits_apply_per_principal_and_per_entrance() {
    let rig = rig(base());
    for n in 0..3 {
        assert!(matches!(
            rig.ingress.admit(cli_chat(&format!("r{n}"), "hi")).await,
            Admission::Accepted { .. }
        ));
    }
    let blocked = rig.ingress.admit(cli_chat("r9", "hi")).await;
    assert!(
        matches!(blocked, Admission::Rejected(Rejection::RateLimited(wait)) if wait.as_secs() >= 1)
    );
    assert!(matches!(
        rig.ingress.admit(slack_dm("U1", "e1", "hi")).await,
        Admission::Accepted { .. }
    ));

    // Per-entrance limit counts unregistered attempts as well.
    let rig = rig_with(base(), |host| host.limits.per_entrance_per_min = Some(2));
    for n in 0..2 {
        assert_eq!(
            rig.ingress
                .admit(slack_dm("U9", &format!("x{n}"), "hi"))
                .await,
            Admission::Rejected(Rejection::Unregistered)
        );
    }
    assert!(matches!(
        rig.ingress.admit(slack_dm("U1", "e1", "hi")).await,
        Admission::Rejected(Rejection::RateLimited(_))
    ));
}

#[tokio::test]
async fn oversized_bodies_and_attachment_floods_are_refused_before_the_store() {
    let rig = rig(base());
    let long = "x".repeat(3000);
    assert_eq!(
        rig.ingress.admit(cli_chat("r1", &long)).await,
        Admission::Rejected(Rejection::TooLarge)
    );
    let mut files = cli_chat("r2", "hi");
    files.attachments = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(
        rig.ingress.admit(files).await,
        Admission::Rejected(Rejection::TooLarge)
    );
    assert!(rig.store.accepted().is_empty());
}

fn http_trigger_with(id: &str, order: i64, action: Option<&str>, tool: &str) -> Trigger {
    let mut trigger = http_trigger(id, "inbox", "POST", "/agenda", tool_target(tool, "{}"));
    trigger.order = order;
    if let Some(action) = action {
        trigger.conditions.insert(
            "body.action".into(),
            bot_core::command::JsonPayload(format!("\"{action}\"")),
        );
    }
    trigger.input.insert("event".into(), "body.event".into());
    trigger
}

#[tokio::test]
async fn overlapping_http_triggers_run_only_the_first_match_with_mapped_args() {
    let fixture = base()
        .trigger(
            "main",
            http_trigger_with("cancel", 10, Some("cancel"), "agenda.delete"),
        )
        .trigger("main", http_trigger_with("any", 20, None, "agenda.list"));
    let rig = rig_with(fixture, |host| host.limits.per_principal_per_min = Some(30));
    let post = |request: &str, body: Value| http_post("http:inbox/svc", request, "/agenda", body);

    assert_eq!(
        rig.ingress
            .admit(post("h1", json!({"action": "cancel", "event": "e-1"})))
            .await,
        accepted("run-1")
    );
    assert_eq!(
        rig.ingress
            .admit(post("h2", json!({"action": "list", "event": "e-2"})))
            .await,
        accepted("run-2")
    );
    let records = rig.store.accepted();
    assert_eq!(
        records[0].target,
        r#"tool main agenda.delete {"event":"e-1"}"#
    );
    assert_eq!(
        records[1].target,
        r#"tool main agenda.list {"event":"e-2"}"#
    );
    // Service principals cannot be designated as human approvers.
    assert_eq!(records[0].approver, None);
    assert_eq!(records[0].scopes, ["calendar.delete"]);

    assert_eq!(
        rig.ingress
            .admit(post("h3", json!({"action": "cancel"})))
            .await,
        Admission::Rejected(Rejection::Invalid)
    );
    assert_eq!(
        rig.ingress
            .admit(http_post("http:inbox/svc", "h4", "/other", json!({})))
            .await,
        Admission::Rejected(Rejection::NoRoute)
    );
    assert_eq!(rig.store.accepted().len(), 2);
}

#[tokio::test]
async fn instruction_triggers_send_their_fixed_instruction_and_keep_the_body_as_input() {
    let ask = || {
        http_trigger(
            "ask",
            "inbox",
            "POST",
            "/ask",
            TriggerTarget::Instruction("Summarize".into()),
        )
    };
    let denied = rig(base().trigger("main", ask()));
    let request = http_post("http:inbox/svc", "h1", "/ask", json!({"topic": "agenda"}));
    assert_eq!(
        denied.ingress.admit(request).await,
        Admission::Rejected(Rejection::Forbidden)
    );
    assert!(denied.store.accepted().is_empty());

    let allowed = rig(base()
        .member("http:inbox/boss", &["owner"])
        .trigger("main", ask()));
    let request = http_post("http:inbox/boss", "h2", "/ask", json!({"topic": "agenda"}));
    assert_eq!(allowed.ingress.admit(request).await, accepted("run-1"));
    let record = &allowed.store.accepted()[0];
    assert_eq!(record.target, "agent main Summarize");
    assert_eq!(record.body, r#"{"topic":"agenda"}"#);
}

#[tokio::test]
async fn routes_private_dm_before_queue() {
    let marker = "synthetic-sensitive-marker-7f3a";
    let mut private = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    private.private = true;
    private.input.insert("text".into(), "body.text".into());
    let rig = rig(base().trigger("main", private));

    assert_eq!(
        rig.ingress.admit(slack_dm("U1", "ev-1", marker)).await,
        Admission::Private
    );
    assert!(rig.store.accepted().is_empty());
    assert_eq!(
        rig.private.received(),
        [(
            "inbox.receive".to_string(),
            format!(r#"{{"text":"{marker}"}}"#)
        )]
    );
    assert_eq!(
        rig.ingress.admit(slack_dm("U1", "ev-1", marker)).await,
        Admission::Private
    );
    assert_eq!(rig.private.received().len(), 1);

    // Private intake failure must stay unsettled (no ack) and never fall through to normal chat.
    rig.private.fail_next(Error::Unavailable);
    let failed = rig.ingress.admit(slack_dm("U1", "ev-2", marker)).await;
    assert_eq!(failed, Admission::Rejected(Rejection::Unavailable));
    assert!(!failed.settled());
    assert!(rig.store.accepted().is_empty());

    assert_eq!(
        rig.ingress.admit(slack_dm("U9", "ev-3", marker)).await,
        Admission::Rejected(Rejection::Unregistered)
    );
    assert_eq!(rig.private.received().len(), 1);
    for record in rig.store.accepted() {
        assert!(!record.body.contains(marker) && !record.target.contains(marker));
    }
}

#[tokio::test]
async fn private_pass_uses_distinct_request_id_for_normal_dm() {
    let mut private = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    private.private = true;
    private.private_pass = true;
    private.input.insert("text".into(), "body.text".into());
    let rig = rig(base().trigger("main", private));

    rig.private.pass_next();
    assert_eq!(
        rig.ingress
            .admit(slack_dm("U1", "D1.100.1", "hello?"))
            .await,
        accepted("run-1")
    );
    let record = &rig.store.accepted()[0];
    assert_eq!(record.key.request.0, "D1.100.1:pass");
    assert_eq!(
        record.reply_transport,
        Some(bot_core::intake::PushTransportId::SLACK)
    );
    assert_eq!(record.target, "agent main hello?");
}

#[derive(Default)]
struct AlwaysPass(Mutex<Vec<(String, [u8; 32])>>);

impl PrivateIntake for AlwaysPass {
    fn receive(&self, call: PrivateCall) -> bot_core::port::Reply<'_, PrivateDecision> {
        assert_eq!(call.args.0, "{}");
        self.0
            .lock()
            .unwrap()
            .push((call.trigger, call.envelope_sha256));
        Box::pin(async { Ok(PrivateDecision::Pass) })
    }
}

#[tokio::test]
async fn only_the_selected_private_trigger_may_release_the_event() {
    let mut closed = event_trigger("closed", "direct", tool_target("inbox.receive", "{}"));
    closed.private = true;
    closed.order = 2;
    let mut open = closed.clone();
    open.id = "open".into();
    open.order = 1;
    open.private_pass = true;
    open.conditions.insert(
        "body.text".into(),
        JsonPayload(r#""general question""#.into()),
    );
    let rig = rig_with(
        base().trigger("main", open).trigger("main", closed),
        |host| {
            host.limits.per_principal_per_min = Some(30);
        },
    );
    let private = Arc::new(AlwaysPass::default());
    let ingress = Ingress::new(
        rig.catalog,
        rig.ingress.host.clone(),
        rig.store.clone(),
        private.clone(),
    )
    .with_uid_resolver(|account| Ok(account.uid.unwrap()));
    assert_eq!(
        ingress
            .admit(slack_dm("U1", "ev-1", "sensitive question"))
            .await,
        Admission::Rejected(Rejection::Forbidden)
    );
    assert!(rig.store.accepted().is_empty());
    let public = || slack_dm("U1", "ev-2", "general question");
    rig.store.fail_next(Error::Unavailable);
    assert!(!ingress.admit(public()).await.settled());
    for _ in 0..2 {
        assert_eq!(ingress.admit(public()).await, accepted("run-1"));
    }
    assert_eq!(rig.store.accepted().len(), 1);
    let mut changed = public();
    changed.attachments.push("slack:home/F1".into());
    ingress.admit(changed).await;
    let mut changed = public();
    changed.body["detail"] = json!("unmapped value");
    ingress.admit(changed).await;
    let calls = private.0.lock().unwrap();
    assert_eq!(calls[0].0, "closed");
    assert!(calls[1..].iter().all(|(id, _)| id == "open"));
    assert_eq!(calls[1].1, calls[2].1);
    assert_eq!(calls[1].1, calls[3].1);
    assert_ne!(calls[1].1, calls[4].1);
    assert_ne!(calls[1].1, calls[5].1);
}

#[tokio::test]
async fn private_pass_fingerprint_binds_the_authenticated_slack_event() {
    use bot_core::slack::SourceKind;

    let mut trigger = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    trigger.private = true;
    trigger.private_pass = true;
    let rig = rig_with(base().trigger("main", trigger), |host| {
        host.limits.per_principal_per_min = Some(30);
    });
    let private = Arc::new(AlwaysPass::default());
    let ingress = Ingress::new(
        rig.catalog,
        rig.ingress.host.clone(),
        rig.store,
        private.clone(),
    );
    let routes = ingress.routes().unwrap();
    let source = SlackSource {
        kind: SourceKind::Message,
        team: "T1".into(),
        channel: "D1".into(),
        ts: "100.1".into(),
        thread_ts: None,
        event_id: "Ev1".into(),
        reaction: None,
    };
    // Keep envelope fixed to isolate differences in the authenticated Slack source.
    for event in [
        source.clone(),
        source.clone(),
        SlackSource {
            ts: "100.2".into(),
            ..source.clone()
        },
        SlackSource {
            event_id: "Ev2".into(),
            ..source
        },
    ] {
        ingress
            .admit_sourced(&routes, slack_dm("U1", "D1.100.1", "question"), Some(event))
            .await;
    }
    let calls = private.0.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].1, calls[1].1);
    assert_ne!(calls[0].1, calls[2].1);
    assert_ne!(calls[0].1, calls[3].1);
}

#[tokio::test]
async fn a_store_outage_is_not_settled_so_the_source_redelivers() {
    let rig = rig(base());
    rig.store.fail_next(Error::Unavailable);
    let outage = rig.ingress.admit(cli_chat("r1", "hi")).await;
    assert_eq!(outage, Admission::Rejected(Rejection::Unavailable));
    assert!(!outage.settled());
    assert_eq!(
        rig.ingress.admit(cli_chat("r1", "hi")).await,
        accepted("run-1")
    );
}

mod active;
mod table;

use super::*;

mod call_limit;
mod effect_secret;
mod env;
mod limits;
mod network;
mod pin;
mod remote;
mod secrets;
use crate::{resolve::Resolved, testing::*};
use bot_core::{
    id::*,
    profile::Tool,
    state::{Conversation, Risk},
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

pub(super) fn schema() -> Value {
    json!({"type":"object","properties":{"event":{"type":"string"}},"required":["event"],"additionalProperties":false})
}

pub(super) fn decl(secrets: &[(&str, &str)], egress: &[&str], effects: bool) -> Tool {
    let mut t = crate::testing::tool(Risk::Irreversible, &["calendar.delete"]);
    // Key order and whitespace intentionally differ from `schema()` to verify canonical matching.
    t.input = JsonPayload(r#"{ "additionalProperties": false, "required": ["event"], "properties": {"event": {"type": "string"}}, "type": "object" }"#.into());
    t.revision = Revision("t1@img1".into());
    t.secrets = secrets
        .iter()
        .map(|(v, n)| (v.to_string(), SecretName::parse(n).unwrap()))
        .collect();
    t.egress = egress.iter().map(|e| e.to_string()).collect();
    t.effects = effects;
    t
}

pub(super) fn job_for(profile: &str, tool: Tool, args: &str) -> ToolJob {
    ToolJob {
        attempt: Attempt {
            profile: ProfileId(profile.into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        call: CallId("c1".into()),
        principal: PrincipalId("http:inbox/reader".into()),
        origin: bot_core::state::RunOrigin::Http,
        tool: Resolved::new(ToolId("agenda.delete".into()), tool).unwrap(),
        args: args.into(),
        env: Vec::new(),
        reply_to: None,
        owner: "r1.1.c1".into(),
    }
}

pub(super) fn job(tool: Tool, args: &str) -> ToolJob {
    job_for("personal", tool, args)
}

pub(super) struct Rig {
    pub(super) runner: McpRunner,
    pub(super) sandbox: Arc<FakeSandbox>,
    pub(super) secrets: Arc<FakeSecrets>,
    pub(super) journal: Arc<MemJournal>,
    pub(super) connector: Arc<FakeConnector>,
}

pub(super) fn rig(
    server: Server,
    cfg: impl FnOnce(&mut RunnerConfig),
    registry: impl FnOnce(&mut HostRegistry),
) -> Rig {
    let sandbox = Arc::new(FakeSandbox::new(server));
    let secrets = Arc::new(FakeSecrets::new(&[
        ("personal", "reader", "value-personal-1"),
        ("other", "reader", "value-other-2"),
        ("personal", "password", "pw1"),
        ("personal", "numeric", "123456789012"),
    ]));
    let journal = Arc::new(MemJournal::new());
    let resolver = Arc::new(FakeResolver::new(&[
        ("api.example", &["93.184.216.34"]),
        ("files.example", &["93.184.216.35"]),
        ("rebind.example", &["10.0.0.7"]),
    ]));
    let connector = Arc::new(FakeConnector::new());
    let mut host = HostRegistry {
        servers: [("demo-agenda".to_owned(), "img1".to_owned())].into(),
        ..HostRegistry::default()
    };
    registry(&mut host);
    let mut config = RunnerConfig {
        call_timeout: Duration::from_millis(300),
        ..RunnerConfig::default()
    };
    cfg(&mut config);
    let runner = McpRunner::new(
        Deps {
            sandbox: sandbox.clone(),
            secrets: secrets.clone(),
            effects: journal.clone(),
            resolver,
            connector: connector.clone(),
            entropy: Arc::new(CountingEntropy::default()),
        },
        host,
        config,
    );
    Rig {
        runner,
        sandbox,
        secrets,
        journal,
        connector,
    }
}

pub(super) fn plain() -> Rig {
    rig(Server::new(&[("delete", schema())]), |_| {}, |_| {})
}

pub(super) async fn run(rig: &Rig, job: ToolJob) -> Ran {
    rig.runner.run(job).await.unwrap()
}

pub(super) fn done_text(ran: &Ran) -> &str {
    match ran {
        Ran::Done(payload) => &payload.0,
        other => panic!("{other:?}"),
    }
}

pub(super) fn blocked(ran: &Ran) -> &'static str {
    match ran {
        Ran::Blocked(code) => code,
        other => panic!("{other:?}"),
    }
}

pub(super) fn server_saying(text: &'static str) -> Server {
    Server::new(&[("delete", schema())])
        .replying(move || Answer::Result(json!({"content": [{"type": "text", "text": text}]})))
}

#[tokio::test]
async fn a_declared_call_runs_in_a_fresh_sandbox_with_only_what_was_declared() {
    let r = plain();
    let ran = run(
        &r,
        job(
            decl(&[("TOKEN", "reader")], &[], false),
            r#"{"event":"walk"}"#,
        ),
    )
    .await;
    assert_eq!(
        done_text(&ran),
        r#"{"content":[{"text":"ok","type":"text"}],"isError":false}"#
    );
    let specs = r.sandbox.specs();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!(
        (
            spec.owner.as_str(),
            spec.server.as_str(),
            spec.image.as_str()
        ),
        ("r1.1.c1", "demo-agenda", "img1")
    );
    assert_eq!(
        spec.env,
        vec![("TOKEN".to_string(), "value-personal-1".to_string())],
        "env must contain only declared secrets"
    );
    assert!(spec.egress.is_none() && !spec.effects);
    assert_eq!(
        r.sandbox.methods(),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    assert_eq!(r.sandbox.terminated(), vec!["r1.1.c1".to_string()]);
}

#[tokio::test]
async fn a_sibling_tool_never_sees_another_tools_secrets() {
    let r = plain();
    run(
        &r,
        job(decl(&[("TOKEN", "reader")], &[], false), r#"{"event":"a"}"#),
    )
    .await;
    run(&r, job(decl(&[], &[], false), r#"{"event":"b"}"#)).await;
    let specs = r.sandbox.specs();
    assert_eq!(specs[0].env.len(), 1);
    assert!(
        specs[1].env.is_empty(),
        "sibling tool on the same server must not inherit env: {:?}",
        specs[1].env
    );
    assert_eq!(
        r.secrets.resolved(),
        vec![("personal".to_string(), "reader".to_string())]
    );
}

#[tokio::test]
async fn profiles_with_the_same_secret_name_get_only_their_own_value() {
    let r = plain();
    run(
        &r,
        job_for(
            "personal",
            decl(&[("TOKEN", "reader")], &[], false),
            r#"{"event":"a"}"#,
        ),
    )
    .await;
    run(
        &r,
        job_for(
            "other",
            decl(&[("TOKEN", "reader")], &[], false),
            r#"{"event":"a"}"#,
        ),
    )
    .await;
    let specs = r.sandbox.specs();
    assert_eq!(specs[0].env[0].1, "value-personal-1");
    assert_eq!(specs[1].env[0].1, "value-other-2");
    let ran = run(
        &r,
        job_for(
            "third",
            decl(&[("TOKEN", "reader")], &[], false),
            r#"{"event":"a"}"#,
        ),
    )
    .await;
    assert!(matches!(ran, Ran::NotSent));
    assert_eq!(r.sandbox.specs().len(), 2);
}

#[tokio::test]
async fn arguments_arrive_byte_for_byte_and_meta_comes_only_from_the_host() {
    let r = plain();
    let args = r#"{"_meta":{"bot/call-id":"evil","bot/principal":"local:someone-else","bot/trigger":"schedule"},"event":"walk"}"#;
    run(&r, job(decl(&[], &[], false), args)).await;
    let call = &r.sandbox.calls()[0];
    assert_eq!(
        call["_meta"],
        json!({"bot/call-id":"c1","bot/generation":1,"bot/principal":"http:inbox/reader","bot/profile-id":"personal","bot/run-id":"r1","bot/trigger":"http"})
    );
    assert_eq!(
        call["arguments"]["_meta"]["bot/trigger"], "schedule",
        "model-supplied origin stays inside arguments and cannot override host _meta"
    );
    assert_eq!(call["name"], "delete");
    assert_eq!(
        call["arguments"]["_meta"]["bot/call-id"], "evil",
        "model-supplied call ID stays inside arguments"
    );
    assert!(
        r.sandbox.raw_calls()[0].contains(&format!("\"arguments\":{args},")),
        "approved argument bytes must be forwarded verbatim"
    );
}

#[tokio::test]
async fn a_reply_tool_is_told_the_original_conversation_through_meta() {
    let r = plain();
    let mut j = job(decl(&[], &[], false), r#"{"event":"a"}"#);
    j.reply_to = Some(Conversation {
        kind: ConversationKind::Cli,
        id: "desk".into(),
        thread: Some("t-1".into()),
    });
    run(&r, j).await;
    run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
    let calls = r.sandbox.calls();
    assert_eq!(
        calls[0]["_meta"]["bot/destination"],
        json!({"id": "desk", "kind": "cli", "thread": "t-1"})
    );
    assert!(calls[1]["_meta"].get("bot/destination").is_none());
}

#[tokio::test]
async fn a_server_that_disagrees_with_the_declaration_is_never_called() {
    let other = json!({"type":"object","properties":{"event":{"type":"string"},"extra":{}},"additionalProperties":false});
    let cases: Vec<(&str, Server)> = vec![
        ("schema differs", Server::new(&[("delete", other)])),
        ("tool missing", Server::new(&[("other", schema())])),
        (
            "tool listed twice",
            Server::new(&[("delete", schema()), ("delete", schema())]),
        ),
        (
            "unsupported protocol",
            Server::new(&[("delete", schema())]).version("2099-01-01"),
        ),
    ];
    for (name, server) in cases {
        let r = rig(server, |_| {}, |_| {});
        let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
        assert!(matches!(ran, Ran::NotSent), "{name}: {ran:?}");
        assert!(
            !r.sandbox.methods().contains(&"tools/call".to_string()),
            "{name}"
        );
        assert_eq!(
            r.sandbox.terminated().len(),
            1,
            "{name}: spawned process must be terminated"
        );
    }
}

#[tokio::test]
async fn a_good_result_is_rebuilt_in_a_fixed_shape_dropping_unknown_fields() {
    let server = Server::new(&[("delete", schema())]).replying(|| {
        Answer::Result(json!({
            "content": [{"type": "text", "text": "done", "annotations": {"audience": ["user"]}, "_meta": {"x": 1}}],
            "isError": false, "structuredContent": {"b": 2, "a": 1}, "_meta": {"secretish": true}, "extra": 5
        }))
    });
    let r = rig(server, |_| {}, |_| {});
    let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
    assert_eq!(
        done_text(&ran),
        r#"{"content":[{"text":"done","type":"text"}],"isError":false,"structuredContent":{"a":1,"b":2}}"#
    );
}

#[tokio::test]
async fn a_call_survives_requests_and_notifications_sent_by_the_server() {
    let before = vec![
        r#"{"jsonrpc":"2.0","id":99,"method":"roots/list"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"data":"hi"}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":"s-1","method":"sampling/createMessage","params":{}}"#.to_owned(),
    ];
    let server = Server::new(&[("delete", schema())]).replying(move || {
        Answer::Before(
            before.clone(),
            Box::new(Answer::Result(
                json!({"content": [{"type": "text", "text": "ok"}]}),
            )),
        )
    });
    let r = rig(server, |_| {}, |_| {});
    let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
    assert_eq!(
        done_text(&ran),
        r#"{"content":[{"text":"ok","type":"text"}],"isError":false}"#
    );
}

#[tokio::test]
async fn an_image_other_than_the_approved_one_is_not_run() {
    let r = plain();
    let mut tool = decl(&[], &[], false);
    tool.revision = Revision("t1@img0".into());
    assert!(matches!(
        run(&r, job(tool, r#"{"event":"a"}"#)).await,
        Ran::NotSent
    ));
    let mut unregistered = decl(&[], &[], false);
    unregistered.server = "gone-server".into();
    assert!(matches!(
        run(&r, job(unregistered, r#"{"event":"a"}"#)).await,
        Ran::NotSent
    ));
    assert!(r.sandbox.specs().is_empty(), "must not spawn sandbox");
}

#[tokio::test]
async fn failing_to_start_the_sandbox_sends_nothing_and_closes_the_proxy() {
    let r = plain();
    r.sandbox.fail_spawn(true);
    assert!(matches!(
        run(
            &r,
            job(decl(&[], &["api.example:443"], false), r#"{"event":"a"}"#)
        )
        .await,
        Ran::NotSent
    ));
    assert!(r.sandbox.methods().is_empty());
}

#[tokio::test]
async fn reaping_and_the_installed_image_are_delegated() {
    let r = plain();
    assert_eq!(r.runner.installed("demo-agenda").as_deref(), Some("img1"));
    assert!(r.runner.installed("gone").is_none());
    r.runner.reap("r1.1.c1".into()).await.unwrap();
    r.sandbox.set_reap_ok(false);
    assert!(r.runner.reap("r1.1.c2".into()).await.is_err());
    assert_eq!(r.sandbox.reaped(), ["r1.1.c1", "r1.1.c2"]);
    let _ = AsyncReadExt::read(&mut tokio::io::empty(), &mut [0u8; 1]).await;
}

#[tokio::test]
async fn the_hosts_private_intake_key_is_never_resolved_for_a_tool() {
    let r = plain();
    let ran = run(
        &r,
        job(
            decl(&[("KEY", "private-intake")], &[], false),
            "{\"event\":\"x\"}",
        ),
    )
    .await;
    assert!(matches!(ran, Ran::NotSent), "{ran:?}");
    assert!(r.sandbox.specs().is_empty(), "must not spawn tool");
}

#[tokio::test]
async fn the_progress_origin_comes_from_the_job_for_every_kind_and_never_from_the_model() {
    use bot_core::state::RunOrigin;
    for (origin, word) in [
        (RunOrigin::Message, "message"),
        (RunOrigin::Http, "http"),
        (RunOrigin::Schedule, "schedule"),
        (RunOrigin::Wake, "wake"),
        (RunOrigin::Unknown, "unknown"),
    ] {
        let r = plain();
        let mut job = job(decl(&[], &[], false), r#"{"event":"x"}"#);
        job.origin = origin;
        run(&r, job).await;
        let call = &r.sandbox.calls()[0];
        assert_eq!(call["_meta"]["bot/trigger"], word);
        assert_eq!(
            call["_meta"].as_object().unwrap().len(),
            6,
            "{word}: only host _meta keys expected"
        );
    }
}

#[tokio::test]
async fn a_server_the_sandbox_cannot_admit_never_gets_its_secrets_resolved_or_started() {
    let r = plain();
    r.sandbox.set_refuse_admit(true);
    let ran = run(
        &r,
        job(decl(&[("TOKEN", "reader")], &[], false), r#"{"event":"x"}"#),
    )
    .await;
    assert!(matches!(ran, Ran::NotSent), "{ran:?}");
    assert!(
        r.secrets.resolved().is_empty(),
        "must reject before resolving secrets"
    );
    assert!(r.sandbox.specs().is_empty(), "must not spawn process");
    r.sandbox.set_refuse_admit(false);
    let ran = run(
        &r,
        job(decl(&[("TOKEN", "reader")], &[], false), r#"{"event":"x"}"#),
    )
    .await;
    assert!(matches!(ran, Ran::Done(_)), "{ran:?}");
    assert_eq!(r.secrets.resolved().len(), 1);
}

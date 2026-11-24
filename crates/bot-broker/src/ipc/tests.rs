use super::*;
use crate::{
    resolve::HostPort,
    testing::{FakeSecrets, FakeUpstream, ManualClock, MemLedger, run_context},
};
use bot_core::{
    command::CallResult,
    port::{Broker, Reply},
};
use tokio::io::{AsyncBufReadExt, duplex};

/// 받은 호출을 기록하고 적어 둔 결과를 돌려주는 브로커. 승인 결정은 받지 않는다.
#[derive(Default)]
struct Stub {
    calls: Mutex<Vec<ToolCall>>,
    script: Mutex<Vec<Result<CallResult, Error>>>,
}

impl Broker for Stub {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        self.calls.lock().unwrap().push(call);
        let next = self
            .script
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Ok(CallResult::Done(JsonPayload("{\"ok\":true}".into()))));
        Box::pin(async move { next })
    }
    fn decide(&self, _: ApprovalId, _: PrincipalId, _: bool) -> Reply<'_, ()> {
        panic!("모델 채널은 승인 결정에 닿지 않는다")
    }
}

struct Rig {
    session: ModelSession,
    stub: Arc<Stub>,
    upstream: Arc<FakeUpstream>,
    attempt: Attempt,
}

fn rig() -> Rig {
    let stub = Arc::new(Stub::default());
    let clock = Arc::new(ManualClock::new(1_000));
    let ledger = Arc::new(MemLedger::new(clock.clone()));
    ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run"],
    ));
    let upstream = Arc::new(FakeUpstream::new());
    let host = ModelHost {
        providers: [(
            "main".to_owned(),
            Provider {
                endpoint: HostPort {
                    host: "api.main.example".into(),
                    port: 443,
                },
                path: "/v1/messages".into(),
                secret: SecretName::parse("model-key").unwrap(),
                header: "authorization".into(),
                scheme: Some("Bearer".into()),
                headers: vec![],
                forward_headers: vec![],
                body_fields: vec![],
                models: [("assistant".to_owned(), "model-large".to_owned())].into(),
            },
        )]
        .into(),
        allow: [(ProfileId("personal".into()), ["main".to_owned()].into())].into(),
    };
    let secrets = Arc::new(FakeSecrets::new(&[(
        "personal",
        "model-key",
        "provider-key-xyz",
    )]));
    let proxy = Arc::new(ModelProxy::new(
        host,
        ledger,
        secrets,
        upstream.clone(),
        clock,
        ModelLimits::default(),
    ));
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    };
    let session = ModelSession::new(
        stub.clone(),
        proxy,
        attempt.clone(),
        "assistant".into(),
        Arc::new(SequentialIds::new("c")),
    );
    Rig {
        session,
        stub,
        upstream,
        attempt,
    }
}

async fn say(r: &Rig, frame: &str) -> String {
    r.session.handle(frame.as_bytes()).await
}

#[tokio::test]
async fn a_tool_frame_reaches_the_broker_with_the_hosts_identity_and_a_hosts_call_id() {
    let r = rig();
    let reply = say(
        &r,
        r#"{"v":1,"id":"7","op":"tool","tool":"agenda.list","args":{"b":1,"a":[2.0]}}"#,
    )
    .await;
    assert_eq!(
        reply,
        r#"{"v":1,"id":"7","state":"done","result":{"ok":true}}"#
    );
    say(
        &r,
        r#"{"v":1,"id":"8","op":"tool","tool":"agenda.list","args":{}}"#,
    )
    .await;
    let calls = r.stub.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].attempt, r.attempt);
    assert_eq!(
        (calls[0].call.0.as_str(), calls[1].call.0.as_str()),
        ("c-1", "c-2")
    );
    assert_eq!(
        (calls[0].tool.0.as_str(), calls[0].args.0.as_str()),
        ("agenda.list", r#"{"a":[2],"b":1}"#)
    );
}

#[tokio::test]
async fn the_sandbox_cannot_name_the_call_run_generation_or_principal() {
    let r = rig();
    for extra in [
        "call",
        "run",
        "generation",
        "claim",
        "profile",
        "principal",
        "attempt",
        "approver",
        "nonce",
    ] {
        let frame = format!(
            r#"{{"v":1,"id":"1","op":"tool","tool":"agenda.list","args":{{}},"{extra}":"x"}}"#
        );
        assert_eq!(
            say(&r, &frame).await,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
            "{extra}"
        );
    }
    let model = r#"{"v":1,"id":"1","op":"model","body":{},"profile":"other"}"#;
    assert_eq!(
        say(&r, model).await,
        r#"{"v":1,"id":"1","error":"invalid"}"#
    );
    assert!(r.stub.calls.lock().unwrap().is_empty() && r.upstream.requests().is_empty());
}

#[tokio::test]
async fn there_is_no_way_to_approve_through_the_model_channel() {
    let r = rig();
    for op in ["decide", "approve", "effect", "secret", ""] {
        let frame = format!(r#"{{"v":1,"id":"1","op":"{op}","nonce":"abc","allow":true}}"#);
        assert_eq!(
            say(&r, &frame).await,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
            "{op}"
        );
    }
    // 승인 대기 결과에도 nonce는 없다
    r.stub
        .script
        .lock()
        .unwrap()
        .push(Ok(CallResult::Awaiting(ApprovalId("secret-nonce".into()))));
    let waiting = say(
        &r,
        r#"{"v":1,"id":"2","op":"tool","tool":"agenda.delete","args":{}}"#,
    )
    .await;
    assert_eq!(waiting, r#"{"v":1,"id":"2","state":"awaiting"}"#);
}

#[tokio::test]
async fn results_and_errors_map_to_fixed_classes() {
    let r = rig();
    r.stub.script.lock().unwrap().extend([
        Err(Error::Unavailable),
        Err(Error::Stale),
        Err(Error::Conflict),
        Ok(CallResult::Held(CallId("c-9".into()))),
        Err(Error::Denied),
    ]);
    let mut seen = Vec::new();
    for n in 0..5 {
        seen.push(
            say(
                &r,
                &format!(r#"{{"v":1,"id":"{n}","op":"tool","tool":"a.b","args":{{}}}}"#),
            )
            .await,
        );
    }
    assert_eq!(seen[0], r#"{"v":1,"id":"0","error":"denied"}"#);
    assert_eq!(seen[1], r#"{"v":1,"id":"1","state":"held"}"#);
    assert_eq!(seen[2], r#"{"v":1,"id":"2","error":"conflict"}"#);
    assert_eq!(seen[3], r#"{"v":1,"id":"3","error":"stale"}"#);
    assert_eq!(seen[4], r#"{"v":1,"id":"4","error":"unavailable"}"#);
}

#[tokio::test]
async fn a_model_frame_goes_through_the_proxy_without_exposing_the_key() {
    let r = rig();
    let reply = say(
        &r,
        r#"{"v":1,"id":"m1","op":"model","body":{"messages":[]}}"#,
    )
    .await;
    assert_eq!(
        reply,
        r#"{"v":1,"id":"m1","status":200,"body":{"ok":true}}"#
    );
    let sent = r.upstream.requests();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0]
            .2
            .iter()
            .any(|(k, v)| k == "authorization" && v.contains("provider-key-xyz"))
    );
    assert!(!reply.contains("provider-key-xyz"));
    // 샌드박스는 모델 별칭을 고를 수 없다(호스트가 세션에 고정한다): 별칭 키가 있으면 프레임 전체를 거절한다
    for frame in [
        r#"{"v":1,"id":"m2","op":"model","alias":"hidden","body":{}}"#,
        r#"{"v":1,"id":"m3","op":"model","alias":"assistant","body":{}}"#,
    ] {
        assert!(
            say(&r, frame).await.contains(r#""error":"invalid""#),
            "{frame}"
        );
    }
    assert_eq!(r.upstream.requests().len(), 1);
}

#[tokio::test]
async fn malformed_frames_get_a_classification_only() {
    let r = rig();
    let cases = [
        ("not json", r#"{"v":1,"error":"invalid"}"#),
        (r#"[1]"#, r#"{"v":1,"error":"invalid"}"#),
        (r#"{"v":1,"op":"tool"}"#, r#"{"v":1,"error":"invalid"}"#),
        (
            r#"{"v":2,"id":"1","op":"tool"}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","op":"tool","tool":"a.b","args":[1]}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","op":"tool","tool":"","args":{}}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","op":"tool","tool":"a.b","args":{"a":1,"a":2}}"#,
            r#"{"v":1,"error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","op":"model","body":"text"}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","op":"tool","tool":"a.b","args":{"n":9007199254740993}}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
    ];
    for (frame, expected) in cases {
        assert_eq!(say(&r, frame).await, expected, "{frame}");
    }
    assert!(r.stub.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_serve_loop_answers_each_line_and_stops_on_signal_or_oversize() {
    let r = rig();
    let (mut tool_write, broker_read) = duplex(1 << 16);
    let (broker_write, tool_read) = duplex(1 << 16);
    let (stop, signal) = watch::channel(false);
    let task = tokio::spawn(
        r.session
            .serve(Box::new(broker_read), Box::new(broker_write), signal),
    );
    tool_write
        .write_all(b"{\"v\":1,\"id\":\"1\",\"op\":\"tool\",\"tool\":\"a.b\",\"args\":{}}\n")
        .await
        .unwrap();
    let mut replies = BufReader::new(tool_read);
    let mut line = String::new();
    replies.read_line(&mut line).await.unwrap();
    assert!(line.contains(r#""state":"done""#), "{line}");
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    // 상한을 넘는 줄은 채널을 닫는다
    let r2 = rig();
    let (mut w, rd) = duplex(MAX_FRAME + 4096);
    let (bw, tr) = duplex(1 << 16);
    let (_keep, sig) = watch::channel(false);
    let task = tokio::spawn(r2.session.serve(Box::new(rd), Box::new(bw), sig));
    w.write_all(format!("{}\n", "a".repeat(MAX_FRAME + 10)).as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    BufReader::new(tr).read_line(&mut out).await.unwrap();
    assert_eq!(out.trim(), r#"{"v":1,"error":"invalid"}"#);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

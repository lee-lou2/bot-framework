//! CLI ingress acceptance tests over a real Unix socket with peer UID verification.
use bot_core::{
    id::{BindingId, PrincipalId, ProfileId, RequestId},
    intake::RequestKey,
    profile::Adapter,
    state::RunState,
};
use bot_gateway::{
    active::ActiveCheck,
    cli::{
        CliGateway,
        client::{self, Chat, Outcome},
        socket,
    },
    host::HostConfig,
    ingress::Ingress,
    testing::{ports::MemoryActive, *},
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

struct Rig {
    socket: PathBuf,
    store: Arc<MemoryStore>,
    catalog: Arc<TestCatalog>,
    stop: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn desk(profile_name: &str, account: &str) -> Fixture {
    profile(profile_name)
        .binding(
            "desk",
            Adapter::Cli {
                account: account.into(),
            },
            "main",
        )
        .member(&format!("local:{account}"), &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

async fn rig() -> Rig {
    let uid = socket::current_uid().unwrap();
    rig_with(uid, vec![desk("personal", "owner")], |_| {}).await
}

async fn rig_with(
    account_uid: u32,
    profiles: Vec<Fixture>,
    tune: impl FnOnce(&mut HostConfig),
) -> Rig {
    rig_checked(account_uid, profiles, None, tune).await
}

async fn rig_checked(
    account_uid: u32,
    profiles: Vec<Fixture>,
    active: Option<Arc<dyn ActiveCheck>>,
    tune: impl FnOnce(&mut HostConfig),
) -> Rig {
    let uid = socket::current_uid().unwrap();
    let mut accounts = serde_json::Map::new();
    for name in ["owner", "second", "ghost"] {
        accounts.insert(name.into(), json!({"uid": account_uid}));
    }
    let mut host = HostConfig::from_value(json!({"cli": {"accounts": accounts}})).unwrap();
    tune(&mut host);
    let store = MemoryStore::new();
    let catalog = TestCatalog::new(profiles.into_iter().map(Fixture::build));
    let mut ingress = Ingress::new(
        catalog.clone(),
        Arc::new(host),
        store.clone(),
        MemoryPrivate::new(),
    );
    if let Some(active) = active {
        ingress = ingress.with_active(active);
    }
    // Keep socket path short to stay within the 104-byte sun_path limit on macOS.
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("run").join(socket::FILE_NAME);
    let listener = socket::bind(&path, uid).unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(
        Arc::new(CliGateway::new(Arc::new(ingress), store.clone())).serve(listener, stop.clone()),
    );
    Rig {
        socket: path,
        store,
        catalog,
        stop,
        _dir: dir,
    }
}

fn key(request: &str) -> RequestKey {
    RequestKey {
        profile: ProfileId("personal".into()),
        binding: BindingId("desk".into()),
        principal: PrincipalId("local:owner".into()),
        request: RequestId(request.into()),
    }
}

fn chat(request: &str, text: &str) -> Chat {
    Chat {
        profile: None,
        request: request.into(),
        thread: None,
        text: text.into(),
        wait: true,
    }
}

fn failed(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failed { code, .. } => code,
        other => panic!("expected failure, got {other:?}"),
    }
}

async fn raw(rig: &Rig, line: &[u8]) -> Vec<Value> {
    let mut stream = UnixStream::connect(&rig.socket).await.unwrap();
    stream.write_all(line).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    let mut lines = BufReader::new(stream).lines();
    let mut out = Vec::new();
    while let Ok(Ok(Some(text))) =
        tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await
    {
        out.push(serde_json::from_str(&text).unwrap());
    }
    out
}

#[tokio::test]
async fn chat_round_trip_reaches_the_bound_agent_and_returns_the_result() {
    let rig = rig().await;
    let store = rig.store.clone();
    let finisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        store.set_state(
            &key("r1"),
            RunState::Succeeded,
            Some(r#"{"text":"3 events"}"#),
        );
    });
    let mut events = Vec::new();
    let outcome = client::chat(&rig.socket, &chat("r1", "Show today's agenda"), |e| {
        events.push(e.clone())
    })
    .await
    .unwrap();
    finisher.await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Done {
            state: "succeeded".into(),
            result: Some(json!({"text": "3 events"}))
        }
    );
    assert_eq!(client::render(&json!({"text": "3 events"})), "3 events");
    assert_eq!(events[0]["type"], "accepted");
    let accepted = rig.store.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].target, "agent main Show today's agenda");
    assert_eq!(accepted[0].approver.as_deref(), Some("local:owner"));
    assert_eq!(accepted[0].scopes, ["run"]);
}

#[tokio::test]
async fn the_same_request_id_reattaches_without_a_second_run() {
    let rig = rig().await;
    let mut first = chat("re1", "delete it");
    first.wait = false;
    assert_eq!(
        client::chat(&rig.socket, &first, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    assert_eq!(
        client::chat(&rig.socket, &first, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    assert_eq!(rig.store.accepted().len(), 1);
    let mut changed = chat("re1", "different text");
    changed.wait = false;
    let outcome = client::chat(&rig.socket, &changed, |_| {}).await.unwrap();
    assert_eq!(failed(&outcome), "conflict");
    rig.store
        .set_state(&key("re1"), RunState::Succeeded, Some(r#""deleted""#));
    let again = client::result(&rig.socket, None, "re1", true, |_| {})
        .await
        .unwrap();
    assert_eq!(
        again,
        Outcome::Done {
            state: "succeeded".into(),
            result: Some(json!("deleted"))
        }
    );
    assert_eq!(rig.store.accepted().len(), 1);
    let missing = client::result(&rig.socket, None, "never", false, |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&missing), "not_found");
}

#[tokio::test]
async fn waiting_runs_report_their_state_and_finish_later() {
    let rig = rig().await;
    let store = rig.store.clone();
    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        store.set_state(&key("w1"), RunState::Waiting, None);
        tokio::time::sleep(Duration::from_millis(300)).await;
        store.set_state(
            &key("w1"),
            RunState::Succeeded,
            Some(r#"{"text":"Approved and deleted"}"#),
        );
    });
    let mut types = Vec::new();
    let outcome = client::chat(&rig.socket, &chat("w1", "Delete the walk event"), |e| {
        types.push(format!(
            "{}:{}",
            e["type"].as_str().unwrap(),
            e["state"].as_str().unwrap_or("")
        ));
    })
    .await
    .unwrap();
    driver.await.unwrap();
    assert!(matches!(outcome, Outcome::Done { ref state, .. } if state == "succeeded"));
    assert_eq!(types, ["accepted:", "state:waiting", "result:succeeded"]);
    for (request, state, want) in [
        ("dead1", RunState::Dead, "dead"),
        ("can1", RunState::Cancelled, "cancelled"),
    ] {
        let mut start = chat(request, "x");
        start.wait = false;
        client::chat(&rig.socket, &start, |_| {}).await.unwrap();
        rig.store.set_state(&key(request), state, None);
        let outcome = client::result(&rig.socket, None, request, true, |_| {})
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Outcome::Done {
                state: want.into(),
                result: None
            }
        );
    }
}

#[tokio::test]
async fn a_peer_without_an_account_gets_no_entrance_and_nothing_is_accepted() {
    let uid = socket::current_uid().unwrap();
    // Map accounts to another UID so the connecting peer has no matching entrance.
    let rig = rig_with(uid + 1, vec![desk("personal", "owner")], |_| {}).await;
    let outcome = client::chat(&rig.socket, &chat("n1", "hi"), |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&outcome), "no_entrance");
    let outcome = client::result(&rig.socket, None, "n1", false, |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&outcome), "no_entrance");
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn members_are_checked_and_limits_are_reported() {
    let uid = socket::current_uid().unwrap();
    let stranger = profile("personal")
        .binding(
            "desk",
            Adapter::Cli {
                account: "owner".into(),
            },
            "main",
        )
        .role("owner", &["run"])
        .policy_scopes(&["run"]);
    let rig = rig_with(uid, vec![stranger], |_| {}).await;
    let outcome = client::chat(&rig.socket, &chat("f1", "hi"), |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&outcome), "forbidden");

    let rig = rig_with(uid, vec![desk("personal", "owner")], |h| {
        h.limits.per_principal_per_min = Some(2)
    })
    .await;
    for n in 0..2 {
        let mut c = chat(&format!("l{n}"), "hi");
        c.wait = false;
        assert_eq!(
            client::chat(&rig.socket, &c, |_| {}).await.unwrap(),
            Outcome::Accepted
        );
    }
    let mut c = chat("l9", "hi");
    c.wait = false;
    match client::chat(&rig.socket, &c, |_| {}).await.unwrap() {
        Outcome::Failed {
            code,
            retry_after_secs,
            ..
        } => {
            assert_eq!(code, "rate_limited");
            assert!(retry_after_secs.unwrap() >= 1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.store.accepted().len(), 2);
}

#[tokio::test]
async fn malformed_oversized_and_unknown_requests_fail_closed() {
    let rig = rig().await;
    let code = |lines: &[Value]| {
        lines
            .iter()
            .find(|l| l["type"] == "error")
            .map(|l| l["code"].as_str().unwrap().to_string())
    };
    for (name, line) in [
        ("not json", b"{nope".to_vec()),
        ("array", b"[1]".to_vec()),
        (
            "wrong version",
            br#"{"v":2,"op":"chat","request":"a","text":"x"}"#.to_vec(),
        ),
        (
            "unknown op",
            br#"{"v":1,"op":"shell","request":"a","text":"x"}"#.to_vec(),
        ),
        (
            "no request id",
            br#"{"v":1,"op":"chat","text":"x"}"#.to_vec(),
        ),
        (
            "bad request id",
            br#"{"v":1,"op":"chat","request":"a b","text":"x"}"#.to_vec(),
        ),
        (
            "blank text",
            br#"{"v":1,"op":"chat","request":"a","text":"  "}"#.to_vec(),
        ),
        (
            "text on result",
            br#"{"v":1,"op":"result","request":"a","text":"x"}"#.to_vec(),
        ),
        (
            "bad profile",
            br#"{"v":1,"op":"chat","request":"a","text":"x","profile":"A/B"}"#.to_vec(),
        ),
        (
            "bad wait",
            br#"{"v":1,"op":"chat","request":"a","text":"x","wait":"yes"}"#.to_vec(),
        ),
    ] {
        assert_eq!(
            code(&raw(&rig, &line).await).as_deref(),
            Some("invalid"),
            "{name}"
        );
    }
    let big = format!(
        r#"{{"v":1,"op":"chat","request":"a","text":"{}"}}"#,
        "x".repeat(70_000)
    );
    assert_eq!(
        code(&raw(&rig, big.as_bytes()).await).as_deref(),
        Some("too_large")
    );
    let unknown = br#"{"v":1,"op":"chat","request":"a","text":"x","profile":"nobody"}"#;
    assert_eq!(
        code(&raw(&rig, unknown).await).as_deref(),
        Some("no_entrance")
    );
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn several_profiles_for_one_user_require_an_explicit_choice() {
    let uid = socket::current_uid().unwrap();
    let rig = rig_with(
        uid,
        vec![desk("personal", "owner"), desk("work", "second")],
        |_| {},
    )
    .await;
    let outcome = client::chat(&rig.socket, &chat("p1", "hi"), |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&outcome), "profile_required");
    let mut chosen = chat("p2", "hi");
    chosen.profile = Some("work".into());
    chosen.wait = false;
    assert_eq!(
        client::chat(&rig.socket, &chosen, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    let accepted = rig.store.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].key.profile.0, "work");
    assert_eq!(accepted[0].key.principal.0, "local:second");
}

#[tokio::test]
async fn a_client_that_leaves_does_not_stop_the_run_or_the_server() {
    let rig = rig().await;
    {
        let mut stream = UnixStream::connect(&rig.socket).await.unwrap();
        let line = json!({"v": 1, "op": "chat", "request": "gone1", "text": "long task"})
            .to_string()
            + "\n";
        stream.write_all(line.as_bytes()).await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["type"], "accepted");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.store.accepted().len(), 1);
    rig.store.set_state(
        &key("gone1"),
        RunState::Succeeded,
        Some(r#"{"text":"done"}"#),
    );
    let outcome = client::result(&rig.socket, None, "gone1", true, |_| {})
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Done { ref state, .. } if state == "succeeded"));
    let mut next = chat("after", "hi");
    next.wait = false;
    assert_eq!(
        client::chat(&rig.socket, &next, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
}

#[tokio::test]
async fn a_revoked_user_gets_no_state_or_result_over_the_socket() {
    let rig = rig().await;
    let (store, catalog) = (rig.store.clone(), rig.catalog.clone());
    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Revoke membership while waiting, before the result is committed.
        let revoked = profile("personal")
            .binding(
                "desk",
                Adapter::Cli {
                    account: "owner".into(),
                },
                "main",
            )
            .role("owner", &["run"])
            .policy_scopes(&["run"]);
        catalog.replace([revoked.build()]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.set_state(
            &key("v1"),
            RunState::Succeeded,
            Some(r#"{"text":"synthetic-result-5b2d"}"#),
        );
    });
    let mut events = Vec::new();
    let outcome = client::chat(&rig.socket, &chat("v1", "hi"), |e| {
        events.push(e.to_string())
    })
    .await
    .unwrap();
    driver.await.unwrap();
    assert_eq!(failed(&outcome), "forbidden");
    assert!(
        events.iter().all(|e| !e.contains("synthetic-result-5b2d")),
        "{events:?}"
    );
    let again = client::result(&rig.socket, None, "v1", false, |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&again), "forbidden");
}

#[tokio::test]
async fn a_stale_process_closes_new_chats_and_result_reads() {
    let active = MemoryActive::new(Some("set-1"));
    let uid = socket::current_uid().unwrap();
    let rig = rig_checked(
        uid,
        vec![desk("personal", "owner")],
        Some(active.check("set-1")),
        |_| {},
    )
    .await;
    let start = Chat {
        wait: false,
        ..chat("a1", "hi")
    };
    let first = client::chat(&rig.socket, &start, |_| {}).await.unwrap();
    assert_eq!(first, Outcome::Accepted);
    rig.store.set_state(
        &key("a1"),
        RunState::Succeeded,
        Some(r#"{"text":"synthetic-result-77c1"}"#),
    );
    let ok = client::result(&rig.socket, None, "a1", true, |_| {})
        .await
        .unwrap();
    assert!(matches!(ok, Outcome::Done { .. }), "{ok:?}");

    // Another process activated a newer set; both admissions and reads fail as unavailable.
    active.set(Some("set-2"));
    let mut events = Vec::new();
    let result = client::result(&rig.socket, None, "a1", true, |e| {
        events.push(e.to_string())
    })
    .await
    .unwrap();
    assert_eq!(failed(&result), "unavailable");
    assert!(events.iter().all(|e| !e.contains("synthetic-result-77c1")));
    let new = client::chat(&rig.socket, &chat("a2", "hi"), |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&new), "unavailable");
    assert_eq!(
        rig.store.accepted().len(),
        1,
        "stale process must not write to the store"
    );

    active.set(Some("set-1"));
    let back = client::result(&rig.socket, None, "a1", true, |_| {})
        .await
        .unwrap();
    assert!(matches!(back, Outcome::Done { .. }), "{back:?}");
}

#[tokio::test]
async fn cli_chat_uses_the_same_routes_snapshot_for_auth_and_admit() {
    let rig = rig().await;
    // Flip active revision to r2 right after the first routes lookup during auth.
    rig.catalog.flip_after(
        1,
        [desk("personal", "owner").revision("personal-r2").build()],
    );
    let start = Chat {
        wait: false,
        ..chat("snap-1", "same routes snapshot")
    };
    assert_eq!(
        client::chat(&rig.socket, &start, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    let accepted = rig.store.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        accepted[0].revision, "personal-r1",
        "admission must use the exact routing snapshot used for auth"
    );
}

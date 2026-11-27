//! CLI 입구 수락 시험: 유닉스 소켓 + 피어 uid → 접수 → 결과 조회·재부착. 실제 소켓으로 붙는다.
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

/// 저장소의 활성 집합과 견주는 확인(`active`)을 붙여 조립한다. None이면 확인하지 않는다.
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
    // 유닉스 소켓 주소는 104바이트 안이어야 해서 짧은 위치에 만든다
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
        other => panic!("실패를 기대했는데 {other:?}"),
    }
}

/// 원시 한 줄을 보내고 응답 줄들을 모두 읽는다.
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
            Some(r#"{"text":"일정 3건"}"#),
        );
    });
    let mut events = Vec::new();
    let outcome = client::chat(&rig.socket, &chat("r1", "오늘 일정 알려줘"), |e| {
        events.push(e.clone())
    })
    .await
    .unwrap();
    finisher.await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Done {
            state: "succeeded".into(),
            result: Some(json!({"text": "일정 3건"}))
        }
    );
    assert_eq!(client::render(&json!({"text": "일정 3건"})), "일정 3건");
    assert_eq!(events[0]["type"], "accepted");
    let accepted = rig.store.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].target, "agent main 오늘 일정 알려줘");
    assert_eq!(accepted[0].approver.as_deref(), Some("local:owner"));
    assert_eq!(accepted[0].scopes, ["run"]);
}

#[tokio::test]
async fn the_same_request_id_reattaches_without_a_second_run() {
    let rig = rig().await;
    let mut first = chat("re1", "삭제해줘");
    first.wait = false;
    assert_eq!(
        client::chat(&rig.socket, &first, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    // 같은 요청 ID·같은 내용의 재시도는 기존 접수
    assert_eq!(
        client::chat(&rig.socket, &first, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    assert_eq!(rig.store.accepted().len(), 1);
    // 다른 내용은 충돌
    let mut changed = chat("re1", "다른 내용");
    changed.wait = false;
    let outcome = client::chat(&rig.socket, &changed, |_| {}).await.unwrap();
    assert_eq!(failed(&outcome), "conflict");
    // 끝난 뒤 같은 요청 ID로 결과만 다시 받는다
    rig.store
        .set_state(&key("re1"), RunState::Succeeded, Some(r#""삭제했어요""#));
    let again = client::result(&rig.socket, None, "re1", true, |_| {})
        .await
        .unwrap();
    assert_eq!(
        again,
        Outcome::Done {
            state: "succeeded".into(),
            result: Some(json!("삭제했어요"))
        }
    );
    assert_eq!(rig.store.accepted().len(), 1);
    // 없는 요청 ID
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
            Some(r#"{"text":"승인돼서 지웠어요"}"#),
        );
    });
    let mut types = Vec::new();
    let outcome = client::chat(
        &rig.socket,
        &chat("w1", "산책 일정을 삭제해줘"),
        |e| {
            types.push(format!(
                "{}:{}",
                e["type"].as_str().unwrap(),
                e["state"].as_str().unwrap_or("")
            ));
        },
    )
    .await
    .unwrap();
    driver.await.unwrap();
    assert!(matches!(outcome, Outcome::Done { ref state, .. } if state == "succeeded"));
    assert_eq!(types, ["accepted:", "state:waiting", "result:succeeded"]);
    // dead·cancelled도 끝으로 알린다
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
    // 호스트가 이 계정을 다른 uid로 연결해 두었다 → 접속한 uid에게는 입구가 없다
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
    // 멤버가 없는 프로필
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
    // 본문 한도(기본 64 KiB)를 넘는 줄
    let big = format!(
        r#"{{"v":1,"op":"chat","request":"a","text":"{}"}}"#,
        "x".repeat(70_000)
    );
    assert_eq!(
        code(&raw(&rig, big.as_bytes()).await).as_deref(),
        Some("too_large")
    );
    // 알 수 없는 프로필은 입구가 없다
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
        let line = json!({"v": 1, "op": "chat", "request": "gone1", "text": "오래 걸리는 일"})
            .to_string()
            + "\n";
        stream.write_all(line.as_bytes()).await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["type"], "accepted");
        // 연결을 닫는다(Ctrl-C)
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.store.accepted().len(), 1);
    rig.store.set_state(
        &key("gone1"),
        RunState::Succeeded,
        Some(r#"{"text":"끝났어요"}"#),
    );
    let outcome = client::result(&rig.socket, None, "gone1", true, |_| {})
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Done { ref state, .. } if state == "succeeded"));
    // 서버는 계속 일한다
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
        // 기다리는 동안 멤버에서 철회된 뒤 결과가 나온다
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
            Some(r#"{"text":"합성-결과-5b2d"}"#),
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
        events.iter().all(|e| !e.contains("합성-결과-5b2d")),
        "{events:?}"
    );
    // 철회 뒤 같은 요청 ID로 결과를 다시 받으려 해도 나가지 않는다
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
        Some(r#"{"text":"합성-결과-77c1"}"#),
    );
    let ok = client::result(&rig.socket, None, "a1", true, |_| {})
        .await
        .unwrap();
    assert!(matches!(ok, Outcome::Done { .. }), "{ok:?}");

    // 다른 프로세스가 새 집합을 활성화했다 — 새 접수와 결과 조회 모두 일시 실패로 닫힌다
    active.set(Some("set-2"));
    let mut events = Vec::new();
    let result = client::result(&rig.socket, None, "a1", true, |e| {
        events.push(e.to_string())
    })
    .await
    .unwrap();
    assert_eq!(failed(&result), "unavailable");
    assert!(events.iter().all(|e| !e.contains("합성-결과-77c1")));
    let new = client::chat(&rig.socket, &chat("a2", "hi"), |_| {})
        .await
        .unwrap();
    assert_eq!(failed(&new), "unavailable");
    assert_eq!(rig.store.accepted().len(), 1, "구판은 저장소에 쓰지 않는다");

    // 같은 집합으로 돌아오면 이어진다
    active.set(Some("set-1"));
    let back = client::result(&rig.socket, None, "a1", true, |_| {})
        .await
        .unwrap();
    assert!(matches!(back, Outcome::Done { .. }), "{back:?}");
}

#[tokio::test]
async fn cli_chat_uses_the_same_routes_snapshot_for_auth_and_admit() {
    let rig = rig().await;
    // 인증 때 첫 번째 표 조회 직후 활성 프로필 판본이 r2로 바뀐다
    rig.catalog.flip_after(
        1,
        [desk("personal", "owner").revision("personal-r2").build()],
    );
    let start = Chat {
        wait: false,
        ..chat("snap-1", "같은 표로 접수")
    };
    assert_eq!(
        client::chat(&rig.socket, &start, |_| {}).await.unwrap(),
        Outcome::Accepted
    );
    let accepted = rig.store.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        accepted[0].revision, "personal-r1",
        "인증에 쓴 바로 그 라우팅 표의 판본으로 접수돼야 해요"
    );
}

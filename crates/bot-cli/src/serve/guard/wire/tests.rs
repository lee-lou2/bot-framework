//! 원격 MCP 요청마다 활성 판본을 다시 확인한다: 쓰기 승인 뒤 도구 목록을 기다리는 사이 목적지가 철회돼도 구판 워커는 옛 주소로 bearer와 `tools/call`을 보내지 않는다.
use super::*;
use bot_broker::{
    mcp::remote::Remote,
    ports::{Ran, ToolJob, ToolRunner},
    resolve::{Resolved, pinned_to_image},
    runner::{Deps, HostRegistry, McpRunner, RunnerConfig},
    testing::{
        CountingEntropy, Era, FakeConnector, FakeMcp, FakeResolver, FakeSandbox, FakeSecrets,
        FakeWire, McpTool, MemJournal, Server, tool,
    },
};
use bot_core::{
    id::{
        Attempt, CallId, ClaimEpoch, Generation, PrincipalId, ProfileId, Revision, RunId, ToolId,
    },
    state::{Risk, RunOrigin},
};
use bot_gateway::testing::ports::MemoryActive;
use serde_json::json;

fn remote() -> Remote {
    Remote::parse(
        "https://mcp.example/mcp",
        bot_core::id::SecretName::parse("reader"),
    )
    .unwrap()
}

fn runner(wire: Arc<dyn RemoteWire>) -> McpRunner {
    let remote = remote();
    McpRunner::new(
        Deps {
            sandbox: Arc::new(FakeSandbox::new(Server::new(&[]))),
            secrets: Arc::new(FakeSecrets::new(&[(
                "personal",
                "reader",
                "value-personal-1",
            )])),
            effects: Arc::new(MemJournal::new()),
            resolver: Arc::new(FakeResolver::new(&[])),
            connector: Arc::new(FakeConnector::new()),
            entropy: Arc::new(CountingEntropy::default()),
        },
        HostRegistry {
            servers: [("weather".to_owned(), remote.revision())].into(),
            remote: [("weather".to_owned(), remote)].into(),
            ..HostRegistry::default()
        },
        RunnerConfig::default(),
    )
    .with_wire(wire)
}

fn job(class: Risk) -> ToolJob {
    let mut declared = tool(class, &["calendar.delete"]);
    declared.server = "weather".into();
    declared.egress = ["mcp.example:443".to_owned()].into();
    declared.revision = Revision("t1".into());
    ToolJob {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        call: CallId("c1".into()),
        principal: PrincipalId("local:owner".into()),
        origin: RunOrigin::Http,
        tool: Resolved::new(
            ToolId("agenda.delete".into()),
            pinned_to_image(declared, &remote().revision()),
        )
        .unwrap(),
        args: "{}".into(),
        env: Vec::new(),
        reply_to: None,
        owner: "r1.1.c1".into(),
    }
}

fn server(era: Era) -> Arc<FakeMcp> {
    let mut fake = FakeMcp::new(
        era,
        vec![McpTool::new("delete", "x", json!({"type": "object"}))],
    );
    fake.token = Some("value-personal-1".into());
    Arc::new(fake)
}

#[tokio::test]
async fn a_set_that_went_stale_while_the_list_was_awaited_never_gets_the_call_or_the_bearer_again()
{
    for era in [Era::Modern, Era::Legacy] {
        let active = MemoryActive::new(Some("set-1"));
        let fake = server(era);
        let wire = Arc::new(FakeWire::new(fake.clone()));
        // 서버가 도구 목록을 내준 직후(= 승인된 쓰기가 `tools/call`을 보내기 직전) 다른 프로세스가 목적지를 철회한 새 집합을 게시한다
        let flip = active.clone();
        *wire.after_request.lock().unwrap() = Some(Box::new(move |rpc| {
            if rpc == "tools/list" {
                flip.set(Some("set-2"));
            }
        }));
        let guarded = Arc::new(GuardedWire::new(wire.clone(), active.check("set-1")));
        let ran = runner(guarded).run(job(Risk::Irreversible)).await.unwrap();
        assert!(matches!(ran, Ran::NotSent), "{era:?}: {ran:?}");
        assert_eq!(
            fake.count("tools/call"),
            0,
            "{era:?}: 철회된 목적지로 쓰기를 보냈어요"
        );
        // 목록 요청까지는 나갔고(그 직전 확인은 통과했다) 그 뒤로는 하나도 나가지 않았다: 구판의 세션 정리(`DELETE`)도 확인에 막힌다
        let seen: Vec<String> = fake.requests().iter().map(|r| r.rpc()).collect();
        assert_eq!(
            seen.last().map(String::as_str),
            Some("tools/list"),
            "{era:?}: {seen:?}"
        );
        assert_eq!(fake.count("DELETE"), 0, "{era:?}");
    }
}

#[tokio::test]
async fn every_request_is_checked_and_a_current_set_runs_normally() {
    let active = MemoryActive::new(Some("set-1"));
    let fake = server(Era::Legacy);
    let wire = Arc::new(FakeWire::new(fake.clone()));
    let guarded = Arc::new(GuardedWire::new(wire, active.check("set-1")));
    let ran = runner(guarded).run(job(Risk::Read)).await.unwrap();
    assert!(matches!(ran, Ran::Done(_)), "{ran:?}");
    assert_eq!(fake.count("tools/call"), 1);
}

#[tokio::test]
async fn an_unpublished_or_unreadable_set_sends_nothing_at_all() {
    for set in [None, Some("set-2")] {
        let active = MemoryActive::new(set);
        let fake = server(Era::Modern);
        let guarded = Arc::new(GuardedWire::new(
            Arc::new(FakeWire::new(fake.clone())),
            active.check("set-1"),
        ));
        let ran = runner(guarded).run(job(Risk::Irreversible)).await.unwrap();
        assert!(matches!(ran, Ran::NotSent), "{set:?}: {ran:?}");
        assert!(
            fake.requests().is_empty(),
            "{set:?}: 아무것도 나가면 안 돼요"
        );
    }
    // 확인이 끝나지 않으면 기다리다 보내지 않는다
    let active = MemoryActive::new(Some("set-1"));
    active.delay(Duration::from_secs(30));
    let fake = server(Era::Modern);
    let guarded = Arc::new(GuardedWire::with_grace(
        Arc::new(FakeWire::new(fake.clone())),
        active.check("set-1"),
        Duration::from_millis(100),
    ));
    let ran = runner(guarded).run(job(Risk::Irreversible)).await.unwrap();
    assert!(matches!(ran, Ran::NotSent), "{ran:?}");
    assert!(fake.requests().is_empty());
}

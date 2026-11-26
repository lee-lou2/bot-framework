//! 대화에서 만든 예약의 정상 경로 수락 시험: 실제 `claude` CLI(가짜 상류)·실제 PG·실제 브로커 프런트(PlannedCalls·CheckpointSink·호스트 Park)
//! 위에서 gateway·scheduler·worker 조립(`serve`)이 생성→목록→취소와 실제 발화를 끝까지 한다. 모델 호출은 0이다.
//! 모델 샌드박스는 시험에서만 합성(호스트 프로세스)으로 넣고, 운영 VM 격리 수락은 Apple 어댑터로 따로 본다.
//! 두 대화 종류를 나눠 본다: A) CLI 대화(`bot chat`, 게이트웨이 경로 전체. CLI 대화는 outbox가 아니라 상태 조회로 답하므로 «원래 대화 복원»을
//! run 기록으로 본다), B) direct 대화(답이 outbox로 간다). **B의 첫 접수는 Slack 입구(소켓·서명·버튼)를 거치지 않고 store.accept로 직접 한다** —
//! Slack 입구는 gateway 시험이 따로 본다. C) 위임으로 시도가 대기에 들어가면 실제 호스트 Park가 통지를 받고 부모가 정확히 한 번 재개된다.
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_schedule` (claude CLI가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_broker::front::park::ParkReason;
use bot_cli::cli::Role;
use bot_core::{
    command::{Accept, Envelope, JsonPayload, Target},
    id::{AgentId, BindingId, PrincipalId, ProfileId, RequestId, Revision},
    port::RunStore,
    state::{Conversation, ConversationKind},
};
use bot_store::{Config, Store};
use claude_support::*;
use process_support::Schema;

fn profile(member: &str, binding: &str) -> String {
    profile_with(member, binding, "")
}

/// `agents`는 `policy.agents` 아래에 더할 줄들(자식 에이전트의 범위)
fn profile_with(member: &str, binding: &str, agents: &str) -> String {
    let agents = if agents.is_empty() {
        String::new()
    } else {
        format!("  agents:\n{agents}\n")
    };
    format!(
        r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: {SCOPES}
members:
  - {{identity: "{member}", roles: [owner]}}
bindings:
  - {binding}
secrets: []
policy:
  scopes: {SCOPES}
{agents}  approve: []
"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cli_conversation_creates_lists_cancels_and_gets_the_fired_run_back_in_the_same_conversation()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let profile = profile(
        "local:owner",
        r#"{id: desk, adapter: cli, account: owner, agent: main}"#,
    );
    let mut rig = Rig::start(
        schema,
        claude,
        &profile,
        &[],
        &cli_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
    )
    .await;

    // 1) 생성: 대화(`bot chat`) → 모델이 `schedule.create`를 부른다 → 예약이 원래 대화로 저장된다
    let far = in_secs(3600);
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &format!(r#"{{"at":"{far}","instruction":"자료 도착 확인","quiet":false}}"#),
    ));
    rig.script(final_turn("msg_c2", "예약했어요"));
    let created = rig.chat("c1", "한 시간 뒤에 자료 도착을 확인해 줘").await;
    assert!(created.status.success(), "{}", said(&created));
    assert!(said(&created).contains("예약했어요"), "{}", said(&created));
    let id = id_in(&tool_result(&rig.upstream, "toolu_c1").expect("생성 결과"));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'"),
        "1"
    );
    // 신원·원래 대화는 만든 run에서 복원된다(모델 인자에는 없었다)
    let origin = rig.query(
        "select r.binding || '|' || r.conv_kind || '|' || r.conv_id || '|' || r.principal from {s}.bot_runs r \
         where r.id = (select origin_run from {s}.bot_user_schedules)",
    );
    let stored = rig.query(
        "select origin_binding || '|' || origin_kind || '|' || origin_id || '|' || owner from {s}.bot_user_schedules",
    );
    assert_eq!(origin, stored);
    assert!(
        stored.starts_with("desk|cli|") && stored.ends_with("|local:owner"),
        "{stored}"
    );

    // 2) 목록: 본인 활성 예약의 ID가 모델에게 돌아온다
    rig.script(tool_turn(
        "msg_l1",
        "toolu_l1",
        "mcp__relay__schedule_list",
        "{}",
    ));
    rig.script(final_turn("msg_l2", "예약이 하나 있어요"));
    let listed = rig.chat("c2", "예약 목록 보여 줘").await;
    assert!(listed.status.success(), "{}", said(&listed));
    let list = tool_result(&rig.upstream, "toolu_l1").expect("목록 결과");
    assert!(list.contains(&id), "목록에 {id}가 없어요: {list}");

    // 3) 취소: 소유자가 자기 예약을 취소하고 더는 활성이 아니다
    rig.script(tool_turn(
        "msg_x1",
        "toolu_x1",
        "mcp__relay__schedule_cancel",
        &format!(r#"{{"id":"{id}"}}"#),
    ));
    rig.script(final_turn("msg_x2", "취소했어요"));
    let canceled = rig.chat("c3", "그 예약 취소해 줘").await;
    assert!(canceled.status.success(), "{}", said(&canceled));
    assert_eq!(
        rig.query(&format!(
            "select state from {{s}}.bot_user_schedules where id = '{id}'"
        )),
        "canceled"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'"),
        "0"
    );

    // 4) 실제 발화: 곧 올 시각의 예약을 만들면 scheduler가 그 시각에 run을 만들고 worker가 모델 CLI로 끝낸다
    let soon = in_secs(15);
    rig.script(tool_turn(
        "msg_n1",
        "toolu_n1",
        "mcp__relay__schedule_create",
        &format!(r#"{{"at":"{soon}","instruction":"자료 도착 확인","quiet":false}}"#),
    ));
    rig.script(final_turn("msg_n2", "한 번 예약했어요"));
    // 발화한 run의 모델 응답(만드는 대화가 끝난 뒤 첫 요청에 쓰인다)
    rig.script(final_turn("msg_f1", "자료가 도착했어요"));
    let again = rig.chat("c4", "잠깐 뒤에 자료 도착을 확인해 줘").await;
    assert!(again.status.success(), "{}", said(&again));
    let id = id_in(&tool_result(&rig.upstream, "toolu_n1").expect("두 번째 생성 결과"));
    let fired_state = format!(
        "select r.state from {{s}}.bot_runs r join {{s}}.bot_user_occurrences o \
         on o.run = r.id and o.profile = r.profile where o.schedule = '{id}'"
    );
    rig.until("예약이 발화해 끝나지 않았어요", 90, |rig| {
        rig.query(&fired_state) == "succeeded"
    })
    .await;
    // 발화한 run은 원래 대화(바인딩·대화 종류·대화 ID)와 소유자로 복원되고, 저장된 지시문으로 돌았다
    let fired = rig.query(&format!(
        "select run from {{s}}.bot_user_occurrences where schedule = '{id}'"
    ));
    let at_fire = rig.query(&format!(
        "select binding || '|' || conv_kind || '|' || conv_id || '|' || principal from {{s}}.bot_runs where id = '{fired}'"
    ));
    assert_eq!(
        at_fire, stored,
        "발화한 run이 원래 대화로 돌아가지 않았어요"
    );
    assert!(
        rig.query(&format!(
            "select result from {{s}}.bot_runs where id = '{fired}'"
        ))
        .contains("자료가 도착했어요")
    );
    let last = bodies(&rig.upstream).pop().unwrap().to_string();
    assert!(
        last.contains("자료 도착 확인"),
        "저장된 지시문으로 돌았어야 해요"
    );
    // 한 번짜리 예약은 발화 뒤 닫힌다
    assert_eq!(
        rig.query(&format!(
            "select state from {{s}}.bot_user_schedules where id = '{id}'"
        )),
        "closed"
    );
    // 모델 요청은 정확히 대화 넷(2+2+2+2)과 발화 하나뿐이다(숨은 재전송 없음)
    assert_eq!(rig.upstream.requests().len(), 9);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_conversation_gets_the_fired_reply_in_its_own_outbox_and_a_quiet_one_gets_none() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    // Slack DM과 같은 접수 모양을 store.accept로 직접 접수한다(실제 Slack 연결이 없다). 그 뒤의 worker·모델 CLI·브로커·scheduler·outbox는
    // 실제 경로다. 게이트웨이 역할은 띄우지 않는다.
    let profile = profile(
        "slack:home/U1",
        r#"{id: dm, adapter: slack, workspace: home, dm: true, agent: main}"#,
    );
    let gateway = r#""slack": {"workspaces": {"home": {"team_id": "T1", "bot_token": "bot-token", "app_token": "app-token", "dm": true}}}"#;
    let mut rig = Rig::start(
        schema,
        claude,
        &profile,
        &[],
        gateway,
        &[Role::Scheduler, Role::Worker],
    )
    .await;
    let store = Store::connect(&rig.schema.url(), Config::default())
        .await
        .expect("저장소");
    let revision = Revision(
        rig.query("select revision from {s}.bot_active_profiles where profile = 'personal'"),
    );
    let accept = |request: &str, text: &str| Accept {
        reply_transport: Some(bot_core::intake::PushTransportId::SLACK),
        event: Envelope {
            profile: ProfileId("personal".into()),
            request: RequestId(request.into()),
            binding: BindingId("dm".into()),
            principal: PrincipalId("slack:home/U1".into()),
            conversation: Conversation {
                kind: ConversationKind::Direct,
                id: "D1".into(),
                thread: None,
            },
            http: None,
            attachments: vec![],
            body: JsonPayload(serde_json::json!({ "text": text }).to_string()),
        },
        approver: Some(PrincipalId("slack:home/U1".into())),
        revision: revision.clone(),
        target: Target::Agent {
            agent: AgentId("main".into()),
            instruction: text.into(),
        },
        scopes: ["run", "schedule.create", "schedule.list", "schedule.cancel"]
            .map(String::from)
            .into_iter()
            .collect(),
    };
    let finished = |run: &str| format!("select state from {{s}}.bot_runs where id = '{run}'");

    // 예약 둘을 만든다(하나는 일반, 하나는 quiet). 먼 시각이라 아직 발화하지 않는다
    let far = in_secs(3600);
    let mut ids = Vec::new();
    for (n, (quiet, said_text)) in [(false, "예약했어요"), (true, "조용히 예약했어요")]
        .into_iter()
        .enumerate()
    {
        let call = format!("toolu_d{n}");
        rig.script(tool_turn(
            &format!("msg_d{n}a"),
            &call,
            "mcp__relay__schedule_create",
            &format!(r#"{{"at":"{far}","instruction":"자료 도착 확인","quiet":{quiet}}}"#),
        ));
        rig.script(final_turn(&format!("msg_d{n}b"), said_text));
        let run = store
            .accept(accept(&format!("r{n}"), "자료 도착을 확인해 줘"))
            .await
            .expect("접수")
            .0;
        rig.until(
            "예약을 만드는 run이 끝나지 않았어요",
            90,
            |rig| rig.query(&finished(&run)) == "succeeded",
        )
        .await;
        ids.push(id_in(
            &tool_result(&rig.upstream, &call).expect("생성 결과"),
        ));
        // 만든 run의 답은 그 대화의 outbox에 쌓인다(게이트웨이가 없으니 아직 보내지 않은 pending)
        assert_eq!(
            rig.query(&format!(
                "select binding || '|' || conv_kind || '|' || conv_id || '|' || state from {{s}}.bot_outbox where run = '{run}'"
            )),
            "dm|direct|D1|pending"
        );
    }
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules where origin_kind = 'direct' and origin_id = 'D1' and state = 'active'"),
        "2"
    );

    // 발화: 일반 예약 → 원래 대화의 outbox에 답이 쌓이고, quiet 예약 → `<조용히>`면 답이 없다. 시각은 DB 시계를 앞당겨 기다림 없이 본다
    for (id, quiet, reply) in [
        (&ids[0], false, "자료가 도착했어요"),
        (&ids[1], true, "<조용히>"),
    ] {
        rig.script(final_turn(&format!("msg_f{quiet}"), reply));
        assert_eq!(
            rig.query(&format!(
                "update {{s}}.bot_user_schedules set at_ms = created_ms + 1000, next_ms = created_ms + 1000 where id = '{id}'"
            )),
            "UPDATE 1"
        );
        let fired_state = format!(
            "select r.state from {{s}}.bot_runs r join {{s}}.bot_user_occurrences o \
             on o.run = r.id and o.profile = r.profile where o.schedule = '{id}'"
        );
        rig.until("예약이 발화해 끝나지 않았어요", 90, |rig| {
            rig.query(&fired_state) == "succeeded"
        })
        .await;
        let fired = rig.query(&format!(
            "select run from {{s}}.bot_user_occurrences where schedule = '{id}'"
        ));
        // 발화한 run은 원래 대화·소유자로 복원된다
        assert_eq!(
            rig.query(&format!(
                "select binding || '|' || conv_kind || '|' || conv_id || '|' || principal from {{s}}.bot_runs where id = '{fired}'"
            )),
            "dm|direct|D1|slack:home/U1"
        );
        let outbox = rig.query(&format!(
            "select binding || '|' || conv_kind || '|' || conv_id || '|' || state from {{s}}.bot_outbox where run = '{fired}'"
        ));
        if quiet {
            assert_eq!(outbox, "", "quiet 예약은 보낼 답이 없어요");
            assert!(
                rig.query(&format!(
                    "select result from {{s}}.bot_runs where id = '{fired}'"
                ))
                .contains("<조용히>")
            );
        } else {
            assert_eq!(outbox, "dm|direct|D1|pending");
            assert!(
                rig.query(&format!(
                    "select body from {{s}}.bot_outbox where run = '{fired}'"
                ))
                .contains("자료가 도착했어요")
            );
        }
    }
    // 모델 요청은 생성 둘(2+2)과 발화 둘뿐이다
    assert_eq!(rig.upstream.requests().len(), 6);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegation_parks_the_attempt_through_the_real_host_park_and_the_parent_resumes_once() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let profile = profile_with(
        "local:owner",
        r#"{id: desk, adapter: cli, account: owner, agent: main}"#,
        "    main/research: [run]",
    );
    let rig = Rig::start(
        schema,
        claude,
        &profile,
        &[("agents/main/agents/research/AGENT.md", "조사해서 요약한다.")],
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
    )
    .await;
    // 부모가 직계 자식에게 일을 맡긴다 → 부모 시도는 자식을 기다리며 멈추고(Park), 자식 run이 모델 응답으로 끝나고, 부모가 이어서 마무리한다
    rig.script(tool_turn(
        "msg_p1",
        "toolu_p1",
        "mcp__relay__delegate",
        r#"{"agent":"main/research","instruction":"일정을 요약해 줘"}"#,
    ));
    rig.script(final_turn("msg_p2", "자식 요약이에요"));
    rig.script(final_turn("msg_p3", "위임을 마쳤어요"));
    let out = rig
        .chat("p1", "조사 에이전트에게 일정 요약을 맡겨 줘")
        .await;
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("위임을 마쳤어요"), "{}", said(&out));
    // 호스트 Park는 정확히 부모 시도 하나가 자식 대기로 들어갔다는 통지 한 건만 받았다
    let parent = rig.query("select id from {s}.bot_runs where parent is null");
    let parked = rig.park.recent();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0].1, ParkReason::Child);
    assert_eq!(parked[0].0.run.0, parent);
    assert_eq!(rig.park.counts(), (0, 0, 1));
    // 자식은 정확히 한 번 돌았고 부모는 같은 run으로 끝났다
    assert_eq!(rig.query("select count(*) from {s}.bot_runs"), "2");
    assert_eq!(
        rig.query(
            "select count(*) from {s}.bot_runs where parent is not null and state = 'succeeded'"
        ),
        "1"
    );
    assert_eq!(
        rig.query(&format!(
            "select state from {{s}}.bot_runs where id = '{parent}'"
        )),
        "succeeded"
    );
    // 모델 요청은 부모 첫 요청·자식·재개한 부모 셋뿐이고, 재개한 부모는 자식 결과를 도구 결과로 받았다
    assert_eq!(rig.upstream.requests().len(), 3);
    let delegated = tool_result(&rig.upstream, "toolu_p1").expect("위임 결과");
    assert!(delegated.contains("자식 요약이에요"), "{delegated}");
    rig.stop().await;
}

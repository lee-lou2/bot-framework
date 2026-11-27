//! End-to-end acceptance tests for chat schedules and delegation using real `claude` CLI (fake upstream), real PG, and real broker front (`PlannedCalls`, `CheckpointSink`, `HostPark`).
//! Run: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-cli --test claude_schedule`.
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

/// `agents` appends child-agent scope lines under `policy.agents`.
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
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
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

    // 1) Create: `bot chat` -> `schedule.create` stores schedule bound to the origin conversation.
    let far = in_secs(3600);
    rig.script(tool_turn(
        "msg_c1",
        "toolu_c1",
        "mcp__relay__schedule_create",
        &format!(
            r#"{{"at":"{far}","instruction":"Check whether the materials arrived","quiet":false}}"#
        ),
    ));
    rig.script(final_turn("msg_c2", "Scheduled."));
    let created = rig
        .chat("c1", "Check in one hour whether the materials arrived")
        .await;
    assert!(created.status.success(), "{}", said(&created));
    assert!(said(&created).contains("Scheduled."), "{}", said(&created));
    let id = id_in(&tool_result(&rig.upstream, "toolu_c1").expect("create result"));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_user_schedules where state = 'active'"),
        "1"
    );
    // Principal and origin conversation are derived from the creating run, not model args.
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

    // 2) List: owner's active schedule ID is returned to the model.
    rig.script(tool_turn(
        "msg_l1",
        "toolu_l1",
        "mcp__relay__schedule_list",
        "{}",
    ));
    rig.script(final_turn("msg_l2", "There is one schedule."));
    let listed = rig.chat("c2", "Show my schedules").await;
    assert!(listed.status.success(), "{}", said(&listed));
    let list = tool_result(&rig.upstream, "toolu_l1").expect("list result");
    assert!(list.contains(&id), "list missing {id}: {list}");

    // 3) Cancel: owner cancels own schedule.
    rig.script(tool_turn(
        "msg_x1",
        "toolu_x1",
        "mcp__relay__schedule_cancel",
        &format!(r#"{{"id":"{id}"}}"#),
    ));
    rig.script(final_turn("msg_x2", "Canceled."));
    let canceled = rig.chat("c3", "Cancel that schedule").await;
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

    // 4) Fire: near-term schedule fires via scheduler and completes via worker.
    let soon = in_secs(15);
    rig.script(tool_turn(
        "msg_n1",
        "toolu_n1",
        "mcp__relay__schedule_create",
        &format!(
            r#"{{"at":"{soon}","instruction":"Check whether the materials arrived","quiet":false}}"#
        ),
    ));
    rig.script(final_turn("msg_n2", "Scheduled once."));
    rig.script(final_turn("msg_f1", "The materials have arrived."));
    let again = rig
        .chat("c4", "Check shortly whether the materials arrived")
        .await;
    assert!(again.status.success(), "{}", said(&again));
    let id = id_in(&tool_result(&rig.upstream, "toolu_n1").expect("second create result"));
    let fired_state = format!(
        "select r.state from {{s}}.bot_runs r join {{s}}.bot_user_occurrences o \
         on o.run = r.id and o.profile = r.profile where o.schedule = '{id}'"
    );
    rig.until("scheduled run did not fire and finish", 90, |rig| {
        rig.query(&fired_state) == "succeeded"
    })
    .await;
    let fired = rig.query(&format!(
        "select run from {{s}}.bot_user_occurrences where schedule = '{id}'"
    ));
    let at_fire = rig.query(&format!(
        "select binding || '|' || conv_kind || '|' || conv_id || '|' || principal from {{s}}.bot_runs where id = '{fired}'"
    ));
    assert_eq!(
        at_fire, stored,
        "fired run did not restore origin conversation"
    );
    assert!(
        rig.query(&format!(
            "select result from {{s}}.bot_runs where id = '{fired}'"
        ))
        .contains("The materials have arrived.")
    );
    let last = bodies(&rig.upstream).pop().unwrap().to_string();
    assert!(
        last.contains("Check whether the materials arrived"),
        "fired run must use stored instruction"
    );
    assert_eq!(
        rig.query(&format!(
            "select state from {{s}}.bot_user_schedules where id = '{id}'"
        )),
        "closed"
    );
    // Exactly 4 chats (2 turns each) + 1 fired turn; no hidden retries.
    assert_eq!(rig.upstream.requests().len(), 9);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_conversation_gets_the_fired_reply_in_its_own_outbox_and_a_quiet_one_gets_none() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    // Accepts directly via `store.accept` (bypassing Slack gateway) to test scheduler + worker + outbox.
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
        .expect("store");
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

    // Create two far-future schedules: one normal, one quiet.
    let far = in_secs(3600);
    let mut ids = Vec::new();
    for (n, (quiet, said_text)) in [(false, "Scheduled."), (true, "Scheduled quietly.")]
        .into_iter()
        .enumerate()
    {
        let call = format!("toolu_d{n}");
        rig.script(tool_turn(
            &format!("msg_d{n}a"),
            &call,
            "mcp__relay__schedule_create",
            &format!(
                r#"{{"at":"{far}","instruction":"Check whether the materials arrived","quiet":{quiet}}}"#
            ),
        ));
        rig.script(final_turn(&format!("msg_d{n}b"), said_text));
        let run = store
            .accept(accept(
                &format!("r{n}"),
                "Check whether the materials arrived",
            ))
            .await
            .expect("accept")
            .0;
        rig.until("schedule-creating run did not finish", 90, |rig| {
            rig.query(&finished(&run)) == "succeeded"
        })
        .await;
        ids.push(id_in(
            &tool_result(&rig.upstream, &call).expect("create result"),
        ));
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

    // Backdate timestamps to trigger immediately: normal enqueues outbox reply; quiet `<quiet>` suppresses it.
    for (id, quiet, reply) in [
        (&ids[0], false, "The materials have arrived."),
        (&ids[1], true, "<quiet>"),
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
        rig.until("scheduled run did not fire and finish", 90, |rig| {
            rig.query(&fired_state) == "succeeded"
        })
        .await;
        let fired = rig.query(&format!(
            "select run from {{s}}.bot_user_occurrences where schedule = '{id}'"
        ));
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
            assert_eq!(outbox, "", "quiet schedule must not enqueue a reply");
            assert!(
                rig.query(&format!(
                    "select result from {{s}}.bot_runs where id = '{fired}'"
                ))
                .contains("<quiet>")
            );
        } else {
            assert_eq!(outbox, "dm|direct|D1|pending");
            assert!(
                rig.query(&format!(
                    "select body from {{s}}.bot_outbox where run = '{fired}'"
                ))
                .contains("The materials have arrived.")
            );
        }
    }
    assert_eq!(rig.upstream.requests().len(), 6);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegation_parks_the_attempt_through_the_real_host_park_and_the_parent_resumes_once() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
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
        &[(
            "agents/main/agents/research/AGENT.md",
            "Research and summarize.",
        )],
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
    )
    .await;
    rig.script(tool_turn(
        "msg_p1",
        "toolu_p1",
        "mcp__relay__delegate",
        r#"{"agent":"main/research","instruction":"Summarize the agenda"}"#,
    ));
    rig.script(final_turn("msg_p2", "Child summary."));
    rig.script(final_turn("msg_p3", "Delegation completed."));
    let out = rig
        .chat("p1", "Ask the research agent to summarize the agenda")
        .await;
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("Delegation completed."),
        "{}",
        said(&out)
    );
    let parent = rig.query("select id from {s}.bot_runs where parent is null");
    let parked = rig.park.recent();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0].1, ParkReason::Child);
    assert_eq!(parked[0].0.run.0, parent);
    assert_eq!(rig.park.counts(), (0, 0, 1));
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
    assert_eq!(rig.upstream.requests().len(), 3);
    let delegated = tool_result(&rig.upstream, "toolu_p1").expect("delegation result");
    assert!(delegated.contains("Child summary."), "{delegated}");
    rig.stop().await;
}

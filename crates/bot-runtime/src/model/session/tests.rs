//! Session journal and restoration protocol tests using a captured Claude Code 2.1.331 fixture.

use super::*;
use bot_core::{
    cli::{CliGroup, CliResponse, CliResponseUse, CliUse},
    command::JsonPayload,
};
use serde_json::json;
use std::collections::BTreeMap;

const CHAIN: &str = include_str!("fixture.jsonl");
const RELAY: &str = "mcp__relay__";

fn lines() -> Vec<&'static str> {
    CHAIN.lines().collect()
}

fn journal(next: u64) -> Journal {
    Journal::new(next, BTreeMap::new(), RELAY.into())
}

fn fed(n: usize) -> Journal {
    let mut j = journal(0);
    for line in &lines()[..n] {
        assert_eq!(j.feed(line), Fed::Accepted);
    }
    j
}

fn probe_response() -> CliResponse {
    CliResponse {
        id: "msg_1".into(),
        uses: vec![CliResponseUse {
            id: "toolu_probe1".into(),
            name: "mcp__relay__nothing".into(),
            input: json!({"x": 1}),
        }],
    }
}

fn keys(plan: &Plan) -> Vec<&str> {
    plan.steps.iter().map(|s| s.key.as_str()).collect()
}

fn assistant(uuid: &str, parent: &str, message: &str, tool: &str, name: &str) -> String {
    json!({"type":"assistant","uuid":uuid,"parentUuid":parent,
        "message":{"id":message,"role":"assistant","content":[{"type":"tool_use","id":tool,"name":name,"input":{}}]}})
    .to_string()
}

fn result(uuid: &str, parent: &str, tool: &str) -> String {
    json!({"type":"user","uuid":uuid,"parentUuid":parent,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":tool,"content":"ok"}]}})
    .to_string()
}

fn response(id: &str, tool: &str, name: &str) -> CliResponse {
    CliResponse {
        id: id.into(),
        uses: vec![CliResponseUse {
            id: tool.into(),
            name: name.into(),
            input: json!({}),
        }],
    }
}

#[test]
fn the_session_id_and_project_dir_are_deterministic() {
    let (p, r) = (ProfileId("personal".into()), RunId("run-1".into()));
    let id = session_id(&p, &r);
    assert_eq!(id, session_id(&p, &r));
    assert_ne!(id, session_id(&p, &RunId("run-2".into())));
    assert_eq!(id.len(), 36);
    assert_eq!(id.as_bytes()[14], b'5', "UUID version nibble");
    assert!(
        matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
        "UUID variant nibble"
    );
    assert_eq!(project_dir("/bot"), "-bot");
    assert_eq!(
        project_dir("/private/tmp/a.b/run1"),
        "-private-tmp-a-b-run1"
    );
}

#[test]
fn a_batch_is_made_only_when_the_front_confirmed_exactly_the_tools_in_the_session_lines() {
    let j = fed(4);
    assert_eq!(j.oldest(), Some("msg_1"));
    let plan = j.plan(&probe_response()).unwrap();
    assert_eq!(
        keys(&plan),
        [
            "cli-rec-0",
            "cli-rec-1",
            "cli-rec-2",
            "cli-rec-3",
            "cli-use-toolu_probe1",
            "cli-group-3"
        ],
        "records -> uses -> group (last), anchored at first assistant line"
    );
    assert!(plan.verify.is_empty());
    let used: CliUse = serde_json::from_str(&plan.steps[4].body.0).unwrap();
    assert_eq!(
        (
            used.seq,
            used.turn,
            used.pos,
            used.name.as_str(),
            &used.input
        ),
        (3, 3, 0, "mcp__relay__nothing", &json!({"x": 1}))
    );
    let group: CliGroup = serde_json::from_str(&plan.steps[5].body.0).unwrap();
    assert_eq!(
        (group.seq, group.uses),
        (3, vec!["toolu_probe1".to_owned()])
    );

    let mut other = probe_response();
    other.uses[0].id = "toolu_other".into();
    assert_eq!(j.plan(&other).unwrap_err(), Refused::Mismatch);
    let mut other = probe_response();
    other.uses[0].name = "mcp__relay__else".into();
    assert_eq!(j.plan(&other).unwrap_err(), Refused::Mismatch);
    let mut other = probe_response();
    other.uses[0].input = json!({"x": 2});
    assert_eq!(j.plan(&other).unwrap_err(), Refused::Mismatch);
    let mut longer = probe_response();
    longer.uses.push(CliResponseUse {
        id: "toolu_second".into(),
        name: "mcp__relay__nothing".into(),
        input: json!({}),
    });
    assert_eq!(
        j.plan(&longer).unwrap_err(),
        Refused::Incomplete,
        "must wait for remaining session lines when front saw more tools"
    );
    let mut unknown = probe_response();
    unknown.id = "msg_unknown".into();
    assert_eq!(j.plan(&unknown).unwrap_err(), Refused::Mismatch);
    let mut two = fed(4);
    two.feed(&assistant(
        "u9",
        "cf3bcfae-3ae9-4dbb-94dd-7fb7c1abaf9f",
        "msg_1",
        "toolu_more",
        "mcp__relay__x",
    ));
    assert_eq!(two.plan(&probe_response()).unwrap_err(), Refused::Mismatch);
}

#[test]
fn a_response_split_over_lines_keeps_its_first_line_as_the_turn_and_one_slot_per_tool() {
    let mut j = journal(10);
    let text = json!({"type":"assistant","uuid":"a0","parentUuid":"p","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"first"}]}});
    j.feed(&text.to_string());
    j.feed(&assistant("a1", "a0", "m1", "t1", "mcp__relay__one"));
    j.feed(&assistant("a2", "a1", "m1", "t2", "mcp__relay__two"));
    let mut resp = response("m1", "t1", "mcp__relay__one");
    resp.uses.push(CliResponseUse {
        id: "t2".into(),
        name: "mcp__relay__two".into(),
        input: json!({}),
    });
    let plan = j.plan(&resp).unwrap();
    assert_eq!(
        keys(&plan),
        [
            "cli-rec-10",
            "cli-rec-11",
            "cli-rec-12",
            "cli-use-t1",
            "cli-use-t2",
            "cli-group-10"
        ]
    );
    let t2: CliUse = serde_json::from_str(&plan.steps[4].body.0).unwrap();
    assert_eq!(
        (t2.seq, t2.turn, t2.pos),
        (12, 10, 1),
        "line 12, response anchored at turn 10, second tool"
    );
}

#[test]
fn a_batch_ends_before_the_next_response_and_the_numbering_continues() {
    let mut j = journal(0);
    j.feed(
        &json!({"type":"user","uuid":"u0","message":{"role":"user","content":"hi"}}).to_string(),
    );
    j.feed(&assistant("a1", "u0", "mA", "tA", "mcp__relay__one"));
    j.feed(&result("r1", "a1", "tA"));
    j.feed(&assistant("a2", "r1", "mB", "tB", "mcp__relay__two"));
    assert_eq!(j.oldest(), Some("mA"));
    assert_eq!(
        j.plan(&response("mB", "tB", "mcp__relay__two"))
            .unwrap_err(),
        Refused::NotYet
    );
    let first = j.plan(&response("mA", "tA", "mcp__relay__one")).unwrap();
    assert_eq!(
        keys(&first),
        [
            "cli-rec-0",
            "cli-rec-1",
            "cli-rec-2",
            "cli-use-tA",
            "cli-group-1"
        ],
        "stops before the first line of the next response (seq 3)"
    );
    assert_eq!(
        first
            .verify
            .iter()
            .map(|p| (p.turn, p.pos, p.tool_use.id.as_str()))
            .collect::<Vec<_>>(),
        [(1, 0, "tA")],
        "relay tool results in the batch must be verified against the ledger"
    );
    j.applied(&first);
    assert_eq!(j.oldest(), Some("mB"));
    let second = j.plan(&response("mB", "tB", "mcp__relay__two")).unwrap();
    assert_eq!(keys(&second), ["cli-rec-3", "cli-use-tB", "cli-group-3"]);
    assert!(second.verify.is_empty());
}

#[test]
fn only_relay_results_are_checked_against_the_ledger_and_unplaceable_ones_are_refused() {
    let mut j = journal(0);
    j.feed(&assistant("a1", "u0", "mA", "tRead", "Read"));
    j.feed(&result("r1", "a1", "tRead"));
    j.feed(&assistant("a2", "r1", "mB", "tB", "mcp__relay__two"));
    let plan = j.plan(&response("mA", "tRead", "Read")).unwrap();
    assert!(
        plan.verify.is_empty(),
        "VM built-in results are not in the ledger"
    );
    let mut j = journal(0);
    j.feed(&result("r0", "p", "tGhost"));
    j.feed(&assistant("a1", "r0", "mA", "tA", "mcp__relay__one"));
    assert_eq!(
        j.plan(&response("mA", "tA", "mcp__relay__one"))
            .unwrap_err(),
        Refused::Unverified
    );
}

#[test]
fn noise_and_oversized_lines_do_not_reach_the_batch() {
    let mut j = journal(1);
    for noise in [
        r#"{"type":"queue-operation","sessionId":"s"}"#,
        r#"{"type":"cost-state","sessionId":"s"}"#,
        "not json",
    ] {
        assert_eq!(j.feed(noise), Fed::Skipped);
    }
    let big = json!({"type":"user","uuid":"u","message":{"role":"user","content":"x".repeat(MAX_LINE_BYTES)}}).to_string();
    assert_eq!(j.feed(&big), Fed::Frozen);
    assert!(j.frozen());
    assert_eq!(
        j.feed(lines()[0]),
        Fed::Skipped,
        "frozen journal ignores subsequent lines"
    );
    assert_eq!(j.oldest(), None);
}

#[test]
fn a_frozen_journal_is_stuck_only_when_a_relay_call_could_not_be_saved() {
    let tool_line = |id: &str, name: &str| {
        json!({"type":"assistant","uuid":format!("u-{id}"),"message":{"id":format!("m-{id}"),"role":"assistant",
            "content":[{"type":"tool_use","id":id,"name":name,"input":{}}]}})
        .to_string()
    };
    let mut j = journal(0);
    j.feed(&tool_line("t1", "mcp__relay__nothing"));
    assert!(j.unsaved_relay());
    j.freeze(Why::Unsaved);
    assert_eq!(j.stuck(), Some(Why::Unsaved));
    let mut j = journal(0);
    j.feed(&tool_line("t2", "Read"));
    assert!(!j.unsaved_relay());
    j.freeze(Why::Unsaved);
    assert_eq!(j.stuck(), None);
    j.feed(&tool_line("t3", "Bash"));
    assert_eq!(j.stuck(), None);
    j.feed(r#"{"type":"user","uuid":"x","message":{"role":"user","content":"\"tool_use\""}}"#);
    assert_eq!(j.stuck(), None);
    j.feed(&tool_line("t4", "mcp__relay__nothing"));
    assert_eq!(j.stuck(), Some(Why::Unsaved));
    let mut j = journal(0);
    let huge =
        json!({"type":"assistant","uuid":"h","message":{"id":"m-h","role":"assistant","content":[
        {"type":"text","text":"x".repeat(MAX_LINE_BYTES)},
        {"type":"tool_use","id":"th","name":"mcp__relay__nothing","input":{}}]}})
        .to_string();
    assert_eq!(j.feed(&huge), Fed::Frozen);
    assert_eq!(j.stuck(), Some(Why::TooLarge));
}

fn saved_steps(n: usize) -> Vec<Step> {
    fed(n).plan(&probe_response()).unwrap().steps
}

#[test]
fn restore_rebuilds_the_chain_up_to_the_last_group_and_reports_the_pending_slots() {
    let steps = saved_steps(4);
    let saved = restore(&steps).unwrap().expect("group present");
    assert_eq!(
        saved.lines.iter().map(String::as_str).collect::<Vec<_>>(),
        lines()[..4]
    );
    assert_eq!(saved.next_seq(), 4);
    let pending = saved.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (
            pending[0].turn,
            pending[0].pos,
            pending[0].tool_use.id.as_str()
        ),
        (3, 0, "toolu_probe1")
    );
    assert_eq!(saved.last_uuid, "cf3bcfae-3ae9-4dbb-94dd-7fb7c1abaf9f");
    assert_eq!(saved.known["toolu_probe1"].name, "mcp__relay__nothing");

    let mut answered = steps.clone();
    answered.push(rec_step(4, lines()[4]));
    let saved = restore(&answered).unwrap().unwrap();
    assert_eq!(saved.lines.len(), 5);
    assert!(saved.pending().is_empty());
    let no_group: Vec<Step> = steps
        .iter()
        .filter(|s| !s.key.starts_with("cli-group-"))
        .cloned()
        .collect();
    assert_eq!(restore(&no_group), Ok(None));
    assert_eq!(restore(&[]), Ok(None));
}

#[test]
fn restore_rejects_gaps_missing_plans_and_plans_that_differ_from_the_lines() {
    let steps = saved_steps(4);
    let drop_key =
        |key: &str| -> Vec<Step> { steps.iter().filter(|s| s.key != key).cloned().collect() };
    assert_eq!(restore(&drop_key("cli-use-toolu_probe1")), Err(Corrupt));
    assert_eq!(
        restore(&drop_key("cli-rec-1")),
        Err(Corrupt),
        "sequence gap must fail"
    );
    let mut bad = steps.clone();
    bad[0].body = JsonPayload("not json".into());
    assert_eq!(restore(&bad), Err(Corrupt));
    let mut not_text = steps.clone();
    not_text[1].body = JsonPayload("{}".into());
    assert_eq!(restore(&not_text), Err(Corrupt));
    for patch in [
        json!({"seq":3,"turn":3,"pos":0,"name":"mcp__relay__other","input":{"x":1}}),
        json!({"seq":3,"turn":3,"pos":0,"name":"mcp__relay__nothing","input":{"x":2}}),
        json!({"seq":3,"turn":3,"pos":1,"name":"mcp__relay__nothing","input":{"x":1}}),
        json!({"seq":2,"turn":3,"pos":0,"name":"mcp__relay__nothing","input":{"x":1}}),
    ] {
        let mut changed = steps.clone();
        let at = changed
            .iter()
            .position(|s| s.key.starts_with("cli-use-"))
            .unwrap();
        changed[at].body = JsonPayload(patch.to_string());
        assert_eq!(restore(&changed), Err(Corrupt), "{patch}");
    }
    let mut renamed = steps.clone();
    let at = renamed
        .iter()
        .position(|s| s.key.starts_with("cli-group-"))
        .unwrap();
    renamed[at].key = "cli-group-2".into();
    assert_eq!(restore(&renamed), Err(Corrupt));
}

#[test]
fn an_older_response_with_an_unanswered_tool_is_corrupt() {
    let mut j = journal(0);
    j.feed(&assistant("a1", "u0", "mA", "tA", "mcp__relay__one"));
    j.feed(&assistant("a2", "a1", "mB", "tB", "mcp__relay__two"));
    let mut steps = j
        .plan(&response("mA", "tA", "mcp__relay__one"))
        .unwrap()
        .steps;
    j.applied(&j.plan(&response("mA", "tA", "mcp__relay__one")).unwrap());
    steps.extend(
        j.plan(&response("mB", "tB", "mcp__relay__two"))
            .unwrap()
            .steps,
    );
    assert_eq!(
        restore(&steps),
        Err(Corrupt),
        "next response without prior tool result must be rejected"
    );
}

#[test]
fn result_lines_chain_after_the_last_line_and_are_deterministic() {
    let saved = restore(&saved_steps(4)).unwrap().unwrap();
    let session = "ca6aa50b-17a1-438b-ac2d-3b9068a34c34";
    let outcomes = vec![(
        "toolu_probe1".to_owned(),
        ToolOutput::from_payload(r#"{"content":[{"type":"text","text":"done"}],"isError":false}"#),
    )];
    let a = result_line(&saved, session, &outcomes, "2026-11-24T05:00:00.000Z").unwrap();
    let b = result_line(&saved, session, &outcomes, "2026-11-24T05:00:09.000Z").unwrap();
    let (va, vb): (Value, Value) = (
        serde_json::from_str(&a).unwrap(),
        serde_json::from_str(&b).unwrap(),
    );
    assert_eq!(va["uuid"], vb["uuid"], "same inputs produce the same uuid");
    assert_eq!(va["parentUuid"], "cf3bcfae-3ae9-4dbb-94dd-7fb7c1abaf9f");
    assert_eq!(va["sessionId"], session);
    assert_eq!(va["cwd"], "/bot");
    assert_eq!(va["version"], "2.1.331");
    let block = &va["message"]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "toolu_probe1");
    assert_eq!(block["content"], json!([{"type":"text","text":"done"}]));
    assert_eq!(block["is_error"], false);
    let mut steps = saved_steps(4);
    steps.push(rec_step(4, &a));
    assert!(restore(&steps).unwrap().unwrap().pending().is_empty());
    let notice = vec![("toolu_probe1".to_owned(), ToolOutput::notice("denied"))];
    let line: Value =
        serde_json::from_str(&result_line(&saved, session, &notice, "t").unwrap()).unwrap();
    assert_eq!(line["message"]["content"][0]["is_error"], true);
    assert_eq!(
        line["message"]["content"][0]["content"][0]["text"],
        "denied"
    );
    assert_eq!(result_line(&saved, session, &[], "t"), None);
}

#[test]
fn too_many_unconfirmed_lines_freeze_the_journal() {
    let mut j = journal(0);
    for n in 0..4096 {
        let line =
            json!({"type":"user","uuid":format!("u{n}"),"message":{"role":"user","content":"x"}})
                .to_string();
        assert_eq!(j.feed(&line), Fed::Accepted, "{n}");
    }
    let line =
        json!({"type":"user","uuid":"over","message":{"role":"user","content":"x"}}).to_string();
    assert_eq!(j.feed(&line), Fed::Frozen);
    assert!(j.frozen());
    let mut j = journal(0);
    let chunk = "y".repeat(800 * 1024);
    let mut frozen_at = None;
    for n in 0..64 {
        let line =
            json!({"type":"user","uuid":format!("u{n}"),"message":{"role":"user","content":chunk}})
                .to_string();
        if j.feed(&line) == Fed::Frozen {
            frozen_at = Some(n);
            break;
        }
    }
    assert!(
        matches!(frozen_at, Some(n) if (15..=25).contains(&n)),
        "{frozen_at:?}"
    );
}

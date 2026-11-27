//! Isolation of holds sharing a call ID across kinds (tool vs reply) and across runs.

use crate::support::{
    holdfx::{hold as hold_at, step_of as step_in, views, waiting_run as waiting_in},
    *,
};
use bot_admin::{
    fake::{Fake, approval_view, call_view, digest, run_view},
    json::hex,
};
use bot_core::{
    admin::{CallState, HoldKind},
    id::{CallId, ProfileId, RunId},
    state::{EffectState, Risk, RunState},
};
use serde_json::Value;

fn pair() -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        waiting_in(d, "personal", "r-pair", "c-pair");
        d.holds.push(hold_at(
            HoldKind::Tool,
            "personal",
            "r-pair",
            "c-pair",
            Some("h-t"),
            vec![],
        ));
        d.holds.push(hold_at(
            HoldKind::Reply,
            "personal",
            "r-pair",
            "c-pair",
            Some("h-r"),
            vec![],
        ));
    });
    fake
}

fn stepped_pair() -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        waiting_in(d, "personal", "r-pair", "c-pair");
        let steps: Vec<_> = ["e-p1", "e-p2"]
            .iter()
            .map(|e| step_in(d, "personal", "r-pair", "c-pair", e, EffectState::Held))
            .collect();
        d.effects.extend(steps.clone());
        d.holds.push(hold_at(
            HoldKind::Tool,
            "personal",
            "r-pair",
            "c-pair",
            None,
            views(&steps),
        ));
        d.holds.push(hold_at(
            HoldKind::Reply,
            "personal",
            "r-pair",
            "c-pair",
            Some("h-r"),
            vec![],
        ));
    });
    fake
}

const PAIR_PATH: &str = "/v1/admin/holds/personal/r-pair/c-pair/resolve";

fn body(id: &str, effect: Option<&str>) -> String {
    let at = effect.map_or(String::new(), |e| format!(r#","effect":"{e}""#));
    format!(
        r#"{{"verdict":"not_executed","evidence":"no record at recipient","hold_id":"{id}"{at}}}"#
    )
}

async fn holds_of(s: &Server, call: &str) -> Vec<Value> {
    get(s, OPERATOR, "/v1/admin/holds").await.json()["items"]
        .as_array()
        .expect("list")
        .iter()
        .filter(|h| h["call"] == call)
        .cloned()
        .collect()
}

fn kinds(list: &[Value]) -> Vec<(&str, Option<&str>)> {
    let mut out: Vec<_> = list
        .iter()
        .map(|h| (h["kind"].as_str().unwrap_or(""), h["hold_id"].as_str()))
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_tool_hold_and_a_reply_hold_of_one_call_are_two_holds_with_their_own_ids() {
    let s = start(pair()).await;
    let list = holds_of(&s, "c-pair").await;
    assert_eq!(
        kinds(&list),
        [("reply", Some("h-r")), ("tool", Some("h-t"))],
        "expected two distinct holds"
    );
    let status = get(&s, OPERATOR, "/v1/admin/status").await.json();
    assert_eq!(
        status["profiles"]["personal"]["holds"], 3,
        "1 demo hold + 2 paired holds"
    );
}

#[tokio::test]
async fn judging_either_one_leaves_the_other_open_and_judgable() {
    for (first, second, left) in [("h-r", "h-t", "tool"), ("h-t", "h-r", "reply")] {
        let s = start(pair()).await;
        let r = post(&s, OPERATOR, PAIR_PATH, &body(first, None)).await;
        assert_eq!(r.status, 200, "{first}: {}", r.body);
        let list = holds_of(&s, "c-pair").await;
        assert_eq!(
            kinds(&list),
            [(left, Some(second))],
            "resolving {first} leaves {left} open"
        );
        assert_eq!(
            post(&s, OPERATOR, PAIR_PATH, &body(second, None))
                .await
                .status,
            200
        );
        assert!(holds_of(&s, "c-pair").await.is_empty());
        let resolved = s.fake.resolutions();
        let ids: Vec<&str> = resolved.iter().map(|r| r.hold.hold_id.0.as_str()).collect();
        assert_eq!(ids, [first, second], "each hold resolved once");
        let again = post(&s, OPERATOR, PAIR_PATH, &body(first, None)).await;
        assert_eq!(
            (again.status, again.json()["repeated"].as_bool()),
            (200, Some(true))
        );
        assert_eq!(s.fake.resolutions().len(), 2);
    }
}

#[tokio::test]
async fn a_stepless_reply_verdict_is_not_mistaken_for_a_stepless_verdict_on_the_stepped_tool_hold()
{
    let s = start(stepped_pair()).await;
    let list = holds_of(&s, "c-pair").await;
    assert_eq!(kinds(&list), [("reply", Some("h-r")), ("tool", None)]);
    let tool = list.iter().find(|h| h["kind"] == "tool").expect("tool");
    assert_eq!(
        tool["steps"].as_array().map(Vec::len),
        Some(2),
        "only tool holds carry steps"
    );
    let reply = list.iter().find(|h| h["kind"] == "reply").expect("reply");
    assert_eq!(reply["steps"].as_array().map(Vec::len), Some(0));
    let r = post(&s, OPERATOR, PAIR_PATH, &body("h-r", None)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let list = holds_of(&s, "c-pair").await;
    assert_eq!(kinds(&list), [("tool", None)], "tool hold remains open");
    let states: Vec<&str> = list[0]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(|e| e["state"].as_str())
        .collect();
    assert_eq!(
        states,
        ["held", "held"],
        "reply verdict must not touch tool steps"
    );
    let bare = post(&s, OPERATOR, PAIR_PATH, &body("h-e-p1", None)).await;
    assert_eq!(bare.status, 400, "step hold requires explicit effect ID");
    for e in ["e-p1", "e-p2"] {
        let r = post(&s, OPERATOR, PAIR_PATH, &body(&format!("h-{e}"), Some(e))).await;
        assert_eq!(r.status, 200, "{e}: {}", r.body);
    }
    assert!(holds_of(&s, "c-pair").await.is_empty());
}

fn two_runs(stepped: bool) -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        for run in ["r-a", "r-b"] {
            waiting_in(d, "personal", run, "c-same");
            let id = format!("h-{run}");
            if stepped {
                let step = step_in(
                    d,
                    "personal",
                    run,
                    "c-same",
                    &format!("e-{run}"),
                    EffectState::Held,
                );
                d.effects.push(step.clone());
                d.holds.push(hold_at(
                    HoldKind::Tool,
                    "personal",
                    run,
                    "c-same",
                    None,
                    views(&[step]),
                ));
            } else {
                d.holds.push(hold_at(
                    HoldKind::Tool,
                    "personal",
                    run,
                    "c-same",
                    Some(&id),
                    vec![],
                ));
            }
        }
    });
    fake
}

fn run_state(s: &Server, run: &str) -> Option<RunState> {
    s.fake
        .edit(|d| d.runs.iter().find(|r| r.run.0 == run).map(|r| r.state))
}

#[tokio::test]
async fn another_run_using_the_same_call_id_keeps_its_hold_and_steps_when_one_is_judged() {
    let s = start(two_runs(true)).await;
    let list = holds_of(&s, "c-same").await;
    assert_eq!(list.len(), 2);
    for h in &list {
        assert_eq!(
            h["steps"].as_array().map(Vec::len),
            Some(1),
            "each hold contains only its own run's step: {h}"
        );
    }
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/holds/personal/r-a/c-same/resolve",
        &body("h-e-r-a", Some("e-r-a")),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let list = holds_of(&s, "c-same").await;
    assert_eq!(list.len(), 1, "r-b hold stays open");
    assert_eq!(
        (
            list[0]["run"].as_str(),
            list[0]["steps"][0]["effect"].as_str(),
            list[0]["steps"][0]["state"].as_str()
        ),
        (Some("r-b"), Some("e-r-b"), Some("held"))
    );
    assert_eq!(run_state(&s, "r-a"), Some(RunState::Ready));
    assert_eq!(
        run_state(&s, "r-b"),
        Some(RunState::Waiting),
        "r-b keeps waiting on its open hold"
    );
    // Cross-run hold_id mismatches return 409 for both settled and open holds.
    for (path, id, effect) in [("r-b", "h-e-r-a", "e-r-a"), ("r-a", "h-e-r-b", "e-r-b")] {
        let wrong = post(
            &s,
            OPERATOR,
            &format!("/v1/admin/holds/personal/{path}/c-same/resolve"),
            &body(id, Some(effect)),
        )
        .await;
        assert_eq!(wrong.status, 409, "{path} {id}: {}", wrong.body);
    }
    let list = holds_of(&s, "c-same").await;
    assert_eq!(list.len(), 1, "conflicting requests change nothing");
    assert_eq!(s.fake.resolutions().len(), 1);
}

#[tokio::test]
async fn call_wide_holds_of_two_runs_with_one_call_id_are_judged_one_at_a_time() {
    let s = start(two_runs(false)).await;
    assert_eq!(holds_of(&s, "c-same").await.len(), 2);
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/holds/personal/r-a/c-same/resolve",
        &body("h-r-a", None),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let list = holds_of(&s, "c-same").await;
    assert_eq!(kinds(&list), [("tool", Some("h-r-b"))]);
    assert_eq!(list[0]["run"], "r-b");
    let wrong = post(
        &s,
        OPERATOR,
        "/v1/admin/holds/personal/r-a/c-same/resolve",
        &body("h-r-b", None),
    )
    .await;
    assert_eq!(wrong.status, 409);
    assert_eq!(holds_of(&s, "c-same").await.len(), 1);
    let key = (
        ProfileId("personal".into()),
        RunId("r-b".into()),
        CallId("c-same".into()),
        HoldKind::Tool,
    );
    let again = s.fake.edit(|d| d.rehold_hold(&key, None));
    let list = holds_of(&s, "c-same").await;
    assert_eq!(kinds(&list), [("tool", Some(again.0.as_str()))]);
    assert_eq!(list[0]["run"], "r-b");
}

#[tokio::test]
async fn an_approval_decision_changes_only_the_call_of_its_own_run() {
    let fake = Fake::demo();
    fake.edit(|d| {
        for (run, nonce) in [("r-a", "ap-a"), ("r-b", "ap-b")] {
            let r = run_view("personal", run, "main", "local:owner", RunState::Waiting);
            let c = call_view(
                &r,
                "c-same",
                "agenda.delete",
                Risk::Irreversible,
                CallState::Awaiting,
            );
            d.approvals
                .push(approval_view(&c, &r, nonce, "local:owner"));
            d.calls.push(c);
            d.runs.push(r);
        }
    });
    let s = start(fake).await;
    let seen = hex(&digest("c-same:agenda.delete"));
    // Decide the second run to catch naive first-match call lookups.
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-b/decide",
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let states = s.fake.edit(|d| {
        ["r-a", "r-b"].map(|run| {
            d.calls
                .iter()
                .find(|c| c.run.0 == run && c.call.0 == "c-same")
                .map(|c| c.state)
        })
    });
    assert_eq!(
        states,
        [Some(CallState::Awaiting), Some(CallState::Running)],
        "same call ID in another run stays untouched"
    );
}

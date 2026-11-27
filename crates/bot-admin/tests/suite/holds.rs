//! Multi-step hold validation, hold kind semantics (`tool`, `reply`, `private`), and `not_executed` resolution markers.

#[path = "holds/final_result.rs"]
mod final_result;
use crate::support::*;
use bot_admin::fake::Fake;
use bot_core::state::RunState;

fn held(effects: &[(&str, &str)]) -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("call");
        for (id, step) in effects {
            d.effects.push(bot_admin::fake::effect_view(
                &call,
                id,
                step,
                bot_core::state::EffectState::Held,
                None,
            ));
        }
    });
    fake
}

#[tokio::test]
async fn a_verdict_must_name_a_held_step_of_that_call() {
    let s = start(held(&[])).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    let ev = |effect: &str| {
        format!(
            r#"{{"verdict":"not_executed","evidence":"recipient lookup confirmed not executed","effect":"{effect}","hold_id":"h-{effect}"}}"#
        )
    };
    assert_eq!(post(&s, OPERATOR, path, &ev("e-404")).await.status, 404);
    assert_eq!(
        post(&s, OPERATOR, path, &ev("e-1")).await.status,
        404,
        "confirmed step has no hold_id"
    );
    let wrong_step =
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-1","hold_id":"h-e-2"}"#;
    assert_eq!(
        post(&s, OPERATOR, path, wrong_step).await.status,
        409,
        "hold_id belongs to e-2"
    );
    let wrong_call =
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-4/c-0/resolve",
            wrong_call
        )
        .await
        .status,
        409,
        "hold_id belongs to c-9"
    );
    let none = r#"{"verdict":"not_executed","evidence":"x","hold_id":"h-e-2"}"#;
    assert_eq!(
        post(&s, OPERATOR, path, none).await.status,
        400,
        "stepped hold requires an explicit effect ID"
    );
    assert!(s.fake.resolutions().is_empty());
    let holds = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert_eq!(
        holds["items"].as_array().map(Vec::len),
        Some(1),
        "hold remains open"
    );
    let state = s
        .fake
        .edit(|d| d.runs.iter().find(|r| r.run.0 == "r-4").map(|r| r.state));
    assert_eq!(state, Some(RunState::Waiting));
    // Steps belonging to another call cannot be resolved via c-9.
    s.fake.edit(|d| {
        let other = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-1")
            .cloned()
            .expect("call");
        d.effects.push(bot_admin::fake::effect_view(
            &other,
            "e-x",
            "other",
            bot_core::state::EffectState::Held,
            None,
        ));
    });
    assert_eq!(post(&s, OPERATOR, path, &ev("e-x")).await.status, 404);
}

#[tokio::test]
async fn a_step_judged_not_executed_stops_being_selectable_while_the_others_stay_held() {
    let s = start(held(&[("e-3", "archive")])).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    let first = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"not_executed","evidence":"recipient lookup confirmed not executed","effect":"e-2","hold_id":"h-e-2"}"#,
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.body);
    let h = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let states: Vec<(&str, &str)> = h["items"][0]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(|x| Some((x["effect"].as_str()?, x["state"].as_str()?)))
        .collect();
    assert_eq!(
        states,
        [("e-1", "confirmed"), ("e-2", "started"), ("e-3", "held")]
    );
    assert!(
        h["items"][0]["steps"][1]["reason"].is_null(),
        "resolved step clears hold reason"
    );
    let selectable: Vec<&str> = h["items"][0]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|x| x["state"] == "held")
        .filter_map(|x| x["effect"].as_str())
        .collect();
    assert_eq!(selectable, ["e-3"], "only unresolved steps stay selectable");
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/status").await.json()["profiles"]["personal"]["holds"],
        1
    );
    let state = s.fake.edit(|d| {
        d.runs
            .iter()
            .find(|r| r.run.0 == "r-4")
            .map(|r| (r.state, r.waiting.is_some()))
    });
    assert_eq!(
        state,
        Some((RunState::Waiting, true)),
        "run waits while held steps remain"
    );
    let again = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"confirmed","receipt":"late-receipt","effect":"e-2","hold_id":"h-e-2"}"#,
    )
    .await;
    assert_eq!(
        (again.status, again.json()["error"].as_str()),
        (409, Some("conflict"))
    );
    let h = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert_eq!(h["items"][0]["steps"][2]["state"], "held");
    assert!(
        h["items"][0]["steps"][2]["receipt"].is_null(),
        "remaining step stays untouched"
    );
    // Even if all held steps are not_executed, a confirmed step forces a call-wide receipt hold.
    assert_eq!(
        post(
            &s,
            OPERATOR,
            path,
            r#"{"verdict":"not_executed","evidence":"no record at recipient","effect":"e-3","hold_id":"h-e-3"}"#
        )
        .await
        .status,
        200
    );
    let state = s.fake.edit(|d| {
        d.runs
            .iter()
            .find(|r| r.run.0 == "r-4")
            .map(|r| (r.state, r.waiting.is_some()))
    });
    assert_eq!(state, Some((RunState::Waiting, true)));
    let view = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert!(view["items"][0]["hold_id"].is_string());
    assert_eq!(view["items"][0]["steps"][0]["receipt"], "file-17");
}

fn kinds() -> Fake {
    use bot_admin::fake::run_view;
    use bot_core::{
        admin::{HoldKind, HoldView, Waiting},
        id::{CallId, HoldId, ProfileId, RunId, ToolId},
    };
    let fake = Fake::demo();
    fake.edit(|d| {
        for (run, call, tool, kind) in [
            ("r-p1", "private:r-p1", "inbox.receive", HoldKind::Private),
            ("r-p2", "private:r-p2", "inbox.receive", HoldKind::Private),
            ("r-rp", "c-rp", "reply", HoldKind::Reply),
            // Store-provided kind must override suggestive call/tool names.
            ("r-t1", "private:r-t1", "reply", HoldKind::Tool),
            ("r-q1", "c-q1", "agenda.list", HoldKind::Private),
        ] {
            let mut r = run_view("personal", run, "main", "local:owner", RunState::Waiting);
            r.waiting = Some(Waiting::Hold(CallId(call.into())));
            d.runs.push(r);
            d.holds.push(HoldView {
                kind,
                hold_id: Some(HoldId(format!("h-{call}"))),
                profile: ProfileId("personal".into()),
                run: RunId(run.into()),
                call: CallId(call.into()),
                tool: ToolId(tool.into()),
                reason: "unknown_outcome".into(),
                since: std::time::SystemTime::now(),
                steps: Vec::new(),
            });
        }
    });
    fake
}

#[tokio::test]
async fn holds_are_labelled_by_kind_so_the_screen_can_say_what_not_executed_will_do() {
    let s = start(kinds()).await;
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let kind = |call: &str| {
        list["items"]
            .as_array()
            .expect("list")
            .iter()
            .find(|h| h["call"] == call)
            .map(|h| h["kind"].as_str().map(str::to_owned))
    };
    assert_eq!(kind("c-9"), Some(Some("tool".into())), "tool call");
    assert_eq!(
        kind("private:r-p1"),
        Some(Some("private".into())),
        "private DM"
    );
    assert_eq!(kind("c-rp"), Some(Some("reply".into())), "reply");
}

#[tokio::test]
async fn a_sensitive_dm_hold_ends_instead_of_resuming() {
    let s = start(kinds()).await;
    let ev = |call: &str| {
        format!(
            r#"{{"verdict":"not_executed","evidence":"no record at recipient","hold_id":"h-{call}"}}"#
        )
    };
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-p1/private:r-p1/resolve",
            &ev("private:r-p1")
        )
        .await
        .status,
        200
    );
    let run = |id: &str| {
        s.fake.edit(|d| {
            d.runs
                .iter()
                .find(|r| r.run.0 == id)
                .map(|r| (r.state, r.error.clone(), r.waiting.is_some()))
        })
    };
    assert_eq!(
        run("r-p1"),
        Some((RunState::Dead, Some("not_executed".into()), false)),
        "private DM cannot replay because body is not stored"
    );
    let ok = r#"{"verdict":"confirmed","receipt":"dm-77","hold_id":"h-private:r-p2"}"#;
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-p2/private:r-p2/resolve",
            ok
        )
        .await
        .status,
        200
    );
    assert_eq!(run("r-p2"), Some((RunState::Succeeded, None, false)));
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-rp/c-rp/resolve",
            &ev("c-rp")
        )
        .await
        .status,
        200
    );
    assert_eq!(run("r-rp"), Some((RunState::Ready, None, false)));
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let calls: Vec<&str> = list["items"]
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|h| h["call"].as_str())
        .collect();
    assert_eq!(
        calls,
        ["c-9", "private:r-t1", "c-q1"],
        "only unresolved holds remain"
    );
    for untouched in ["r-t1", "r-q1"] {
        assert_eq!(
            run(untouched),
            Some((RunState::Waiting, None, true)),
            "{untouched}"
        );
    }
}

#[tokio::test]
async fn the_stores_kind_wins_over_names_that_merely_look_like_another_kind() {
    let s = start(kinds()).await;
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let kind = |call: &str| {
        list["items"]
            .as_array()
            .expect("list")
            .iter()
            .find(|h| h["call"] == call)
            .and_then(|h| h["kind"].as_str().map(str::to_owned))
    };
    assert_eq!(kind("private:r-t1").as_deref(), Some("tool"));
    assert_eq!(kind("c-q1").as_deref(), Some("private"));
    let ev = |call: &str| {
        format!(
            r#"{{"verdict":"not_executed","evidence":"no record at recipient","hold_id":"h-{call}"}}"#
        )
    };
    let run = |id: &str| {
        s.fake.edit(|d| {
            d.runs
                .iter()
                .find(|r| r.run.0 == id)
                .map(|r| (r.state, r.error.clone()))
        })
    };
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-t1/private:r-t1/resolve",
            &ev("private:r-t1")
        )
        .await
        .status,
        200
    );
    assert_eq!(run("r-t1"), Some((RunState::Ready, None)));
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-q1/c-q1/resolve",
            &ev("c-q1")
        )
        .await
        .status,
        200
    );
    assert_eq!(
        run("r-q1"),
        Some((RunState::Dead, Some("not_executed".into())))
    );
}

#[tokio::test]
async fn only_a_step_judged_not_executed_carries_the_resolution_mark_a_new_or_restarted_start_does_not()
 {
    let s = start(held(&[("e-3", "archive")])).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    async fn steps(s: &Server) -> serde_json::Value {
        get(s, OPERATOR, "/v1/admin/holds").await.json()["items"][0]["steps"].clone()
    }
    let before = steps(&s).await;
    assert!(
        before
            .as_array()
            .expect("steps")
            .iter()
            .all(|x| x["resolution"].is_null())
    );
    assert_eq!(
        post(
            &s,
            OPERATOR,
            path,
            r#"{"verdict":"not_executed","evidence":"no record at recipient","effect":"e-2","hold_id":"h-e-2"}"#
        )
        .await
        .status,
        200
    );
    let judged = steps(&s).await;
    let by = |id: &str| {
        judged
            .as_array()
            .expect("steps")
            .iter()
            .find(|x| x["effect"] == id)
            .expect("step")
            .clone()
    };
    assert_eq!(
        (
            by("e-2")["state"].as_str(),
            by("e-2")["resolution"].as_str()
        ),
        (Some("started"), Some("not_executed"))
    );
    assert_eq!(
        (
            by("e-3")["state"].as_str(),
            by("e-3")["resolution"].is_null()
        ),
        (Some("held"), true)
    );
    assert!(
        by("e-1")["resolution"].is_null(),
        "confirmed step has no resolution mark"
    );
    // Freshly started steps have no resolution mark, distinguishing them from judged steps.
    s.fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("call");
        d.effects.push(bot_admin::fake::effect_view(
            &call,
            "e-9",
            "fresh",
            bot_core::state::EffectState::Started,
            None,
        ));
    });
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    let fresh = d["effects"]
        .as_array()
        .expect("effects")
        .iter()
        .find(|x| x["effect"] == "e-9")
        .expect("e-9")
        .clone();
    assert_eq!(
        (fresh["state"].as_str(), fresh["resolution"].is_null()),
        (Some("started"), true)
    );
    let e2 = d["effects"]
        .as_array()
        .expect("effects")
        .iter()
        .find(|x| x["effect"] == "e-2")
        .expect("e-2")
        .clone();
    assert_eq!(
        (e2["state"].as_str(), e2["resolution"].as_str()),
        (Some("started"), Some("not_executed")),
        "run detail shows the same resolution mark"
    );
    // Restarting the effect clears the resolution mark.
    s.fake.edit(|d| d.restart_effect("personal", "e-2"));
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    let e2 = d["effects"]
        .as_array()
        .expect("effects")
        .iter()
        .find(|x| x["effect"] == "e-2")
        .expect("e-2")
        .clone();
    assert_eq!(
        (e2["state"].as_str(), e2["resolution"].is_null()),
        (Some("started"), true)
    );
}

//! Profile isolation for holds, effects, approvals, and runs sharing identical run/call/effect IDs.

use crate::support::{
    holdfx::{hold, step_of, views, waiting_run, with_hold_id},
    *,
};
use bot_admin::{
    fake::{Fake, approval_view, call_view, digest, run_view},
    json::hex,
};
use bot_core::{
    admin::{CallState, HoldKind, Waiting},
    id::{CallId, ProfileId, RunId},
    state::{EffectState, Risk, RunState},
};
use serde_json::Value;

const PROFILES: [&str; 2] = ["personal", "work"];

fn path(profile: &str) -> String {
    format!("/v1/admin/holds/{profile}/r-x/c-x/resolve")
}

/// Two profiles sharing `r-x` / `c-x` / `e-x` with distinct per-profile hold IDs.
fn crossed(stepped: bool) -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        for profile in PROFILES {
            waiting_run(d, profile, "r-x", "c-x");
            let id = format!("h-{profile}-x");
            if stepped {
                let step = with_hold_id(
                    step_of(d, profile, "r-x", "c-x", "e-x", EffectState::Held),
                    &id,
                );
                d.effects.push(step.clone());
                d.holds.push(hold(
                    HoldKind::Tool,
                    profile,
                    "r-x",
                    "c-x",
                    None,
                    views(&[step]),
                ));
            } else {
                d.holds.push(hold(
                    HoldKind::Tool,
                    profile,
                    "r-x",
                    "c-x",
                    Some(&id),
                    vec![],
                ));
            }
        }
    });
    fake
}

fn confirm(profile: &str, stepped: bool, hold_profile: &str) -> String {
    let at = if stepped { r#","effect":"e-x""# } else { "" };
    format!(
        r#"{{"verdict":"confirmed","receipt":"R-{profile}","hold_id":"h-{hold_profile}-x"{at}}}"#
    )
}

async fn holds_in(s: &Server, profile: &str) -> Vec<Value> {
    get(s, OPERATOR, "/v1/admin/holds").await.json()["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|h| h["profile"] == profile && h["call"] == "c-x")
        .cloned()
        .collect()
}

async fn confirm_final(s: &Server, profile: &str) {
    let holds = holds_in(s, profile).await;
    let id = holds[0]["hold_id"].as_str().expect("call-wide hold_id");
    let body = serde_json::json!({"verdict": "confirmed", "receipt": "all", "hold_id": id});
    assert_eq!(
        post(s, OPERATOR, &path(profile), &body.to_string())
            .await
            .status,
        200
    );
}

fn run_state(s: &Server, profile: &str) -> Option<RunState> {
    s.fake.edit(|d| {
        d.runs
            .iter()
            .find(|r| r.profile.0 == profile && r.run.0 == "r-x")
            .map(|r| r.state)
    })
}

fn step_state(s: &Server, profile: &str) -> Option<(EffectState, Option<String>)> {
    s.fake.edit(|d| {
        d.effects
            .iter()
            .find(|e| e.profile.0 == profile && e.run.0 == "r-x" && e.effect.0 == "e-x")
            .map(|e| (e.state, e.receipt.clone()))
    })
}

#[tokio::test]
async fn each_profile_keeps_its_own_steps_and_hold_ids_when_run_and_call_ids_match() {
    let s = start(crossed(true)).await;
    for profile in PROFILES {
        let list = holds_in(&s, profile).await;
        assert_eq!(list.len(), 1, "{profile}: expected one hold");
        let steps = list[0]["steps"].as_array().expect("steps");
        assert_eq!(steps.len(), 1, "{profile}: expected only own step");
        assert_eq!(steps[0]["hold_id"], format!("h-{profile}-x"));
    }
    for profile in PROFILES {
        let d = get(&s, OPERATOR, &format!("/v1/admin/runs/{profile}/r-x"))
            .await
            .json();
        assert_eq!(d["calls"].as_array().map(Vec::len), Some(1), "{profile}");
        let effects = d["effects"].as_array().expect("effects");
        assert_eq!(effects.len(), 1, "{profile}");
        assert_eq!(effects[0]["hold_id"], format!("h-{profile}-x"), "{profile}");
    }
}

#[tokio::test]
async fn judging_one_profile_leaves_the_other_profiles_step_and_run_alone() {
    for (done, other) in [("work", "personal"), ("personal", "work")] {
        let s = start(crossed(true)).await;
        let r = post(&s, OPERATOR, &path(done), &confirm(done, true, done)).await;
        assert_eq!(r.status, 200, "{done}: {}", r.body);
        assert_eq!(
            step_state(&s, done),
            Some((EffectState::Confirmed, Some(format!("R-{done}"))))
        );
        assert_eq!(
            step_state(&s, other),
            Some((EffectState::Held, None)),
            "{other}: matching step must remain held"
        );
        assert_eq!(run_state(&s, done), Some(RunState::Waiting));
        confirm_final(&s, done).await;
        assert_eq!(run_state(&s, done), Some(RunState::Ready));
        assert_eq!(
            run_state(&s, other),
            Some(RunState::Waiting),
            "{other}: run must stay waiting"
        );
        assert!(holds_in(&s, done).await.is_empty());
        assert_eq!(holds_in(&s, other).await.len(), 1);
        // Status counts are per-profile: personal also includes the demo c-9 hold.
        let status = get(&s, OPERATOR, "/v1/admin/status").await.json();
        for profile in PROFILES {
            let want = u64::from(profile == "personal") + u64::from(profile == other);
            assert_eq!(
                status["profiles"][profile]["holds"].as_u64(),
                Some(want),
                "{profile}: hold count"
            );
        }
    }
}

#[tokio::test]
async fn another_profiles_hold_id_never_applies_here() {
    let s = start(crossed(true)).await;
    // Submitting personal's hold_id against work's route must conflict without mutating either profile.
    let r = post(
        &s,
        OPERATOR,
        &path("work"),
        &confirm("work", true, "personal"),
    )
    .await;
    assert_eq!(r.status, 409, "{}", r.body);
    for profile in PROFILES {
        assert_eq!(
            step_state(&s, profile),
            Some((EffectState::Held, None)),
            "{profile}"
        );
        assert_eq!(run_state(&s, profile), Some(RunState::Waiting), "{profile}");
    }
    assert!(s.fake.resolutions().is_empty());
}

#[tokio::test]
async fn call_wide_holds_of_two_profiles_with_one_run_and_call_id_are_judged_separately() {
    let s = start(crossed(false)).await;
    let r = post(&s, OPERATOR, &path("work"), &confirm("work", false, "work")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(holds_in(&s, "work").await.is_empty());
    assert_eq!(
        holds_in(&s, "personal").await.len(),
        1,
        "other profile's matching call-wide hold must remain open"
    );
    assert_eq!(run_state(&s, "work"), Some(RunState::Ready));
    assert_eq!(run_state(&s, "personal"), Some(RunState::Waiting));
    let r = post(
        &s,
        OPERATOR,
        &path("personal"),
        &confirm("personal", false, "personal"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(holds_in(&s, "personal").await.is_empty());
}

#[tokio::test]
async fn re_holding_and_restarting_touch_only_the_named_profiles_step() {
    let s = start(crossed(true)).await;
    for profile in PROFILES {
        let r = post(
            &s,
            OPERATOR,
            &path(profile),
            &confirm(profile, true, profile),
        )
        .await;
        assert_eq!(r.status, 200, "{profile}: {}", r.body);
    }
    confirm_final(&s, "personal").await;
    let key = (
        ProfileId("work".into()),
        RunId("r-x".into()),
        CallId("c-x".into()),
        HoldKind::Tool,
    );
    let renewed = s.fake.edit(|d| d.rehold_hold(&key, Some("e-x")));
    assert_eq!(step_state(&s, "work"), Some((EffectState::Held, None)));
    assert_eq!(
        step_state(&s, "personal"),
        Some((EffectState::Confirmed, Some("R-personal".into())))
    );
    let list = holds_in(&s, "work").await;
    assert_eq!(list[0]["steps"][0]["hold_id"], renewed.0);
    assert_eq!(run_state(&s, "work"), Some(RunState::Waiting));
    assert_eq!(run_state(&s, "personal"), Some(RunState::Ready));
    s.fake.edit(|d| d.restart_effect("work", "e-x"));
    assert_eq!(
        step_state(&s, "work").map(|(state, _)| state),
        Some(EffectState::Started)
    );
    assert_eq!(
        step_state(&s, "personal").map(|(state, _)| state),
        Some(EffectState::Confirmed)
    );
}

#[tokio::test]
async fn an_approval_decision_changes_only_the_call_of_its_own_profile() {
    let fake = Fake::demo();
    fake.edit(|d| {
        for (profile, nonce) in [("personal", "ap-p"), ("work", "ap-w")] {
            let mut r = run_view(profile, "r-x", "main", "local:owner", RunState::Waiting);
            r.waiting = Some(Waiting::Approval);
            let c = call_view(
                &r,
                "c-x",
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
    let seen = hex(&digest("c-x:agenda.delete"));
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-w/decide",
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let states = s.fake.edit(|d| {
        PROFILES.map(|profile| {
            d.calls
                .iter()
                .find(|c| c.profile.0 == profile && c.run.0 == "r-x" && c.call.0 == "c-x")
                .map(|c| c.state)
        })
    });
    assert_eq!(
        states,
        [Some(CallState::Awaiting), Some(CallState::Running)],
        "other profile's matching call must remain untouched"
    );
    assert_eq!(run_state(&s, "personal"), Some(RunState::Waiting));
    assert_eq!(run_state(&s, "work"), Some(RunState::Ready));
}

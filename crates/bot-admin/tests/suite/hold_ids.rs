//! Hold IDs: resolutions bind strictly to the exact hold occurrence, making retries idempotent across re-holds.

use crate::support::*;
use bot_admin::{FenceFuture, fake::Fake};
use bot_core::{id::HoldId, state::RunState};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::SeqCst},
};

const PATH: &str = "/v1/admin/holds/personal/r-4/c-9/resolve";

fn confirm(receipt: &str, hold: &str) -> String {
    format!(r#"{{"verdict":"confirmed","receipt":"{receipt}","effect":"e-2","hold_id":"{hold}"}}"#)
}

async fn audit_resolves(s: &Server) -> usize {
    let list = get(s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    list["items"].as_array().map_or(0, |v| {
        v.iter().filter(|e| e["action"] == "hold.resolve").count()
    })
}

fn step(list: &serde_json::Value, effect: &str) -> serde_json::Value {
    list["items"][0]["steps"]
        .as_array()
        .and_then(|v| v.iter().find(|x| x["effect"] == effect))
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn ids_are_exposed_on_resolvable_steps_and_single_holds_but_not_on_the_bundle() {
    let s = start(Fake::demo()).await;
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert!(
        list["items"][0]["hold_id"].is_null(),
        "multi-step hold bundle carries no top-level hold_id"
    );
    assert_eq!(
        step(&list, "e-2")["hold_id"],
        "h-e-2",
        "resolvable held step exposes its own hold_id"
    );
    assert!(
        step(&list, "e-1")["hold_id"].is_null(),
        "confirmed step has no hold_id"
    );
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    let ids: Vec<(&str, Option<&str>)> = d["effects"]
        .as_array()
        .expect("effects")
        .iter()
        .map(|e| (e["effect"].as_str().unwrap_or(""), e["hold_id"].as_str()))
        .collect();
    assert_eq!(ids, [("e-1", None), ("e-2", Some("h-e-2"))]);
    assert_eq!(
        post(&s, OPERATOR, PATH, &confirm("msg-9", "h-e-2"))
            .await
            .status,
        200
    );
    let after = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    assert!(after["effects"][1]["hold_id"].is_null());
}

/// Retry after a lost HTTP response (503 post-commit) must report repeated without touching a renewed hold H2.
#[tokio::test]
async fn a_retry_of_a_committed_judgment_repeats_it_and_never_touches_the_renewed_hold() {
    // Fence fails only on the 3rd check (post-commit response gate of the resolve POST).
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let s = start_cfg(Fake::demo(), move |cfg| {
        cfg.fence(move || -> FenceFuture {
            let n = c.fetch_add(1, SeqCst);
            Box::pin(async move { n != 2 })
        })
    })
    .await;
    let csrf = csrf(&s, OPERATOR).await;
    let hs = [
        ("Authorization", "Bearer operator-secret-1"),
        ("Content-Type", "application/json"),
        ("X-Bot-Csrf", csrf.as_str()),
    ];
    let h1 = confirm("msg-9", "h-e-2");
    let lost = raw(s.addr, "POST", PATH, None, &hs, Some(&h1)).await;
    assert_eq!(
        (lost.status, lost.body.as_str()),
        (503, r#"{"error":"unavailable"}"#),
        "response lost after commit"
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (1, 1),
        "H1 applied once"
    );
    let h2: HoldId = s.fake.edit(|d| d.rehold("c-9", Some("e-2")));
    assert_ne!(h2.0, "h-e-2");
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert_eq!(
        (
            step(&list, "e-2")["state"].as_str(),
            step(&list, "e-2")["hold_id"].as_str()
        ),
        (Some("held"), Some(h2.0.as_str()))
    );
    let again = raw(s.addr, "POST", PATH, None, &hs, Some(&h1)).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true)),
        "{}",
        again.body
    );
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert_eq!(
        (
            step(&list, "e-2")["state"].as_str(),
            step(&list, "e-2")["receipt"].is_null()
        ),
        (Some("held"), true),
        "H2 must remain open"
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (1, 1),
        "repeated retry must not add resolutions or audit entries"
    );
    assert_eq!(
        s.fake
            .edit(|d| d.runs.iter().find(|r| r.run.0 == "r-4").map(|r| r.state)),
        Some(RunState::Waiting),
        "run stays waiting while H2 is unresolved"
    );
    let second = raw(
        s.addr,
        "POST",
        PATH,
        None,
        &hs,
        Some(&confirm("msg-22", &h2.0)),
    )
    .await;
    assert_eq!(
        (second.status, second.json()["repeated"].as_bool()),
        (200, Some(false)),
        "{}",
        second.body
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (2, 2)
    );
    let list = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    assert_eq!(list["effects"][1]["receipt"], "msg-22");
}

#[tokio::test]
async fn the_same_id_with_another_value_operator_or_target_is_a_conflict_not_a_repeat() {
    let s = start(Fake::demo()).await;
    assert_eq!(
        post(&s, OPERATOR, PATH, &confirm("msg-9", "h-e-2"))
            .await
            .status,
        200
    );
    s.fake.edit(|d| d.rehold("c-9", Some("e-2")));
    for (why, body) in [
        ("different receipt", confirm("other", "h-e-2")),
        (
            "different verdict",
            r#"{"verdict":"not_executed","evidence":"msg-9","effect":"e-2","hold_id":"h-e-2"}"#
                .to_owned(),
        ),
        (
            "different effect",
            r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-1","hold_id":"h-e-2"}"#
                .to_owned(),
        ),
    ] {
        let r = post(&s, OPERATOR, PATH, &body).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (409, Some("conflict")),
            "{why}"
        );
    }
    // Trimmed-equivalent input counts as the same judgment.
    let same = post(&s, OPERATOR, PATH, &confirm("  msg-9  ", "h-e-2")).await;
    assert_eq!(
        (same.status, same.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (1, 1)
    );
    assert_eq!(
        post(&s, OPERATOR, PATH, &confirm("msg-9", "h-nobody"))
            .await
            .status,
        404
    );
    assert_eq!(
        post(&s, GUEST, PATH, &confirm("msg-9", "h-e-2"))
            .await
            .status,
        403
    );
}

#[tokio::test]
async fn a_call_wide_hold_has_its_own_id_and_repeats_after_it_is_held_again() {
    use bot_admin::fake::run_view;
    use bot_core::{
        admin::{HoldKind, HoldView, Waiting},
        id::{CallId, ProfileId, RunId, ToolId},
    };
    let fake = Fake::demo();
    fake.edit(|d| {
        let mut r = run_view("personal", "r-q", "main", "local:owner", RunState::Waiting);
        r.waiting = Some(Waiting::Hold(CallId("c-q".into())));
        d.runs.push(r);
        d.holds.push(HoldView {
            kind: HoldKind::Reply,
            hold_id: Some(HoldId("h-c-q".into())),
            profile: ProfileId("personal".into()),
            run: RunId("r-q".into()),
            call: CallId("c-q".into()),
            tool: ToolId("reply".into()),
            reason: "unknown_outcome".into(),
            since: std::time::SystemTime::now(),
            steps: Vec::new(),
        });
    });
    let s = start(fake).await;
    let path = "/v1/admin/holds/personal/r-q/c-q/resolve";
    let body = |id: &str| {
        format!(
            r#"{{"verdict":"not_executed","evidence":"no record at recipient","hold_id":"{id}"}}"#
        )
    };
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let mine = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|h| h["call"] == "c-q")
        .cloned()
        .expect("hold");
    assert_eq!(mine["hold_id"], "h-c-q");
    assert_eq!(post(&s, OPERATOR, path, &body("h-c-q")).await.status, 200);
    let h2 = s.fake.edit(|d| d.rehold("c-q", None));
    let again = post(&s, OPERATOR, path, &body("h-c-q")).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let renewed = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|h| h["call"] == "c-q")
        .cloned()
        .expect("renewed hold");
    assert_eq!(renewed["hold_id"].as_str(), Some(h2.0.as_str()));
    assert_eq!(audit_resolves(&s).await, 1);
    let second = post(&s, OPERATOR, path, &body(&h2.0)).await;
    assert_eq!(
        (second.status, second.json()["repeated"].as_bool()),
        (200, Some(false))
    );
    assert_eq!(audit_resolves(&s).await, 2);
}

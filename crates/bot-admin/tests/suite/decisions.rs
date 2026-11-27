//! Approval and hold decision tests: broker routing, designated approver, operator-only holds, CSRF, and audit chain.

use crate::support::*;
use bot_admin::fake::{Fake, digest};
use bot_core::{admin::ApprovalState, state::RunState};

fn seen() -> String {
    bot_admin::json::hex(&digest("c-1:agenda.delete"))
}

fn decide_body(allow: bool) -> String {
    format!(r#"{{"allow":{allow},"seen":"{}"}}"#, seen())
}

#[tokio::test]
async fn approval_goes_through_the_broker_as_the_authenticated_approver() {
    let s = start(Fake::demo()).await;
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["approval"]["state"], "approved");
    let d = s.fake.decisions();
    assert_eq!(d.len(), 1);
    assert_eq!(
        (d[0].nonce.0.as_str(), d[0].actor.0.as_str(), d[0].allow),
        ("ap-1", "local:owner", true)
    );
    let again = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    assert_eq!(again.status, 200);
    assert_eq!(
        (
            again.json()["repeated"].as_bool(),
            again.json()["approval"]["state"].as_str()
        ),
        (Some(true), Some("approved"))
    );
    assert_eq!(s.fake.decisions().len(), 1);
    let a = get(&s, OPERATOR, "/v1/admin/audit").await.json();
    assert_eq!(
        (
            a["items"][0]["action"].as_str(),
            a["items"][0]["actor"].as_str(),
            a["items"][0]["verdict"].as_str()
        ),
        (Some("approval.decide"), Some("local:owner"), Some("allow"))
    );
}

#[tokio::test]
async fn denying_is_recorded_and_the_run_moves_on() {
    let s = start(Fake::demo()).await;
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(false),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["approval"]["state"], "denied");
    assert!(!s.fake.decisions()[0].allow);
}

#[tokio::test]
async fn only_the_designated_approver_may_decide_and_stale_screens_are_refused() {
    let s = start(Fake::demo()).await;
    // Even an operator cannot decide on behalf of another designated approver.
    s.fake
        .edit(|d| d.approvals[0].approver = bot_admin::fake::pid("local:guest"));
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (403, Some("denied"))
    );
    assert!(s.fake.decisions().is_empty(), "must not reach broker");
    s.fake
        .edit(|d| d.approvals[0].approver = bot_admin::fake::pid("local:owner"));
    let wrong = format!(r#"{{"allow":true,"seen":"{}"}}"#, "0".repeat(64));
    let r = post(&s, OPERATOR, "/v1/admin/approvals/ap-1/decide", &wrong).await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("changed"))
    );
    s.fake
        .edit(|d| d.approvals[0].state = ApprovalState::Superseded);
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    assert_eq!((r.status, r.json()["error"].as_str()), (409, Some("stale")));
    assert!(s.fake.decisions().is_empty());
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/approvals/ap-404/decide",
            &decide_body(true)
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn a_member_cannot_decide_someone_elses_approval() {
    let s = start(Fake::demo()).await;
    let r = post(
        &s,
        GUEST,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    assert_eq!(
        r.status, 404,
        "invisible approvals return 404 to hide existence"
    );
    assert!(s.fake.decisions().is_empty());
}

#[tokio::test]
async fn state_changes_need_the_csrf_guards() {
    let s = start(Fake::demo()).await;
    let auth = ("Authorization", "Bearer operator-secret-1");
    let tok = csrf(&s, OPERATOR).await;
    let path = "/v1/admin/approvals/ap-1/decide";
    let body = decide_body(true);
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "missing token",
            vec![auth, ("Content-Type", "application/json")],
        ),
        (
            "wrong token",
            vec![
                auth,
                ("Content-Type", "application/json"),
                ("X-Bot-Csrf", "0000"),
            ],
        ),
        (
            "not JSON",
            vec![auth, ("Content-Type", "text/plain"), ("X-Bot-Csrf", &tok)],
        ),
        (
            "cross origin",
            vec![
                auth,
                ("Content-Type", "application/json"),
                ("X-Bot-Csrf", &tok),
                ("Origin", "http://evil.example"),
            ],
        ),
        (
            "cross-site",
            vec![
                auth,
                ("Content-Type", "application/json"),
                ("X-Bot-Csrf", &tok),
                ("Sec-Fetch-Site", "cross-site"),
            ],
        ),
    ];
    for (name, headers) in cases {
        let r = raw(s.addr, "POST", path, None, &headers, Some(&body)).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (403, Some("csrf")),
            "{name}"
        );
    }
    assert!(
        s.fake.decisions().is_empty(),
        "blocked requests must not reach broker"
    );
    // Tokens are bound to the authenticated principal.
    let other = csrf(&s, GUEST).await;
    let h = [
        auth,
        ("Content-Type", "application/json"),
        ("X-Bot-Csrf", other.as_str()),
    ];
    assert_eq!(
        raw(s.addr, "POST", path, None, &h, Some(&body))
            .await
            .status,
        403
    );
    let origin = format!("http://{}", s.addr);
    let h = [
        auth,
        ("Content-Type", "application/json"),
        ("X-Bot-Csrf", tok.as_str()),
        ("Origin", origin.as_str()),
        ("Sec-Fetch-Site", "same-origin"),
    ];
    assert_eq!(
        raw(s.addr, "POST", path, None, &h, Some(&body))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn decision_bodies_are_validated() {
    let s = start(Fake::demo()).await;
    for bad in [
        "{}",
        r#"{"allow":"yes","seen":"x"}"#,
        r#"{"allow":true}"#,
        &format!(r#"{{"allow":true,"seen":"{}","extra":1}}"#, seen()),
        "[]",
        "not json",
        &format!(r#"{{"allow":true,"seen":"{}"}}"#, "a".repeat(5000)),
    ] {
        let r = post(&s, OPERATOR, "/v1/admin/approvals/ap-1/decide", bad).await;
        assert!(
            r.status == 400 || r.status == 413,
            "{bad:.40}: {}",
            r.status
        );
    }
    assert!(s.fake.decisions().is_empty());
}

async fn audit_count(s: &Server) -> Option<usize> {
    get(s, OPERATOR, "/v1/admin/audit?limit=200").await.json()["items"]
        .as_array()
        .map(Vec::len)
}

#[tokio::test]
async fn hold_verdicts_are_operator_only_audited_and_idempotent() {
    let s = start(Fake::demo()).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    let body = r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, GUEST, path, body).await.status, 403);
    assert!(s.fake.resolutions().is_empty());
    let r = post(&s, OPERATOR, path, body).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let rs = s.fake.resolutions();
    assert_eq!(
        (
            rs[0].actor.0.as_str(),
            rs[0].hold.call.0.as_str(),
            rs[0].hold.effect.as_ref().map(|e| e.0.as_str())
        ),
        ("local:owner", "c-9", Some("e-2"))
    );
    // Raw receipt text is not retained; only the verdict kind and digest are stored.
    assert_eq!(
        (rs[0].verdict, rs[0].digest),
        ("confirmed", bot_admin::fake::digest("msg-9"))
    );
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    assert_eq!(
        (
            d["effects"][0]["receipt"].as_str(),
            d["effects"][1]["receipt"].as_str(),
            d["effects"][1]["state"].as_str()
        ),
        (Some("file-17"), Some("msg-9"), Some("confirmed"))
    );
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert!(list["items"][0]["hold_id"].is_string());
    assert_eq!(list["items"][0]["steps"][1]["receipt"], "msg-9");
    // Replaying the same hold_id, operator, and verdict returns repeated=true with no extra audit row.
    let again = post(&s, OPERATOR, path, body).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    assert_eq!(s.fake.resolutions().len(), 1);
    assert_eq!(
        audit_count(&s).await,
        Some(3),
        "only the first resolution appends an audit row (demo 2 + 1)"
    );
    let a = get(&s, OPERATOR, "/v1/admin/audit?limit=1").await.json();
    assert_eq!(
        (
            a["items"][0]["action"].as_str(),
            a["items"][0]["verdict"].as_str()
        ),
        (Some("hold.resolve"), Some("confirmed"))
    );
    let other =
        r#"{"verdict":"confirmed","receipt":"other-receipt","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, other).await.status, 409);
    let kind = r#"{"verdict":"not_executed","evidence":"msg-9","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, kind).await.status, 409);
    let target = r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-1","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, target).await.status, 409);
    let unknown =
        r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-2","hold_id":"h-unknown"}"#;
    assert_eq!(post(&s, OPERATOR, path, unknown).await.status, 404);
    assert_eq!(s.fake.resolutions().len(), 1);
    assert_eq!(
        audit_count(&s).await,
        Some(3),
        "rejected requests must not append audit rows"
    );
}

#[tokio::test]
async fn not_executed_resumes_the_run_and_bad_verdicts_are_refused() {
    let fake = Fake::demo();
    // Tests resume when all steps are not_executed; confirmed-step transitions live in holds.rs.
    fake.edit(|d| d.effects.retain(|e| e.effect.0 != "e-1"));
    let s = start(fake).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    for bad in [
        r#"{"verdict":"confirmed","hold_id":"h-e-2"}"#,
        r#"{"verdict":"confirmed","receipt":"","hold_id":"h-e-2"}"#,
        r#"{"verdict":"confirmed","receipt":"a","evidence":"b","hold_id":"h-e-2"}"#,
        r#"{"verdict":"not_executed","hold_id":"h-e-2"}"#,
        r#"{"verdict":"not_executed","receipt":"x","hold_id":"h-e-2"}"#,
        r#"{"verdict":"not_executed","evidence":"line\nbreak","hold_id":"h-e-2"}"#,
        r#"{"verdict":"retry","evidence":"x","hold_id":"h-e-2"}"#,
        r#"{"evidence":"x","hold_id":"h-e-2"}"#,
        r#"{"verdict":"not_executed","evidence":"x","effect":"../e","hold_id":"h-e-2"}"#,
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-2"}"#,
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-2","hold_id":""}"#,
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-2","hold_id":7}"#,
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-2","hold_id":"a b"}"#,
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-2","hold_id":"../h"}"#,
    ] {
        assert_eq!(post(&s, OPERATOR, path, bad).await.status, 400, "{bad}");
    }
    assert!(s.fake.resolutions().is_empty());
    assert_eq!(
        post(
            &s,
            OPERATOR,
            "/v1/admin/holds/personal/r-4/c-0/resolve",
            r#"{"verdict":"not_executed","evidence":"ok","hold_id":"h-none"}"#
        )
        .await
        .status,
        404
    );
    let ok = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"not_executed","evidence":"recipient lookup confirmed request absent","effect":"e-2","hold_id":"h-e-2"}"#,
    )
    .await;
    assert_eq!(ok.status, 200);
    let state = s
        .fake
        .edit(|d| d.runs.iter().find(|r| r.run.0 == "r-4").map(|r| r.state));
    assert_eq!(state, Some(RunState::Ready));
}

#[tokio::test]
async fn audit_chain_verifies_and_reports_the_first_tampered_entry() {
    let s = start(Fake::demo()).await;
    post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    let ok = get(&s, OPERATOR, "/v1/admin/audit/verify").await.json();
    assert_eq!(
        (
            ok["ok"].as_bool(),
            ok["entries"].as_u64(),
            ok["broken_at"].is_null()
        ),
        (Some(true), Some(3), true)
    );
    let rows = get(&s, OPERATOR, "/v1/admin/audit").await.json();
    let seqs: Vec<u64> = rows["items"]
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert_eq!(seqs, [3, 2, 1], "newest first");
    assert_eq!(
        rows["items"][1]["prev"], rows["items"][2]["hash"],
        "linked hash chain"
    );
    let page = get(&s, OPERATOR, "/v1/admin/audit?limit=2").await.json();
    assert_eq!(page["next"], 2);
    let rest = get(&s, OPERATOR, "/v1/admin/audit?limit=2&before=2")
        .await
        .json();
    assert_eq!(rest["items"].as_array().map(Vec::len), Some(1));
    assert!(rest["next"].is_null());
    s.fake.tamper_audit(2);
    let bad = get(&s, OPERATOR, "/v1/admin/audit/verify").await.json();
    assert_eq!(
        (bad["ok"].as_bool(), bad["broken_at"].as_u64()),
        (Some(false), Some(2))
    );
}

#[tokio::test]
async fn repeating_the_same_decision_returns_the_existing_result_but_the_opposite_is_refused() {
    let path = "/v1/admin/approvals/ap-1/decide";
    let s = start(Fake::demo()).await;
    assert_eq!(
        post(&s, OPERATOR, path, &decide_body(true)).await.status,
        200
    );
    let again = post(&s, OPERATOR, path, &decide_body(true)).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    let opposite = post(&s, OPERATOR, path, &decide_body(false)).await;
    assert_eq!(
        (opposite.status, opposite.json()["error"].as_str()),
        (409, Some("stale"))
    );
    let wrong_hash = format!(r#"{{"allow":true,"seen":"{}"}}"#, "0".repeat(64));
    assert_eq!(
        post(&s, OPERATOR, path, &wrong_hash).await.status,
        409,
        "replay with a different seen hash must be refused"
    );
    assert_eq!(s.fake.decisions().len(), 1, "broker called once");
    let s = start(Fake::demo()).await;
    assert_eq!(
        post(&s, OPERATOR, path, &decide_body(false)).await.status,
        200
    );
    let again = post(&s, OPERATOR, path, &decide_body(false)).await;
    assert_eq!(
        (again.status, again.json()["approval"]["state"].as_str()),
        (200, Some("denied"))
    );
    assert_eq!(
        post(&s, OPERATOR, path, &decide_body(true)).await.status,
        409
    );
    assert_eq!(s.fake.decisions().len(), 1);
    // Only the original decider receives the idempotent replay result.
    s.fake
        .edit(|d| d.approvals[0].decided_by = Some(bot_admin::fake::pid("local:guest")));
    assert_eq!(
        post(&s, OPERATOR, path, &decide_body(false)).await.status,
        409
    );
}

#[tokio::test]
async fn the_audit_page_has_a_next_cursor_only_when_more_rows_exist() {
    let s = start(Fake::demo()).await;
    post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &decide_body(true),
    )
    .await;
    let total = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    assert_eq!(total["items"].as_array().map(Vec::len), Some(3));
    assert!(total["next"].is_null());
    let exact = get(&s, OPERATOR, "/v1/admin/audit?limit=3").await.json();
    assert_eq!(exact["items"].as_array().map(Vec::len), Some(3));
    assert!(
        exact["next"].is_null(),
        "exact-fit page must not emit a next cursor"
    );
    let part = get(&s, OPERATOR, "/v1/admin/audit?limit=2").await.json();
    assert_eq!(
        (
            part["items"].as_array().map(Vec::len),
            part["next"].as_u64()
        ),
        (Some(2), Some(2))
    );
    let rest = get(&s, OPERATOR, "/v1/admin/audit?limit=2&before=2")
        .await
        .json();
    assert_eq!(
        (
            rest["items"].as_array().map(Vec::len),
            rest["next"].is_null()
        ),
        (Some(1), true)
    );
    let one = get(&s, OPERATOR, "/v1/admin/audit?limit=1").await.json();
    assert_eq!(
        (one["items"].as_array().map(Vec::len), one["next"].as_u64()),
        (Some(1), Some(3))
    );
}

#[tokio::test]
async fn the_audit_page_is_exact_at_the_maximum_page_size_too() {
    let s = start(Fake::demo()).await;
    let add = |n: usize| {
        s.fake.edit(|d| {
            let (p, a) = (
                bot_core::id::ProfileId("personal".into()),
                bot_admin::fake::pid("service:morning"),
            );
            for _ in 0..n {
                d.log(Some(&p), &a, "tool.call", "agenda.list", None, "allow");
            }
        })
    };
    add(198); // 200 rows total
    let full = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    assert_eq!(full["items"].as_array().map(Vec::len), Some(200));
    assert!(
        full["next"].is_null(),
        "no next cursor when final page has exactly 200 rows"
    );
    add(1); // 201 rows total
    let page = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    assert_eq!(page["items"].as_array().map(Vec::len), Some(200));
    let next = page["next"].as_u64().expect("next cursor");
    let rest = get(
        &s,
        OPERATOR,
        &format!("/v1/admin/audit?limit=200&before={next}"),
    )
    .await
    .json();
    assert_eq!(
        (
            rest["items"].as_array().map(Vec::len),
            rest["next"].is_null()
        ),
        (Some(1), true)
    );
}

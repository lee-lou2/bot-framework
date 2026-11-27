//! 판정: 승인은 브로커 경유·본인만, 보류 판정은 운영자만, 둘 다 CSRF 방어와 감사 체인을 지난다.

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
    // 다시 누르면 새로 소비하지 않고 기존 결과를 돌려준다.
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
    // 감사 체인에 판정 주체가 남는다.
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
    // 지정 승인자가 아닌 운영자도 대신 누를 수 없다: 승인자를 다른 사람으로 바꿔 본다.
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
    assert!(s.fake.decisions().is_empty(), "브로커까지 가지 않는다");
    // 화면이 본 해시와 지금 값이 다르면 거절한다.
    s.fake
        .edit(|d| d.approvals[0].approver = bot_admin::fake::pid("local:owner"));
    let wrong = format!(r#"{{"allow":true,"seen":"{}"}}"#, "0".repeat(64));
    let r = post(&s, OPERATOR, "/v1/admin/approvals/ap-1/decide", &wrong).await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("changed"))
    );
    // 위험 상향으로 무효화된 승인은 소비할 수 없다.
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
    assert_eq!(r.status, 404, "보이지 않는 승인은 존재를 숨긴다");
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
            "토큰 없음",
            vec![auth, ("Content-Type", "application/json")],
        ),
        (
            "틀린 토큰",
            vec![
                auth,
                ("Content-Type", "application/json"),
                ("X-Bot-Csrf", "0000"),
            ],
        ),
        (
            "JSON 아님",
            vec![auth, ("Content-Type", "text/plain"), ("X-Bot-Csrf", &tok)],
        ),
        (
            "다른 출처",
            vec![
                auth,
                ("Content-Type", "application/json"),
                ("X-Bot-Csrf", &tok),
                ("Origin", "http://evil.example"),
            ],
        ),
        (
            "교차 사이트",
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
        "막힌 요청은 브로커에 닿지 않는다"
    );
    // 다른 신원의 토큰은 쓸 수 없다.
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
    // 같은 출처 표시가 있으면 통과한다.
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

/// 감사 기록의 수(최대 200).
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
    // 영수증 원문은 보존하지 않는다: 판정 종류와 지문만 남는다.
    assert_eq!(
        (rs[0].verdict, rs[0].digest),
        ("confirmed", bot_admin::fake::digest("msg-9"))
    );
    // 알림 단계와 원래 업로드 영수증이 확정됐다. 목록에는 호출 전체 보류가 남는다.
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
    // 같은 보류 식별자로 같은 운영자·같은 판정을 다시 보내면 새로 기록하지 않고 «반복»으로 답한다(감사도 더하지 않는다).
    let again = post(&s, OPERATOR, path, body).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    assert_eq!(s.fake.resolutions().len(), 1);
    assert_eq!(
        audit_count(&s).await,
        Some(3),
        "처음 판정의 감사 한 줄만 늘었다(demo 2 + 1)"
    );
    let a = get(&s, OPERATOR, "/v1/admin/audit?limit=1").await.json();
    assert_eq!(
        (
            a["items"][0]["action"].as_str(),
            a["items"][0]["verdict"].as_str()
        ),
        (Some("hold.resolve"), Some("confirmed"))
    );
    // 같은 식별자에 다른 값(영수증)이면 반복이 아니라 충돌이다.
    let other =
        r#"{"verdict":"confirmed","receipt":"other-receipt","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, other).await.status, 409);
    // 같은 식별자에 다른 판정 종류여도 충돌이다.
    let kind = r#"{"verdict":"not_executed","evidence":"msg-9","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, kind).await.status, 409);
    // 같은 식별자에 다른 대상(다른 단계)이면 충돌이다.
    let target = r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-1","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, target).await.status, 409);
    // 처음 보는 식별자는 없는 보류다.
    let unknown =
        r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-2","hold_id":"h-unknown"}"#;
    assert_eq!(post(&s, OPERATOR, path, unknown).await.status, 404);
    assert_eq!(s.fake.resolutions().len(), 1);
    assert_eq!(
        audit_count(&s).await,
        Some(3),
        "거절된 요청은 감사를 더하지 않는다"
    );
}

#[tokio::test]
async fn not_executed_resumes_the_run_and_bad_verdicts_are_refused() {
    let fake = Fake::demo();
    // 모든 단계가 미실행인 경우의 재개. 확정 단계가 있는 경우는 holds 시험에서 다룬다.
    fake.edit(|d| d.effects.retain(|e| e.effect.0 != "e-1"));
    let s = start(fake).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    // 각 본문은 보류 식별자를 갖추고도 다른 결함 하나 때문에 거절돼야 한다(식별자가 없으면 그것만으로 400이다).
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
        // 보류 식별자: 없거나 모양이 틀리면 거절한다.
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
        r#"{"verdict":"not_executed","evidence":"수신처 조회에서 해당 요청 없음 확인","effect":"e-2","hold_id":"h-e-2"}"#,
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
        .expect("행")
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert_eq!(seqs, [3, 2, 1], "최신순");
    assert_eq!(
        rows["items"][1]["prev"], rows["items"][2]["hash"],
        "체인으로 이어진다"
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
    // 승인 뒤: 같은 승인·같은 해시는 기존 결과, 거절은 막힌다.
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
        "다른 내용을 본 재전송은 기존 결과를 주지 않는다"
    );
    assert_eq!(s.fake.decisions().len(), 1, "브로커는 한 번만 불린다");
    // 거절 뒤도 대칭이다.
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
    // 결정한 사람이 아닌 이는 기존 결과를 받지 못한다.
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
    .await; // 감사 3건
    let total = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    assert_eq!(total["items"].as_array().map(Vec::len), Some(3));
    assert!(total["next"].is_null());
    // 정확히 가득 찬 쪽에는 다음 쪽이 없다.
    let exact = get(&s, OPERATOR, "/v1/admin/audit?limit=3").await.json();
    assert_eq!(exact["items"].as_array().map(Vec::len), Some(3));
    assert!(
        exact["next"].is_null(),
        "더 없는데 «더 보기»가 나오면 안 된다"
    );
    // 더 있으면 마지막 행의 순번이 다음 기준이다.
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
    let s = start(Fake::demo()).await; // 감사 2건
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
    add(198); // 정확히 200건
    let full = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    assert_eq!(full["items"].as_array().map(Vec::len), Some(200));
    assert!(
        full["next"].is_null(),
        "마지막 쪽이 정확히 200건이면 다음 쪽은 없다"
    );
    add(1); // 201건
    let page = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json();
    assert_eq!(page["items"].as_array().map(Vec::len), Some(200));
    let next = page["next"].as_u64().expect("다음 쪽이 있다");
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

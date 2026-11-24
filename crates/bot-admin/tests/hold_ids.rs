//! 보류 식별자: 판정은 «첫 확인에서 본 바로 그 보류»에만 적용된다. 응답을 잃고 다시 보내거나 같은 대상이 다시 보류돼도
//! 옛 판정이 새 보류에 닿지 않고, 같은 판정의 반복은 새로 적용하지 않는다.

mod support;
use bot_admin::{FenceFuture, fake::Fake};
use bot_core::{id::HoldId, state::RunState};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::SeqCst},
};
use support::*;

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
        "여러 단계의 묶음에는 식별자가 없다"
    );
    assert_eq!(
        step(&list, "e-2")["hold_id"],
        "h-e-2",
        "판정할 수 있는 단계가 자기 식별자를 가진다"
    );
    assert!(
        step(&list, "e-1")["hold_id"].is_null(),
        "확정된 단계는 판정할 수 없어 식별자가 없다"
    );
    // 실행 상세의 효과에도 같은 값이 보인다.
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    let ids: Vec<(&str, Option<&str>)> = d["effects"]
        .as_array()
        .expect("효과")
        .iter()
        .map(|e| (e["effect"].as_str().unwrap_or(""), e["hold_id"].as_str()))
        .collect();
    assert_eq!(ids, [("e-1", None), ("e-2", Some("h-e-2"))]);
    // 판정하면 그 단계의 식별자는 더 제안되지 않는다.
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

/// 실제 HTTP 흐름: H1 판정이 커밋된 뒤 응답을 잃고(503), 같은 단계가 다시 보류돼(H2) 옛 요청을 다시 보낸다.
#[tokio::test]
async fn a_retry_of_a_committed_judgment_repeats_it_and_never_touches_the_renewed_hold() {
    // 훅: 첫 읽기(csrf)와 판정의 핸들러 전 확인은 통과, 판정의 응답 직전 확인에서 닫힌다(3번째). 이후는 모두 통과.
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let s = start_cfg(Fake::demo(), move |cfg| {
        cfg.fence(move || -> FenceFuture {
            let n = c.fetch_add(1, SeqCst);
            Box::pin(async move { n != 2 })
        })
    })
    .await;
    let csrf = csrf(&s, OPERATOR).await; // 확인 1번째
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
        "커밋된 뒤 응답을 잃었다"
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (1, 1),
        "H1은 한 번 적용됐다"
    );
    // 같은 대상이 다시 보류된다: 새 식별자 H2.
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
    // H1 재전송: 새로 적용하지 않고 «반복»으로 답한다. H2는 건드리지 않는다.
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
        "H2는 미결로 남는다"
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (1, 1),
        "반복은 판정도 감사도 더하지 않는다"
    );
    assert_eq!(
        s.fake
            .edit(|d| d.runs.iter().find(|r| r.run.0 == "r-4").map(|r| r.state)),
        Some(RunState::Waiting),
        "H2가 미결이라 run은 계속 기다린다"
    );
    // H2의 새 판정만 두 번째 전이를 만든다.
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
    s.fake.edit(|d| d.rehold("c-9", Some("e-2"))); // 새 보류가 있어도 옛 식별자의 규칙은 같다
    for (why, body) in [
        ("다른 영수증", confirm("other", "h-e-2")),
        (
            "다른 판정 종류",
            r#"{"verdict":"not_executed","evidence":"msg-9","effect":"e-2","hold_id":"h-e-2"}"#
                .to_owned(),
        ),
        (
            "다른 단계",
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
    // 같은 값이면 반복이다(공백만 다른 입력도 같은 값으로 본다).
    let same = post(&s, OPERATOR, PATH, &confirm("  msg-9  ", "h-e-2")).await;
    assert_eq!(
        (same.status, same.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    assert_eq!(
        (s.fake.resolutions().len(), audit_resolves(&s).await),
        (1, 1)
    );
    // 처음 보는 식별자는 없는 보류다.
    assert_eq!(
        post(&s, OPERATOR, PATH, &confirm("msg-9", "h-nobody"))
            .await
            .status,
        404
    );
    // 운영자가 아니면 식별자를 맞춰도 거절이다(반복도 판정도 운영자만).
    assert_eq!(
        post(&s, GUEST, PATH, &confirm("msg-9", "h-e-2"))
            .await
            .status,
        403
    );
}

#[tokio::test]
async fn a_call_wide_hold_has_its_own_id_and_repeats_after_it_is_held_again() {
    use bot_admin::{
        fake::run_view,
        port::{HoldKind, HoldView, Waiting},
    };
    use bot_core::id::{CallId, ProfileId, RunId, ToolId};
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
        format!(r#"{{"verdict":"not_executed","evidence":"수신처에 기록 없음","hold_id":"{id}"}}"#)
    };
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let mine = list["items"]
        .as_array()
        .expect("목록")
        .iter()
        .find(|h| h["call"] == "c-q")
        .cloned()
        .expect("보류");
    assert_eq!(mine["hold_id"], "h-c-q");
    assert_eq!(post(&s, OPERATOR, path, &body("h-c-q")).await.status, 200);
    let h2 = s.fake.edit(|d| d.rehold("c-q", None));
    // 옛 식별자의 재전송은 반복이고 새 보류는 미결이다.
    let again = post(&s, OPERATOR, path, &body("h-c-q")).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let renewed = list["items"]
        .as_array()
        .expect("목록")
        .iter()
        .find(|h| h["call"] == "c-q")
        .cloned()
        .expect("새 보류");
    assert_eq!(renewed["hold_id"].as_str(), Some(h2.0.as_str()));
    assert_eq!(audit_resolves(&s).await, 1);
    // 새 식별자로 판정하면 새로 적용된다.
    let second = post(&s, OPERATOR, path, &body(&h2.0)).await;
    assert_eq!(
        (second.status, second.json()["repeated"].as_bool()),
        (200, Some(false))
    );
    assert_eq!(audit_resolves(&s).await, 2);
}

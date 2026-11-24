//! 같은 호출 ID를 쓰는 보류가 여럿일 때 서로 섞이지 않는다: 구판 데이터는 같은 프로필·run·호출 ID 아래 도구와 회신을 따로 보류할 수 있고,
//! 다른 run이 같은 호출 ID를 쓸 수도 있다. 보류는 보류 식별자(또는 프로필·run·호출·종류)로만 찾는다.

mod support;
use bot_admin::{
    fake::{Fake, approval_view, call_view, digest, run_view},
    json::hex,
    port::HoldKind,
};
use bot_core::{
    admin::CallState,
    id::{CallId, ProfileId, RunId},
    state::{EffectState, Risk, RunState},
};
use serde_json::Value;
use support::{
    holdfx::{hold as hold_at, step_of as step_in, views, waiting_run as waiting_in},
    *,
};

/// 도구 보류(식별자 `h-t`)와 회신 보류(`h-r`)가 같은 run·호출 아래 함께 있다.
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

/// 같은 쌍인데 도구 보류는 두 단계를 가진다(회신 보류는 단계가 없다).
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
    format!(r#"{{"verdict":"not_executed","evidence":"수신처에 기록 없음","hold_id":"{id}"{at}}}"#)
}

async fn holds_of(s: &Server, call: &str) -> Vec<Value> {
    get(s, OPERATOR, "/v1/admin/holds").await.json()["items"]
        .as_array()
        .expect("목록")
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
        "목록은 둘이다"
    );
    let status = get(&s, OPERATOR, "/v1/admin/status").await.json();
    assert_eq!(
        status["profiles"]["personal"]["holds"], 3,
        "데모 보류 하나와 이 쌍 둘"
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
            "{first}만 판정했으니 {left} 보류가 남는다"
        );
        // 남은 보류도 자기 식별자로 판정된다.
        assert_eq!(
            post(&s, OPERATOR, PAIR_PATH, &body(second, None))
                .await
                .status,
            200
        );
        assert!(holds_of(&s, "c-pair").await.is_empty());
        let resolved = s.fake.resolutions();
        let ids: Vec<&str> = resolved.iter().map(|r| r.hold.hold_id.0.as_str()).collect();
        assert_eq!(ids, [first, second], "판정은 각자 한 번씩 적용됐다");
        // 한 보류의 식별자를 다시 보내면 그 보류의 반복이고 다른 보류는 건드리지 않는다.
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
    let tool = list.iter().find(|h| h["kind"] == "tool").expect("도구");
    assert_eq!(
        tool["steps"].as_array().map(Vec::len),
        Some(2),
        "단계는 도구 보류만 가진다"
    );
    let reply = list.iter().find(|h| h["kind"] == "reply").expect("회신");
    assert_eq!(reply["steps"].as_array().map(Vec::len), Some(0));
    // 회신 보류를 단계 없이 판정한다(같은 호출의 도구 보류가 단계를 가져도 형식 오류가 아니다).
    let r = post(&s, OPERATOR, PAIR_PATH, &body("h-r", None)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let list = holds_of(&s, "c-pair").await;
    assert_eq!(kinds(&list), [("tool", None)], "도구 보류는 그대로 남는다");
    let states: Vec<&str> = list[0]["steps"]
        .as_array()
        .expect("단계")
        .iter()
        .filter_map(|e| e["state"].as_str())
        .collect();
    assert_eq!(
        states,
        ["held", "held"],
        "회신 판정이 도구 단계를 건드리지 않았다"
    );
    // 도구 단계는 여전히 단계를 지정해야 한다.
    let bare = post(&s, OPERATOR, PAIR_PATH, &body("h-e-p1", None)).await;
    assert_eq!(bare.status, 400, "단계의 식별자만 보내면 형식 오류다");
    // 단계를 모두 판정하면 도구 보류가 사라진다.
    for e in ["e-p1", "e-p2"] {
        let r = post(&s, OPERATOR, PAIR_PATH, &body(&format!("h-{e}"), Some(e))).await;
        assert_eq!(r.status, 200, "{e}: {}", r.body);
    }
    assert!(holds_of(&s, "c-pair").await.is_empty());
}

/// 같은 프로필의 두 run이 같은 호출 ID `c-same`을 쓴다.
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
            "각 보류는 자기 run의 단계 하나만 가진다: {h}"
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
    assert_eq!(list.len(), 1, "r-b의 보류는 그대로다");
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
        "다른 run의 보류가 남아 있으니 그 run은 계속 기다린다"
    );
    // 다른 run의 경로로 보낸 식별자는 대상이 달라 충돌이다: 끝난 r-a의 식별자를 r-b 경로로, 열린 r-b의 식별자를 r-a 경로로.
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
    assert_eq!(list.len(), 1, "충돌한 요청은 아무것도 바꾸지 않는다");
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
    // 다른 run의 식별자를 이 경로로 보내면 충돌이다.
    let wrong = post(
        &s,
        OPERATOR,
        "/v1/admin/holds/personal/r-a/c-same/resolve",
        &body("h-r-b", None),
    )
    .await;
    assert_eq!(wrong.status, 409);
    assert_eq!(holds_of(&s, "c-same").await.len(), 1);
    // 다시 보류도 정확한 대상에만 적용된다: r-b를 다시 보류해도 r-a의 끝난 판정은 되살아나지 않는다.
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
    // 두 번째 run의 승인을 처리한다(첫 일치를 고르는 구현은 첫 run의 호출을 바꾼다).
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
        "다른 run의 같은 호출 ID는 그대로다"
    );
}

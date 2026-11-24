//! 보류 판정의 단계 검증: 요청한 단계가 그 호출의 보류 단계여야 하고, 모두 판정된 뒤에만 run을 재개한다.

mod support;
use bot_admin::fake::Fake;
use bot_core::state::RunState;
use support::*;

fn held(effects: &[(&str, &str)]) -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("호출");
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
            r#"{{"verdict":"not_executed","evidence":"수신처 조회로 미실행 확인","effect":"{effect}","hold_id":"h-{effect}"}}"#
        )
    };
    // 이 호출에 없는 단계, 이미 확정된 단계, 단계 지정 없는 다단계 판정은 거절하고 아무것도 바꾸지 않는다.
    assert_eq!(post(&s, OPERATOR, path, &ev("e-404")).await.status, 404);
    assert_eq!(
        post(&s, OPERATOR, path, &ev("e-1")).await.status,
        404,
        "이미 확정된 단계에는 보류 식별자가 없다: 없는 보류다"
    );
    // 보류 식별자는 맞지만 대상이 다른 요청(다른 단계·다른 호출)은 충돌이다.
    let wrong_step =
        r#"{"verdict":"not_executed","evidence":"x","effect":"e-1","hold_id":"h-e-2"}"#;
    assert_eq!(
        post(&s, OPERATOR, path, wrong_step).await.status,
        409,
        "식별자는 e-2의 것이다"
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
        "식별자는 c-9의 보류 것이다"
    );
    // 다단계 보류의 묶음에는 식별자가 없다: 단계를 지정하지 않으면 식별자가 있어도 거절한다.
    let none = r#"{"verdict":"not_executed","evidence":"x","hold_id":"h-e-2"}"#;
    assert_eq!(
        post(&s, OPERATOR, path, none).await.status,
        400,
        "다단계 보류는 단계를 지정해야 한다"
    );
    assert!(s.fake.resolutions().is_empty());
    let holds = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    assert_eq!(
        holds["items"].as_array().map(Vec::len),
        Some(1),
        "보류가 그대로 남는다"
    );
    let state = s
        .fake
        .edit(|d| d.runs.iter().find(|r| r.run.0 == "r-4").map(|r| r.state));
    assert_eq!(state, Some(RunState::Waiting));
    // 다른 호출의 단계는 이 호출에서 판정할 수 없다.
    s.fake.edit(|d| {
        let other = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-1")
            .cloned()
            .expect("호출");
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
async fn the_run_resumes_only_after_every_held_step_is_decided() {
    let s = start(held(&[("e-3", "archive")])).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    let first = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"confirmed","receipt":"msg-9","effect":"e-2","hold_id":"h-e-2"}"#,
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.body);
    // 한 단계만 판정했으니 나머지 보류가 남고 run은 계속 기다린다.
    let h = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let steps = h["items"][0]["steps"].as_array().expect("단계");
    let states: Vec<(&str, &str)> = steps
        .iter()
        .filter_map(|x| Some((x["step"].as_str()?, x["state"].as_str()?)))
        .collect();
    assert_eq!(
        states,
        [
            ("upload", "confirmed"),
            ("notify", "confirmed"),
            ("archive", "held")
        ]
    );
    let state = s.fake.edit(|d| {
        d.runs
            .iter()
            .find(|r| r.run.0 == "r-4")
            .map(|r| (r.state, r.waiting.is_some()))
    });
    assert_eq!(state, Some((RunState::Waiting, true)));
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/status").await.json()["profiles"]["personal"]["holds"],
        1
    );
    // 같은 단계를 다시 판정하면 거절한다.
    assert_eq!(
        post(
            &s,
            OPERATOR,
            path,
            r#"{"verdict":"confirmed","receipt":"again","effect":"e-2","hold_id":"h-e-2"}"#
        )
        .await
        .status,
        409
    );
    // 마지막 보류 단계를 판정하면 그때 재개한다.
    let last = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"not_executed","evidence":"수신처에 기록 없음","effect":"e-3","hold_id":"h-e-3"}"#,
    )
    .await;
    assert_eq!(last.status, 200);
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/holds").await.json()["items"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let state = s.fake.edit(|d| {
        d.runs
            .iter()
            .find(|r| r.run.0 == "r-4")
            .map(|r| (r.state, r.waiting.is_some()))
    });
    assert_eq!(state, Some((RunState::Ready, false)));
    assert_eq!(s.fake.resolutions().len(), 2);
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/status").await.json()["profiles"]["personal"]["holds"],
        0
    );
}

#[tokio::test]
async fn a_step_judged_not_executed_stops_being_selectable_while_the_others_stay_held() {
    let s = start(held(&[("e-3", "archive")])).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    let first = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"not_executed","evidence":"수신처 조회로 미실행 확인","effect":"e-2","hold_id":"h-e-2"}"#,
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.body);
    // 목록은 판정한 단계를 더 이상 보류로 보이지 않고(미결 단계만 보류), 다른 보류 단계와 run의 대기는 그대로다.
    let h = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let states: Vec<(&str, &str)> = h["items"][0]["steps"]
        .as_array()
        .expect("단계")
        .iter()
        .filter_map(|x| Some((x["effect"].as_str()?, x["state"].as_str()?)))
        .collect();
    assert_eq!(
        states,
        [("e-1", "confirmed"), ("e-2", "started"), ("e-3", "held")]
    );
    assert!(
        h["items"][0]["steps"][1]["reason"].is_null(),
        "판정한 단계에는 보류 사유가 남지 않는다"
    );
    let selectable: Vec<&str> = h["items"][0]["steps"]
        .as_array()
        .expect("단계")
        .iter()
        .filter(|x| x["state"] == "held")
        .filter_map(|x| x["effect"].as_str())
        .collect();
    assert_eq!(
        selectable,
        ["e-3"],
        "화면이 고를 수 있는 것은 미결 단계뿐이다"
    );
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
        "미결 단계가 남아 run은 계속 기다린다"
    );
    // 같은 단계를 다시(다른 화면에서) 판정하면 거절하고 남은 단계에는 아무 영향이 없다.
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
        "남은 단계에 영수증이 기록되지 않았다"
    );
    // 마지막 미결 단계까지 판정하면 그때 재개한다.
    assert_eq!(
        post(
            &s,
            OPERATOR,
            path,
            r#"{"verdict":"not_executed","evidence":"수신처에 기록 없음","effect":"e-3","hold_id":"h-e-3"}"#
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
    assert_eq!(state, Some((RunState::Ready, false)));
}

/// 보류 종류별 판정 결과가 화면 안내와 같아야 한다: 민감 DM은 본문이 없어 다시 실행하지 않고 끝난다.
fn kinds() -> Fake {
    use bot_admin::{
        fake::run_view,
        port::{HoldKind, HoldView},
    };
    use bot_core::id::{CallId, HoldId, ProfileId, RunId, ToolId};
    let fake = Fake::demo();
    fake.edit(|d| {
        for (run, call, tool, kind) in [
            ("r-p1", "private:r-p1", "inbox.receive", HoldKind::Private),
            ("r-p2", "private:r-p2", "inbox.receive", HoldKind::Private),
            ("r-rp", "c-rp", "reply", HoldKind::Reply),
            // 이름처럼 보이는 값이 있어도 저장소가 채운 kind가 우선한다: 아래 둘은 이름만 그렇다.
            ("r-t1", "private:r-t1", "reply", HoldKind::Tool),
            ("r-q1", "c-q1", "agenda.list", HoldKind::Private),
        ] {
            let mut r = run_view("personal", run, "main", "local:owner", RunState::Waiting);
            r.waiting = Some(bot_admin::port::Waiting::Hold(CallId(call.into())));
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
            .expect("목록")
            .iter()
            .find(|h| h["call"] == call)
            .map(|h| h["kind"].as_str().map(str::to_owned))
    };
    assert_eq!(kind("c-9"), Some(Some("tool".into())), "일반 도구 호출");
    assert_eq!(
        kind("private:r-p1"),
        Some(Some("private".into())),
        "민감 DM"
    );
    assert_eq!(kind("c-rp"), Some(Some("reply".into())), "회신");
}

#[tokio::test]
async fn a_sensitive_dm_hold_ends_instead_of_resuming() {
    let s = start(kinds()).await;
    let ev = |call: &str| {
        format!(
            r#"{{"verdict":"not_executed","evidence":"수신처에 기록 없음","hold_id":"h-{call}"}}"#
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
        "미실행이면 종료하고 새 요청이 필요하다(다시 실행하지 않는다)"
    );
    // 처리됨으로 확인하면 완료로 닫힌다.
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
    // 회신 보류는 미실행이면 같은 회신을 다시 보내려고 되돌린다(재개).
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
    // 판정된 보류는 목록에서 사라진다.
    let list = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let calls: Vec<&str> = list["items"]
        .as_array()
        .expect("목록")
        .iter()
        .filter_map(|h| h["call"].as_str())
        .collect();
    assert_eq!(
        calls,
        ["c-9", "private:r-t1", "c-q1"],
        "판정하지 않은 보류만 남는다"
    );
    // 이름 충돌 fixture 둘은 이 시나리오에서 건드리지 않았으므로 그대로 기다린다(run은 `(상태, 오류, 대기 중 여부)`).
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
            .expect("목록")
            .iter()
            .find(|h| h["call"] == call)
            .and_then(|h| h["kind"].as_str().map(str::to_owned))
    };
    // 호출 ID가 `private:`로 시작하고 도구 이름이 `reply`여도 kind가 Tool이면 도구 보류다.
    assert_eq!(kind("private:r-t1").as_deref(), Some("tool"));
    // 평범한 호출 ID·도구 이름이어도 kind가 Private이면 민감 DM 보류다.
    assert_eq!(kind("c-q1").as_deref(), Some("private"));
    let ev = |call: &str| {
        format!(
            r#"{{"verdict":"not_executed","evidence":"수신처에 기록 없음","hold_id":"h-{call}"}}"#
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
    // 이름은 민감 DM처럼 보이지만 도구 보류라 미실행이면 같은 호출을 다시 이어 간다(dead가 아니다).
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
    // 이름은 평범하지만 민감 DM 보류라 미실행이면 종료한다(다시 실행하지 않는다).
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
    // 판정 전: 보류 단계에도 확정 단계에도 표지가 없다.
    let before = steps(&s).await;
    assert!(
        before
            .as_array()
            .expect("단계")
            .iter()
            .all(|x| x["resolution"].is_null())
    );
    // 미실행 판정: 그 단계만 시작함 + 표지, 다른 보류 단계와 확정 단계는 표지가 없다.
    assert_eq!(
        post(
            &s,
            OPERATOR,
            path,
            r#"{"verdict":"not_executed","evidence":"수신처에 기록 없음","effect":"e-2","hold_id":"h-e-2"}"#
        )
        .await
        .status,
        200
    );
    let judged = steps(&s).await;
    let by = |id: &str| {
        judged
            .as_array()
            .expect("단계")
            .iter()
            .find(|x| x["effect"] == id)
            .expect("단계")
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
    assert!(by("e-1")["resolution"].is_null(), "확정 단계는 표지가 없다");
    // 같은 «시작함»이어도 새로 시작한 단계는 표지가 없다(두 상태를 구분한다).
    s.fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("호출");
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
        .expect("효과")
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
        .expect("효과")
        .iter()
        .find(|x| x["effect"] == "e-2")
        .expect("e-2")
        .clone();
    assert_eq!(
        (e2["state"].as_str(), e2["resolution"].as_str()),
        (Some("started"), Some("not_executed")),
        "실행 상세에도 같은 표지"
    );
    // 일꾼이 다시 시작하면 표지가 사라진다(재시작한 started).
    s.fake.edit(|d| d.restart_effect("personal", "e-2"));
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    let e2 = d["effects"]
        .as_array()
        .expect("효과")
        .iter()
        .find(|x| x["effect"] == "e-2")
        .expect("e-2")
        .clone();
    assert_eq!(
        (e2["state"].as_str(), e2["resolution"].is_null()),
        (Some("started"), true)
    );
}

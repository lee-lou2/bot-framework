use super::*;

#[tokio::test]
async fn the_run_waits_for_a_whole_receipt_after_confirmed_steps() {
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
    // 마지막 미결 단계 판정으로 호출 전체의 영수증을 받을 별도 발생이 열린다.
    let last = post(
        &s,
        OPERATOR,
        path,
        r#"{"verdict":"not_executed","evidence":"수신처에 기록 없음","effect":"e-3","hold_id":"h-e-3"}"#,
    )
    .await;
    assert_eq!(last.status, 200);
    let held = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let hold = &held["items"][0];
    let id = hold["hold_id"].as_str().expect("호출 전체 보류");
    assert_ne!(id, "h-e-3");
    assert_eq!(hold["steps"][0]["receipt"], "file-17");
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/status").await.json()["profiles"]["personal"]["holds"],
        1
    );
    let body = serde_json::json!({"verdict":"not_executed", "evidence":"없음", "hold_id":id});
    assert_eq!(
        post(&s, OPERATOR, path, &body.to_string()).await.status,
        400
    );
    let body =
        serde_json::json!({"verdict":"confirmed", "receipt":"whole-10", "hold_id":id}).to_string();
    let final_decision = post(&s, OPERATOR, path, &body).await;
    assert_eq!(final_decision.status, 200, "{}", final_decision.body);
    assert_eq!(
        post(&s, OPERATOR, path, &body).await.json()["repeated"],
        true
    );
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
    assert_eq!(s.fake.resolutions().len(), 3);
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/status").await.json()["profiles"]["personal"]["holds"],
        0
    );
}

#[tokio::test]
async fn a_final_receipt_for_a_cancelled_run_only_settles_its_call() {
    let s = start(held(&[])).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    s.fake.edit(|d| {
        let r = d.runs.iter_mut().find(|r| r.run.0 == "r-4").unwrap();
        (r.state, r.waiting) = (RunState::Cancelled, None);
    });
    let step = r#"{"verdict":"confirmed","receipt":"notified","effect":"e-2","hold_id":"h-e-2"}"#;
    assert_eq!(post(&s, OPERATOR, path, step).await.status, 200);
    let held = get(&s, OPERATOR, "/v1/admin/holds").await.json();
    let id = held["items"][0]["hold_id"]
        .as_str()
        .expect("호출 전체 보류");
    let body = serde_json::json!({"verdict":"confirmed","receipt":"whole","hold_id":id});
    assert_eq!(
        post(&s, OPERATOR, path, &body.to_string()).await.status,
        200
    );
    let state = s
        .fake
        .edit(|d| d.runs.iter().find(|r| r.run.0 == "r-4").unwrap().state);
    assert_eq!(state, RunState::Cancelled);
}

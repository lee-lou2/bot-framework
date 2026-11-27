//! 조회: 계보·자식·도구 호출·효과·보류·승인·감사. 범위(운영자/주체)와 입력 검증.

use crate::support::*;
use bot_admin::fake::Fake;

#[tokio::test]
async fn lineage_children_calls_and_effects_come_from_one_snapshot() {
    let s = start(Fake::demo()).await;
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-2")
        .await
        .json();
    assert_eq!(d["run"]["parent"], "r-1");
    assert_eq!(d["run"]["step"], "lookup");
    let lineage: Vec<&str> = d["lineage"]
        .as_array()
        .expect("계보")
        .iter()
        .filter_map(|r| r["run"].as_str())
        .collect();
    assert_eq!(lineage, ["r-1", "r-2"], "루트부터 자기까지");
    assert_eq!(d["calls"][0]["tool"], "agenda.list");
    assert_eq!(d["calls"][0]["state"], "done");

    let root = get(&s, OPERATOR, "/v1/admin/runs/personal/r-1")
        .await
        .json();
    assert_eq!(root["children"][0]["run"], "r-2");
    assert_eq!(root["children"][0]["agent"], "main/research");
    assert_eq!(root["run"]["waiting"]["on"], "approval");
    assert_eq!(root["calls"][0]["risk"], "irreversible");
    assert_eq!(
        root["calls"][0]["args_sha256"].as_str().map(str::len),
        Some(64)
    );
}

#[tokio::test]
async fn shows_held_receipt() {
    let s = start(Fake::demo()).await;
    let h = get(&s, OPERATOR, "/v1/admin/holds").await;
    assert_eq!(h.status, 200);
    let item = &h.json()["items"][0];
    assert_eq!(
        (item["run"].as_str(), item["call"].as_str()),
        (Some("r-4"), Some("c-9"))
    );
    assert_eq!(item["reason"], "outcome_unknown");
    let steps = item["steps"].as_array().expect("단계");
    // 업로드는 원래 영수증을 보존하고 알림만 보류로 남는다.
    assert_eq!(
        (
            steps[0]["step"].as_str(),
            steps[0]["state"].as_str(),
            steps[0]["receipt"].as_str()
        ),
        (Some("upload"), Some("confirmed"), Some("file-17"))
    );
    assert_eq!(
        (steps[1]["step"].as_str(), steps[1]["state"].as_str()),
        (Some("notify"), Some("held"))
    );
    assert!(steps[1]["receipt"].is_null());
    // 같은 값이 run 상세의 효과에도 보인다.
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    assert_eq!(d["run"]["waiting"]["call"], "c-9");
    assert_eq!(d["effects"][0]["receipt"], "file-17");
    assert_eq!(d["calls"][0]["state"], "held");
}

#[tokio::test]
async fn run_lists_filter_by_profile_state_root_and_page() {
    let s = start(Fake::demo()).await;
    let all = get(&s, OPERATOR, "/v1/admin/runs?limit=200").await.json();
    assert_eq!(all["items"].as_array().map(Vec::len), Some(6));
    let work = get(&s, OPERATOR, "/v1/admin/runs?profile=work")
        .await
        .json();
    assert_eq!(work["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(work["items"][0]["run"], "r-w1");
    let dead = get(&s, OPERATOR, "/v1/admin/runs?state=dead").await.json();
    assert_eq!(
        (
            dead["items"][0]["run"].as_str(),
            dead["items"][0]["error"].as_str()
        ),
        (Some("r-5"), Some("tool_failed"))
    );
    let tree = get(&s, OPERATOR, "/v1/admin/runs?root=r-1").await.json();
    assert_eq!(tree["items"].as_array().map(Vec::len), Some(2));
    let first = get(&s, OPERATOR, "/v1/admin/runs?limit=4").await.json();
    assert_eq!(first["items"].as_array().map(Vec::len), Some(4));
    let cursor = first["next"].as_str().expect("다음 쪽");
    let second = get(
        &s,
        OPERATOR,
        &format!("/v1/admin/runs?limit=4&after={cursor}"),
    )
    .await
    .json();
    assert_eq!(second["items"].as_array().map(Vec::len), Some(2));
    assert!(second["next"].is_null());
}

#[tokio::test]
async fn a_member_sees_only_their_own_requests_and_approvals() {
    let s = start(Fake::demo()).await;
    s.fake.edit(|d| {
        let mut r = bot_admin::fake::run_view(
            "personal",
            "r-g1",
            "main",
            "local:guest",
            bot_core::state::RunState::Succeeded,
        );
        r.root = r.run.clone();
        d.runs.push(r);
    });
    let mine = get(&s, GUEST, "/v1/admin/runs").await.json();
    let ids: Vec<&str> = mine["items"]
        .as_array()
        .expect("목록")
        .iter()
        .filter_map(|r| r["run"].as_str())
        .collect();
    assert_eq!(ids, ["r-g1"]);
    // 남의 run은 존재 여부도 알리지 않는다(404).
    assert_eq!(
        get(&s, GUEST, "/v1/admin/runs/personal/r-1").await.status,
        404
    );
    assert_eq!(
        get(&s, GUEST, "/v1/admin/runs/personal/r-g1").await.status,
        200
    );
    let ap = get(&s, GUEST, "/v1/admin/approvals").await.json();
    assert_eq!(ap["items"].as_array().map(Vec::len), Some(0));
    let mine_ap = get(&s, OPERATOR, "/v1/admin/approvals").await.json();
    assert_eq!(mine_ap["items"][0]["nonce"], "ap-1");
    assert_eq!(mine_ap["items"][0]["render"][0]["label"], "대상");
    for path in [
        "/v1/admin/holds",
        "/v1/admin/audit",
        "/v1/admin/audit/verify",
        "/v1/admin/status",
    ] {
        let r = get(&s, GUEST, path).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (403, Some("denied")),
            "{path}"
        );
    }
}

#[tokio::test]
async fn bad_inputs_get_one_fixed_word() {
    let s = start(Fake::demo()).await;
    for path in [
        "/v1/admin/runs?profile=Personal",
        "/v1/admin/runs?profile=a&profile=b",
        "/v1/admin/runs?state=Ready",
        "/v1/admin/runs?limit=0",
        "/v1/admin/runs?limit=201",
        "/v1/admin/runs?after=a%20b",
        "/v1/admin/runs?other=1",
        "/v1/admin/runs/personal/a%2Fb",
        "/v1/admin/runs/Personal/r-1",
        "/v1/admin/approvals?state=done",
        "/v1/admin/audit?before=-1",
        "/v1/admin/holds?profile=x_y",
    ] {
        let r = get(&s, OPERATOR, path).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (400, Some("invalid")),
            "{path}"
        );
        assert!(
            !r.body.contains("Personal"),
            "입력 원문을 되돌려 주지 않는다: {path}"
        );
    }
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/runs/personal/r-404")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn status_counts_runs_approvals_and_holds_per_profile() {
    let s = start(Fake::demo()).await;
    let st = get(&s, OPERATOR, "/v1/admin/status").await.json();
    let p = &st["profiles"]["personal"];
    assert_eq!(p["approvals_pending"], 1);
    assert_eq!(p["holds"], 1);
    assert_eq!(p["runs"]["waiting"], 2);
    assert_eq!(p["runs"]["dead"], 1);
    assert_eq!(st["profiles"]["work"]["runs"]["succeeded"], 1);
    assert_eq!(p["obs"]["enabled"], false);
    assert!(st["process"]["uptime_secs"].is_u64());
}

#[tokio::test]
async fn a_failed_call_shows_a_fixed_cause_word_and_a_clean_call_shows_none() {
    use bot_admin::fake::call_view;
    use bot_core::{admin::CallState, state::Risk};
    let s = start(Fake::demo()).await;
    s.fake.edit(|d| {
        let run = d
            .runs
            .iter()
            .find(|r| r.run.0 == "r-5")
            .cloned()
            .expect("run");
        d.calls.push(call_view(
            &run,
            "c-f",
            "agenda.list",
            Risk::Read,
            CallState::Failed,
        ));
        d.calls.push(call_view(
            &run,
            "c-d",
            "agenda.delete",
            Risk::Irreversible,
            CallState::Denied,
        ));
    });
    let d = get(&s, OPERATOR, "/v1/admin/runs/personal/r-5")
        .await
        .json();
    let by = |id: &str| {
        d["calls"]
            .as_array()
            .expect("호출")
            .iter()
            .find(|c| c["call"] == id)
            .expect("호출")
            .clone()
    };
    assert_eq!(
        (by("c-f")["state"].as_str(), by("c-f")["error"].as_str()),
        (Some("failed"), Some("execution_failed"))
    );
    assert_eq!(
        (by("c-d")["state"].as_str(), by("c-d")["error"].as_str()),
        (Some("denied"), Some("denied"))
    );
    // 보류된 호출은 결과 불명, 정상 호출은 원인이 없다(null).
    let held = get(&s, OPERATOR, "/v1/admin/runs/personal/r-4")
        .await
        .json();
    assert_eq!(held["calls"][0]["error"], "outcome_unknown");
    let clean = get(&s, OPERATOR, "/v1/admin/runs/personal/r-2")
        .await
        .json();
    assert!(clean["calls"][0]["error"].is_null());
    // 원문이 아니라 고정 낱말뿐이다.
    let words = [
        "denied",
        "timeout",
        "invalid_result",
        "outcome_unknown",
        "execution_failed",
    ];
    for r in ["r-2", "r-4", "r-5"] {
        let d = get(&s, OPERATOR, &format!("/v1/admin/runs/personal/{r}"))
            .await
            .json();
        for c in d["calls"].as_array().expect("호출") {
            assert!(
                c["error"].is_null() || words.contains(&c["error"].as_str().unwrap_or("")),
                "{c}"
            );
        }
    }
}

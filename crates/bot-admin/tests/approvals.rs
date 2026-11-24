//! 승인 목록: 필터(대기·처리됨·전체)와 정렬은 설계 계약을 따르고, 필터는 쪽 나누기 전에 적용한다.

mod support;
use bot_admin::fake::{Fake, approval_view, pid};
use bot_core::admin::ApprovalState;
use std::time::{Duration, SystemTime};
use support::*;

/// 대기 둘(만료 시각이 다름)과 처리된 넷(처리 시각이 있는 둘, 자동 만료·대체로 처리 시각이 없는 둘)을 더한 서버.
async fn busy() -> Server {
    let fake = Fake::demo();
    fake.edit(|d| {
        let run = d
            .runs
            .iter()
            .find(|r| r.run.0 == "r-1")
            .cloned()
            .expect("run");
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-1")
            .cloned()
            .expect("호출");
        let now = SystemTime::now();
        let mut add = |nonce: &str, state: ApprovalState, expires: u64, decided: Option<u64>| {
            let mut a = approval_view(&call, &run, nonce, "local:owner");
            a.state = state;
            a.expires_at = now + Duration::from_secs(expires);
            a.decided_at = decided.map(|ago| now - Duration::from_secs(ago));
            a.decided_by = decided.map(|_| pid("local:owner"));
            d.approvals.push(a);
        };
        add("ap-b", ApprovalState::Pending, 100, None); // 가장 먼저 만료되는 대기
        add("ap-d1", ApprovalState::Approved, 900, Some(100));
        add("ap-d2", ApprovalState::Denied, 900, Some(10)); // 가장 최근에 처리
        add("ap-y", ApprovalState::Superseded, 900, None);
        add("ap-x", ApprovalState::Expired, 900, None);
    });
    start(fake).await
}

fn nonces(v: &serde_json::Value) -> Vec<&str> {
    v["items"]
        .as_array()
        .expect("목록")
        .iter()
        .filter_map(|a| a["nonce"].as_str())
        .collect()
}

#[tokio::test]
async fn pending_is_the_default_and_comes_in_expiry_order() {
    let s = busy().await;
    let default = get(&s, OPERATOR, "/v1/admin/approvals").await.json();
    assert_eq!(nonces(&default), ["ap-b", "ap-1"], "만료 임박순");
    let explicit = get(&s, OPERATOR, "/v1/admin/approvals?state=pending")
        .await
        .json();
    assert_eq!(nonces(&explicit), nonces(&default));
}

#[tokio::test]
async fn decided_lists_every_non_pending_row_newest_first_with_undated_rows_last_by_nonce() {
    let s = busy().await;
    let decided = get(&s, OPERATOR, "/v1/admin/approvals?state=decided")
        .await
        .json();
    assert_eq!(
        nonces(&decided),
        ["ap-d2", "ap-d1", "ap-x", "ap-y"],
        "처리 시각 내림차순, 시각 없는 행(자동 만료·대체)은 뒤에 nonce 오름차순"
    );
    assert!(
        decided["items"]
            .as_array()
            .expect("목록")
            .iter()
            .all(|a| a["state"] != "pending")
    );
}

#[tokio::test]
async fn all_puts_processed_rows_first_and_pending_rows_last() {
    let s = busy().await;
    let all = get(&s, OPERATOR, "/v1/admin/approvals?state=all")
        .await
        .json();
    assert_eq!(
        nonces(&all),
        ["ap-d2", "ap-d1", "ap-x", "ap-y", "ap-b", "ap-1"]
    );
}

#[tokio::test]
async fn the_filter_applies_before_paging_so_pages_never_mix_in_pending_rows() {
    let s = busy().await;
    let first = get(&s, OPERATOR, "/v1/admin/approvals?state=decided&limit=3")
        .await
        .json();
    assert_eq!(nonces(&first), ["ap-d2", "ap-d1", "ap-x"]);
    let cursor = first["next"].as_str().expect("다음 쪽").to_owned();
    let second = get(
        &s,
        OPERATOR,
        &format!("/v1/admin/approvals?state=decided&limit=3&after={cursor}"),
    )
    .await
    .json();
    assert_eq!(nonces(&second), ["ap-y"]);
    assert!(
        second["next"].is_null(),
        "대기 행이 다음 쪽에 섞여 들어오지 않는다"
    );
}

#[tokio::test]
async fn a_member_sees_only_their_own_rows_under_every_filter_and_bad_filters_are_refused() {
    let s = busy().await;
    for state in ["pending", "decided", "all"] {
        let r = get(&s, GUEST, &format!("/v1/admin/approvals?state={state}"))
            .await
            .json();
        assert!(nonces(&r).is_empty(), "{state}: 남의 승인은 보이지 않는다");
    }
    for bad in ["done", "Decided", "", "pending,decided", "1"] {
        let r = get(&s, OPERATOR, &format!("/v1/admin/approvals?state={bad}")).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (400, Some("invalid")),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn cursors_are_sort_keys_so_a_row_that_changes_between_pages_is_never_repeated_or_skipped() {
    let s = busy().await;
    let all = |after: Option<&str>| match after {
        Some(c) => format!("/v1/admin/approvals?state=all&limit=3&after={c}"),
        None => "/v1/admin/approvals?state=all&limit=3".to_owned(),
    };
    let first = get(&s, OPERATOR, &all(None)).await.json();
    assert_eq!(nonces(&first), ["ap-d2", "ap-d1", "ap-x"]);
    let cursor = first["next"].as_str().expect("다음 쪽").to_owned();
    assert!(
        cursor.contains("ap-x"),
        "커서는 마지막 행의 정렬 키다(행 번호가 아니다): {cursor}"
    );
    // 쪽 사이에 대기 행 ap-b가 처리돼 목록의 맨 앞으로 옮겨 간다.
    s.fake.edit(|d| {
        if let Some(a) = d.approvals.iter_mut().find(|a| a.nonce.0 == "ap-b") {
            a.state = ApprovalState::Approved;
            a.decided_at = Some(SystemTime::now());
            a.decided_by = Some(pid("local:owner"));
        }
    });
    let second = get(&s, OPERATOR, &all(Some(&cursor))).await.json();
    assert_eq!(
        nonces(&second),
        ["ap-y", "ap-1"],
        "이미 본 행(ap-x)이 다시 나오거나 뒤의 행이 빠지지 않는다"
    );
    assert!(second["next"].is_null());
    // 처음부터 다시 보면 옮겨 간 행이 맨 앞에 있다.
    let fresh = get(&s, OPERATOR, &all(None)).await.json();
    assert_eq!(nonces(&fresh), ["ap-b", "ap-d2", "ap-d1"]);
}

#[tokio::test]
async fn pending_pages_follow_the_expiry_order_with_sort_key_cursors_too() {
    let s = busy().await;
    let one = get(&s, OPERATOR, "/v1/admin/approvals?limit=1")
        .await
        .json();
    assert_eq!(nonces(&one), ["ap-b"], "만료가 가장 임박한 대기");
    let cursor = one["next"].as_str().expect("다음 쪽").to_owned();
    let two = get(
        &s,
        OPERATOR,
        &format!("/v1/admin/approvals?limit=1&after={cursor}"),
    )
    .await
    .json();
    assert_eq!(nonces(&two), ["ap-1"]);
    assert!(two["next"].is_null());
}

#[tokio::test]
async fn run_pages_use_sort_key_cursors_as_well() {
    let s = start(Fake::demo()).await;
    let first = get(&s, OPERATOR, "/v1/admin/runs?limit=2").await.json();
    let seen: Vec<String> = first["items"]
        .as_array()
        .expect("목록")
        .iter()
        .filter_map(|r| r["run"].as_str().map(str::to_owned))
        .collect();
    let cursor = first["next"].as_str().expect("다음 쪽").to_owned();
    // 쪽 사이에 아직 보지 못한 run 하나가 가장 최근으로 갱신된다(앞으로 옮겨 가서 이미 지나간 쪽의 일이 된다).
    let moved = s.fake.edit(|d| {
        let r = d
            .runs
            .iter_mut()
            .rev()
            .find(|r| !seen.contains(&r.run.0))
            .expect("run");
        r.updated_at = SystemTime::now();
        r.run.0.clone()
    });
    let rest = get(
        &s,
        OPERATOR,
        &format!("/v1/admin/runs?limit=200&after={cursor}"),
    )
    .await
    .json();
    let ids: Vec<&str> = rest["items"]
        .as_array()
        .expect("목록")
        .iter()
        .filter_map(|r| r["run"].as_str())
        .collect();
    assert!(
        !ids.contains(&moved.as_str()),
        "앞으로 옮겨 간 행은 다음 쪽에 다시 나오지 않는다"
    );
    assert!(
        seen.iter().all(|s| !ids.contains(&s.as_str())),
        "이미 본 행이 겹치지 않는다"
    );
    assert_eq!(
        ids.len() + seen.len() + 1,
        6,
        "나머지는 하나도 빠지지 않는다"
    );
}

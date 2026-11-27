//! Approval listing: state filters, deterministic ordering, and sort-key cursor pagination.

use crate::support::*;
use bot_admin::fake::{Fake, approval_view, pid};
use bot_core::admin::ApprovalState;
use std::time::{Duration, SystemTime};

/// Server seeded with two pending rows (different expiries) and four non-pending rows (two dated, two undated).
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
            .expect("call");
        let now = SystemTime::now();
        let mut add = |nonce: &str, state: ApprovalState, expires: u64, decided: Option<u64>| {
            let mut a = approval_view(&call, &run, nonce, "local:owner");
            a.state = state;
            a.expires_at = now + Duration::from_secs(expires);
            a.decided_at = decided.map(|ago| now - Duration::from_secs(ago));
            a.decided_by = decided.map(|_| pid("local:owner"));
            d.approvals.push(a);
        };
        add("ap-b", ApprovalState::Pending, 100, None);
        add("ap-d1", ApprovalState::Approved, 900, Some(100));
        add("ap-d2", ApprovalState::Denied, 900, Some(10));
        add("ap-y", ApprovalState::Superseded, 900, None);
        add("ap-x", ApprovalState::Expired, 900, None);
    });
    start(fake).await
}

fn nonces(v: &serde_json::Value) -> Vec<&str> {
    v["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|a| a["nonce"].as_str())
        .collect()
}

#[tokio::test]
async fn pending_is_the_default_and_comes_in_expiry_order() {
    let s = busy().await;
    let default = get(&s, OPERATOR, "/v1/admin/approvals").await.json();
    assert_eq!(nonces(&default), ["ap-b", "ap-1"], "soonest expiry first");
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
        "decided_at desc, then undated rows ordered by nonce asc"
    );
    assert!(
        decided["items"]
            .as_array()
            .expect("items")
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
    let cursor = first["next"].as_str().expect("next cursor").to_owned();
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
        "pending rows must not spill into subsequent decided pages"
    );
}

#[tokio::test]
async fn a_member_sees_only_their_own_rows_under_every_filter_and_bad_filters_are_refused() {
    let s = busy().await;
    for state in ["pending", "decided", "all"] {
        let r = get(&s, GUEST, &format!("/v1/admin/approvals?state={state}"))
            .await
            .json();
        assert!(
            nonces(&r).is_empty(),
            "{state}: other principals' approvals must stay hidden"
        );
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
    let cursor = first["next"].as_str().expect("next cursor").to_owned();
    assert!(
        cursor.contains("ap-x"),
        "cursor must encode the last row's sort key, not an offset: {cursor}"
    );
    // Decide pending row ap-b between pages so it moves to the front of the list.
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
        "already-seen rows must not repeat and trailing rows must not be skipped"
    );
    assert!(second["next"].is_null());
    let fresh = get(&s, OPERATOR, &all(None)).await.json();
    assert_eq!(nonces(&fresh), ["ap-b", "ap-d2", "ap-d1"]);
}

#[tokio::test]
async fn pending_pages_follow_the_expiry_order_with_sort_key_cursors_too() {
    let s = busy().await;
    let one = get(&s, OPERATOR, "/v1/admin/approvals?limit=1")
        .await
        .json();
    assert_eq!(nonces(&one), ["ap-b"], "soonest-expiring pending row");
    let cursor = one["next"].as_str().expect("next cursor").to_owned();
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
        .expect("items")
        .iter()
        .filter_map(|r| r["run"].as_str().map(str::to_owned))
        .collect();
    let cursor = first["next"].as_str().expect("next cursor").to_owned();
    // Bump an unseen run to the front between pages.
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
        .expect("items")
        .iter()
        .filter_map(|r| r["run"].as_str())
        .collect();
    assert!(
        !ids.contains(&moved.as_str()),
        "row moved ahead of the cursor must not appear on the next page"
    );
    assert!(
        seen.iter().all(|s| !ids.contains(&s.as_str())),
        "previously seen rows must not repeat"
    );
    assert_eq!(
        ids.len() + seen.len() + 1,
        6,
        "all remaining rows must be accounted for"
    );
}

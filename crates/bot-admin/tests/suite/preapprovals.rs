//! Pre-approval admin API: list contract, pause/resume CAS and idempotency, percent-encoded agent segments, auth, and validation.

use crate::support::*;
use bot_admin::fake::Fake;

const NESTED: &str = "/v1/admin/preapprovals/personal/main%2Fdeveloper/digest";
const PLAIN: &str = "/v1/admin/preapprovals/personal/main/notify";

fn cas(grant: &str, epoch: i64) -> String {
    format!(r#"{{"grant_id":"{grant}","epoch":{epoch}}}"#)
}

#[tokio::test]
async fn the_list_has_exactly_the_contract_fields_and_types() {
    let s = start(Fake::demo()).await;
    let list = get(&s, OPERATOR, "/v1/admin/preapprovals").await;
    assert_eq!(list.status, 200);
    let body = list.json();
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    for row in items {
        let object = row.as_object().expect("row");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "agent",
                "epoch",
                "grant_id",
                "max_per_day",
                "paused",
                "profile",
                "trigger",
                "used_today"
            ]
        );
        for text in ["profile", "agent", "trigger", "grant_id"] {
            assert!(row[text].is_string(), "{text}");
        }
        for number in ["epoch", "used_today", "max_per_day"] {
            assert!(row[number].is_i64(), "{number} must be a JSON integer");
        }
        assert!(row["paused"].is_boolean());
    }
    let nested = items
        .iter()
        .find(|r| r["agent"] == "main/developer")
        .expect("nested agent");
    assert_eq!(
        (
            nested["paused"].clone(),
            nested["used_today"].clone(),
            nested["max_per_day"].clone()
        ),
        (true.into(), 0.into(), 8.into())
    );
    // Profile filtering is client-side; query parameters are rejected.
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/preapprovals?profile=personal")
            .await
            .status,
        400
    );
    let guest = get(&s, GUEST, "/v1/admin/preapprovals").await;
    assert_eq!(
        (guest.status, guest.json()["error"].clone()),
        (403, "denied".into())
    );
    assert_eq!(
        raw(s.addr, "GET", "/v1/admin/preapprovals", None, &[], None)
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn a_slash_in_the_agent_arrives_as_one_percent_encoded_segment() {
    let s = start(Fake::demo()).await;
    let done = post(
        &s,
        OPERATOR,
        &format!("{NESTED}/resume"),
        &cas("g-demo-digest", 1),
    )
    .await;
    assert_eq!(done.status, 200, "{}", done.body);
    let row = done.json();
    assert!(
        row.get("item").is_none() && row.get("ok").is_none(),
        "unwrapped row response"
    );
    assert_eq!(
        (
            row["agent"].clone(),
            row["paused"].clone(),
            row["epoch"].clone(),
            row["grant_id"].clone()
        ),
        (
            "main/developer".into(),
            false.into(),
            2.into(),
            "g-demo-digest".into()
        )
    );
    let loose = post(
        &s,
        OPERATOR,
        "/v1/admin/preapprovals/personal/main/developer/digest/pause",
        &cas("g-demo-digest", 2),
    )
    .await;
    assert_eq!(loose.status, 404);
    // Double-encoding (%252F) decodes once to literal `%2F`, which is not a valid agent ID.
    let double = post(
        &s,
        OPERATOR,
        "/v1/admin/preapprovals/personal/main%252Fdeveloper/digest/pause",
        &cas("g-demo-digest", 2),
    )
    .await;
    assert_eq!(double.status, 400);
    assert_eq!(
        s.fake.lock().pause_audits,
        1,
        "rejected requests do not audit"
    );
}

#[tokio::test]
async fn the_same_command_twice_is_one_change_and_stale_requests_are_409() {
    let s = start(Fake::demo()).await;
    let url = format!("{PLAIN}/pause");
    let first = post(&s, OPERATOR, &url, &cas("g-demo-notify", 0)).await;
    assert_eq!(first.status, 200);
    let row = first.json();
    assert_eq!(
        (row["paused"].clone(), row["epoch"].clone()),
        (true.into(), 1.into())
    );
    // Retrying after a lost response or targeting the current already-paused epoch is idempotent.
    let again = post(&s, OPERATOR, &url, &cas("g-demo-notify", 0)).await;
    assert_eq!((again.status, again.json()), (200, row.clone()));
    let same = post(&s, OPERATOR, &url, &cas("g-demo-notify", 1)).await;
    assert_eq!((same.status, same.json()), (200, row.clone()));
    assert_eq!(s.fake.lock().pause_audits, 1);
    // ABA: after resume -> pause, replaying epoch 0 returns 409 stale even though state is paused.
    let resume = format!("{PLAIN}/resume");
    assert_eq!(
        post(&s, OPERATOR, &resume, &cas("g-demo-notify", 1))
            .await
            .status,
        200
    );
    assert_eq!(
        post(&s, OPERATOR, &url, &cas("g-demo-notify", 2))
            .await
            .status,
        200
    );
    let aba = post(&s, OPERATOR, &url, &cas("g-demo-notify", 0)).await;
    assert_eq!(
        (aba.status, aba.json()["error"].clone()),
        (409, "stale".into())
    );
    let other = post(&s, OPERATOR, &url, &cas("g-old", 3)).await;
    assert_eq!(
        (other.status, other.json()["error"].clone()),
        (409, "stale".into())
    );
    assert_eq!(
        post(&s, OPERATOR, &url, &cas("g-demo-notify", 9))
            .await
            .status,
        409
    );
    let ghost = post(
        &s,
        OPERATOR,
        "/v1/admin/preapprovals/personal/main/ghost/pause",
        &cas("g-x", 0),
    )
    .await;
    assert_eq!(
        (ghost.status, ghost.json()["error"].clone()),
        (404, "not_found".into())
    );
    assert_eq!(
        s.fake.lock().pause_audits,
        3,
        "only pause, resume, pause are audited"
    );
    let list = get(&s, OPERATOR, "/v1/admin/preapprovals").await.json();
    let row = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["trigger"] == "notify")
        .unwrap()
        .clone();
    assert_eq!(
        (row["paused"].clone(), row["epoch"].clone()),
        (true.into(), 3.into())
    );
}

#[tokio::test]
async fn foreign_callers_missing_csrf_and_malformed_bodies_change_nothing() {
    let s = start(Fake::demo()).await;
    let url = format!("{PLAIN}/pause");
    let guest = post(&s, GUEST, &url, &cas("g-demo-notify", 0)).await;
    assert_eq!(
        (guest.status, guest.json()["error"].clone()),
        (403, "denied".into())
    );
    let bearer = format!("Bearer {OPERATOR}");
    let headers = [
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/json"),
    ];
    let missing = raw(
        s.addr,
        "POST",
        &url,
        None,
        &headers,
        Some(&cas("g-demo-notify", 0)),
    )
    .await;
    assert_eq!(missing.status, 403);
    let wrong = raw(
        s.addr,
        "POST",
        &url,
        None,
        &[headers[0], headers[1], ("X-Bot-Csrf", "nope")],
        Some(&cas("g-demo-notify", 0)),
    )
    .await;
    assert_eq!(wrong.status, 403);
    for (name, body) in [
        (
            "unknown key",
            r#"{"grant_id":"g-demo-notify","epoch":0,"x":1}"#.to_owned(),
        ),
        (
            "string epoch",
            r#"{"grant_id":"g-demo-notify","epoch":"0"}"#.to_owned(),
        ),
        (
            "float epoch",
            r#"{"grant_id":"g-demo-notify","epoch":0.5}"#.to_owned(),
        ),
        (
            "negative epoch",
            r#"{"grant_id":"g-demo-notify","epoch":-1}"#.to_owned(),
        ),
        (
            "missing epoch",
            r#"{"grant_id":"g-demo-notify"}"#.to_owned(),
        ),
        ("missing grant_id", r#"{"epoch":0}"#.to_owned()),
        ("empty grant_id", r#"{"grant_id":"","epoch":0}"#.to_owned()),
        ("not JSON", "pause".to_owned()),
        ("array", "[0]".to_owned()),
        (
            "oversized",
            format!(r#"{{"grant_id":"{}","epoch":0}}"#, "g".repeat(5000)),
        ),
    ] {
        let r = post(&s, OPERATOR, &url, &body).await;
        assert!(r.status == 400 || r.status == 413, "{name}: {}", r.status);
    }
    for path in [
        "/v1/admin/preapprovals/Personal/main/notify/pause",
        "/v1/admin/preapprovals/personal/Main/notify/pause",
        "/v1/admin/preapprovals/personal/main/NOTIFY/pause",
        "/v1/admin/preapprovals/personal/main%2F%2Fx/notify/pause",
        "/v1/admin/preapprovals/personal/main/notify/halt",
    ] {
        let r = post(&s, OPERATOR, path, &cas("g-demo-notify", 0)).await;
        assert!(matches!(r.status, 400 | 404), "{path}: {}", r.status);
    }
    assert_eq!(s.fake.lock().pause_audits, 0, "no state change occurred");
    let list = get(&s, OPERATOR, "/v1/admin/preapprovals").await.json();
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["trigger"] == "notify" && r["paused"] == false)
    );
}

#[tokio::test]
async fn a_double_click_in_flight_is_one_audit() {
    let s = start(Fake::demo()).await;
    let url = format!("{PLAIN}/pause");
    let body = cas("g-demo-notify", 0);
    let (a, b) = tokio::join!(
        post(&s, OPERATOR, &url, &body),
        post(&s, OPERATOR, &url, &body)
    );
    assert_eq!((a.status, b.status), (200, 200));
    assert_eq!(a.json(), b.json(), "identical response");
    assert_eq!(s.fake.lock().pause_audits, 1);
}

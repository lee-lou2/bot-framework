//! HTTP stale-process fencing: mismatched active set closes admissions and reads with 503.
use bot_core::state::RunState;
use bot_gateway::testing::{ports::MemoryActive, *};
use serde_json::json;
use std::time::Duration;

use crate::http_support::*;

async fn rig_checked(active: &std::sync::Arc<MemoryActive>) -> Rig {
    rig_active(
        vec![fixture()],
        MemorySecrets::new(&[("personal", "inbox-key", KEY)]),
        Some(active.check("set-1")),
        |_| {},
    )
    .await
}

#[tokio::test]
async fn a_stale_process_closes_new_requests_and_status_reads_with_503() {
    let active = MemoryActive::new(Some("set-1"));
    let rig = rig_checked(&active).await;
    let first = post(&rig, "/inbox/notify", "svc", "n1", json!({"to": "me"})).await;
    assert_eq!(first.status, 202, "{}", first.body);
    rig.store.set_state(
        &key_of("svc", "n1"),
        RunState::Succeeded,
        Some(r#"{"status":200,"body":{"sent":true}}"#),
    );
    assert_eq!(get(&rig, "/inbox/_requests/n1", "svc").await.status, 200);

    active.set(Some("set-2"));
    let status = get(&rig, "/inbox/_requests/n1", "svc").await;
    assert_eq!(status.status, 503, "{}", status.body);
    assert!(!status.body.contains("sent"), "result body must not leak");
    let new = post(&rig, "/inbox/notify", "svc", "n2", json!({"to": "me"})).await;
    assert_eq!(new.status, 503, "{}", new.body);
    assert_eq!(
        rig.store.accepted().len(),
        1,
        "stale process must not write to the store"
    );

    active.set(Some("set-1"));
    assert_eq!(get(&rig, "/inbox/_requests/n1", "svc").await.status, 200);
    assert_eq!(
        post(&rig, "/inbox/notify", "svc", "n2", json!({"to": "me"}))
            .await
            .status,
        202
    );
}

#[tokio::test]
async fn a_set_change_during_the_synchronous_wait_hides_the_result() {
    let active = MemoryActive::new(Some("set-1"));
    let rig = rig_checked(&active).await;
    let (store, flip) = (rig.store.clone(), active.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flip.set(Some("set-2"));
        store.set_state(
            &key_of("svc", "s1"),
            RunState::Succeeded,
            Some(r#"{"status":201,"body":{"id":"e-1"}}"#),
        );
    });
    let reply = post(&rig, "/inbox/agenda", "svc", "s1", json!({"event": "e-1"})).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(!reply.body.contains("e-1"), "result body must not leak");
    assert_eq!(
        rig.store.accepted().len(),
        1,
        "admission committed without creating a duplicate"
    );
}

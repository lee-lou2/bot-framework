//! 구판 프로세스 닫기(HTTP): 저장소의 활성 집합과 다른 프로세스는 새 접수·상태·결과를 503으로 닫고,
//! 같은 집합으로 돌아오면 같은 요청이 그대로 이어진다.
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

    // 다른 프로세스가 새 집합을 활성화했다 — 로컬 표가 멤버라고 해도 아무것도 내보내지 않는다
    active.set(Some("set-2"));
    let status = get(&rig, "/inbox/_requests/n1", "svc").await;
    assert_eq!(status.status, 503, "{}", status.body);
    assert!(!status.body.contains("sent"), "결과 본문이 나가지 않는다");
    let new = post(&rig, "/inbox/notify", "svc", "n2", json!({"to": "me"})).await;
    assert_eq!(new.status, 503, "{}", new.body);
    assert_eq!(rig.store.accepted().len(), 1, "구판은 저장소에 쓰지 않는다");

    // 같은 집합으로 돌아오면 같은 요청이 이어진다
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
        // 결과가 커밋되는 순간 다른 프로세스가 새 집합을 활성화했다
        flip.set(Some("set-2"));
        store.set_state(
            &key_of("svc", "s1"),
            RunState::Succeeded,
            Some(r#"{"status":201,"body":{"id":"e-1"}}"#),
        );
    });
    let reply = post(&rig, "/inbox/agenda", "svc", "s1", json!({"event": "e-1"})).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(!reply.body.contains("e-1"), "결과 본문이 나가지 않는다");
    // 새 집합으로 시작한 쪽이라면 같은 키의 재전달·재조회가 같은 결과를 준다(결과는 저장소에 있다)
    assert_eq!(
        rig.store.accepted().len(),
        1,
        "접수는 커밋됐고 재전달은 새 접수를 만들지 않는다"
    );
}

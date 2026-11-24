//! 구판 프로세스 닫기: 저장소의 활성 집합과 다른 프로세스는 새 접수도 결과 반환도 하지 않는다.
use super::*;
use crate::{
    active::{ActiveCheck, Release},
    entrance::Owner,
    testing::ports::MemoryActive,
};
use bot_core::id::BindingId;

fn with_active(rig: Rig, check: Arc<dyn ActiveCheck>) -> Rig {
    Rig {
        ingress: rig.ingress.with_active(check),
        ..rig
    }
}

fn is_accepted(admission: &Admission) -> bool {
    matches!(admission, Admission::Accepted { .. })
}

#[tokio::test]
async fn a_process_that_is_not_the_active_set_accepts_nothing_and_asks_for_redelivery() {
    let active = MemoryActive::new(Some("set-1"));
    let rig = with_active(rig(base()), active.check("set-1"));
    assert!(is_accepted(
        &rig.ingress.admit(cli_chat("a1", "첫 요청")).await
    ));
    // 다른 프로세스가 새 집합을 활성화했다
    active.set(Some("set-2"));
    let stale = rig.ingress.admit(cli_chat("a2", "새 요청")).await;
    assert_eq!(stale, Admission::Rejected(Rejection::Unavailable));
    assert!(!stale.settled(), "구판은 ack하지 않아 상대가 다시 보낸다");
    // 아직 게시되지 않은 것도 같다
    active.set(None);
    assert_eq!(
        rig.ingress.admit(cli_chat("a2", "새 요청")).await,
        Admission::Rejected(Rejection::Unavailable)
    );
    assert_eq!(rig.store.accepted().len(), 1, "구판은 저장소에 쓰지 않는다");
    // 같은 집합으로 돌아오면 같은 요청이 접수된다
    active.set(Some("set-1"));
    assert!(is_accepted(
        &rig.ingress.admit(cli_chat("a2", "새 요청")).await
    ));
    assert_eq!(rig.store.accepted().len(), 2);
}

#[tokio::test]
async fn a_stale_process_does_not_forward_private_inputs_either() {
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    secure.input.insert("text".into(), "body.text".into());
    let active = MemoryActive::new(Some("set-2"));
    let rig = with_active(
        rig_with(base().trigger("main", secure), |host| {
            host.limits.per_principal_per_min = Some(30)
        }),
        active.check("set-1"),
    );
    let stale = rig.ingress.admit(slack_dm("U1", "p1", "비밀")).await;
    assert_eq!(stale, Admission::Rejected(Rejection::Unavailable));
    assert!(rig.private.received().is_empty());
    active.set(Some("set-1"));
    assert_eq!(
        rig.ingress.admit(slack_dm("U1", "p1", "비밀")).await,
        Admission::Private
    );
    assert_eq!(rig.private.received().len(), 1);
}

#[tokio::test]
async fn releasing_a_result_needs_both_membership_and_the_active_set() {
    let active = MemoryActive::new(Some("set-1"));
    let rig = with_active(rig(base()), active.check("set-1"));
    let real = Real::Cli {
        uid: 501,
        profile: ProfileId("personal".into()),
    };
    let owner = Owner {
        profile: ProfileId("personal".into()),
        binding: BindingId("desk".into()),
    };
    let me = PrincipalId("local:owner".into());
    assert_eq!(
        rig.ingress.release(&real, &owner, &me).await,
        Release::Allowed
    );
    // 멤버가 아니면 활성 여부와 무관하게 막는다
    let ghost = PrincipalId("local:ghost".into());
    assert_eq!(
        rig.ingress.release(&real, &owner, &ghost).await,
        Release::Forbidden
    );
    // 구판은 로컬 표가 멤버라고 해도 결과를 내보내지 않는다
    active.set(Some("set-2"));
    assert_eq!(
        rig.ingress.release(&real, &owner, &me).await,
        Release::Unavailable
    );
    assert!(rig.ingress.verify_active().await.is_err());
}

#[tokio::test]
async fn a_stale_conclusion_from_the_store_is_not_acked_either() {
    let rig = rig(base());
    rig.store.fail_next(Error::Stale);
    let result = rig.ingress.admit(cli_chat("s1", "안녕")).await;
    assert_eq!(result, Admission::Rejected(Rejection::Unavailable));
    assert!(!result.settled());
}

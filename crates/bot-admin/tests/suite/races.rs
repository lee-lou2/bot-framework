//! 동시 요청 경합: 같은 승인을 두 번 동시에 눌러도 원장 판정은 한 건이다.

use crate::support::*;
use bot_admin::fake::{Fake, digest};
use bot_core::admin::ApprovalState;

const PATH: &str = "/v1/admin/approvals/ap-1/decide";

fn body(allow: bool) -> String {
    let seen = bot_admin::json::hex(&digest("c-1:agenda.delete"));
    format!(r#"{{"allow":{allow},"seen":"{seen}"}}"#)
}

#[tokio::test]
async fn two_concurrent_identical_decisions_both_succeed_with_one_ledger_decision() {
    for allow in [true, false] {
        let s = start(Fake::demo()).await;
        s.fake.gate_approvals(2); // 두 요청이 모두 대기 상태를 읽은 뒤에야 브로커로 넘어간다
        let payload = body(allow);
        let (a, b) = tokio::join!(
            post(&s, OPERATOR, PATH, &payload),
            post(&s, OPERATOR, PATH, &payload)
        );
        assert_eq!((a.status, b.status), (200, 200), "{} / {}", a.body, b.body);
        let repeated = [&a, &b]
            .iter()
            .filter(|r| r.json()["repeated"] == true)
            .count();
        assert_eq!(repeated, 1, "한 요청은 신규, 다른 요청은 기존 결과");
        assert_eq!(s.fake.decisions().len(), 1, "원장 판정은 한 건");
        let want = if allow { "approved" } else { "denied" };
        for r in [&a, &b] {
            assert_eq!(r.json()["approval"]["state"], want);
        }
    }
}

#[tokio::test]
async fn two_concurrent_opposite_decisions_leave_exactly_one_winner() {
    let s = start(Fake::demo()).await;
    s.fake.gate_approvals(2);
    let (yes, no) = (body(true), body(false));
    let (a, b) = tokio::join!(
        post(&s, OPERATOR, PATH, &yes),
        post(&s, OPERATOR, PATH, &no)
    );
    let mut codes = [a.status, b.status];
    codes.sort_unstable();
    assert_eq!(codes, [200, 409], "{} / {}", a.body, b.body);
    assert_eq!(s.fake.decisions().len(), 1);
    let loser = if a.status == 409 { &a } else { &b };
    assert_eq!(loser.json()["error"], "stale");
}

#[tokio::test]
async fn a_missing_decider_record_is_never_treated_as_the_same_person() {
    let s = start(Fake::demo()).await;
    assert_eq!(post(&s, OPERATOR, PATH, &body(true)).await.status, 200);
    // 결정자 기록이 사라진(None) 승인은 같은 사람의 재전송으로 인정하지 않는다.
    s.fake.edit(|d| {
        d.approvals[0].decided_by = None;
        d.approvals[0].state = ApprovalState::Consumed;
    });
    let r = post(&s, OPERATOR, PATH, &body(true)).await;
    assert_eq!((r.status, r.json()["error"].as_str()), (409, Some("stale")));
    // 결정자가 요청자 본인으로 기록돼 있으면 소비된 뒤에도 기존 결과를 돌려준다.
    s.fake
        .edit(|d| d.approvals[0].decided_by = Some(bot_admin::fake::pid("local:owner")));
    let r = post(&s, OPERATOR, PATH, &body(true)).await;
    assert_eq!(
        (
            r.status,
            r.json()["repeated"].as_bool(),
            r.json()["approval"]["state"].as_str()
        ),
        (200, Some(true), Some("consumed"))
    );
    assert_eq!(s.fake.decisions().len(), 1);
}

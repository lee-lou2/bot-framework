//! 도구가 받을 자격이 보관소에 없으면 새 호출은 기록도 승인 카드도 없이 거절한다(`call.refuse secret_missing`, Denied).
//! 인자 거절이 먼저이고, 이미 기록한 호출은 기록대로 이어 가며, 읽지 못한 보관소는 «없음»이 아니다.

use super::*;
use crate::ports::Sealed;
use bot_core::profile::Tool;

pub(super) fn needy(class: Risk) -> Tool {
    let mut t = tool(class, &["calendar.read"]);
    t.secrets.insert(
        "MAIL_TOKEN".into(),
        SecretName::parse("mail-token").unwrap(),
    );
    t
}

/// 도구 `agenda.mail`이 `mail-token`을 선언하고 프로필이 그 이름을 허용한다.
fn secret_world(class: Risk) -> World {
    let w = World::new();
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(class))
            .build(),
    );
    w
}

fn refused(w: &World) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.refuse".into(), "secret_missing".into()))
}

#[test]
fn a_missing_secret_refuses_a_new_call_before_it_is_recorded_or_any_approval_is_asked() {
    // 되돌릴 수 없는 도구라 자격이 있었다면 승인 카드가 열렸을 호출이다
    let w = secret_world(Risk::Irreversible);
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(refused(&w));
    assert!(!w.ledger.has_call("r1", "c1"), "기록하지 않는다");
    assert!(w.ledger.audit_log().is_empty(), "승인도 시작도 없다");
    assert!(w.runner.jobs().is_empty());
    // 그 프로필의 이름공간에서 물었고, 선언한 이름만 물었다
    let checks = w.runner.secret_checks();
    assert_eq!(
        checks,
        vec![(ProfileId("personal".into()), vec!["mail-token".to_owned()])]
    );
    // 운영자가 채우면 같은 호출 ID가 정상으로 이어진다(거절은 호출 ID를 쓰지 않았다)
    w.runner.restore_secret("mail-token");
    awaiting(w.call("c1", "agenda.mail", "{}"));
    assert!(w.ledger.has_call("r1", "c1"));
}

#[test]
fn an_unreadable_store_is_not_a_missing_secret_and_other_tools_are_never_checked() {
    let w = secret_world(Risk::Read);
    // 보관소를 읽지 못한다: «없음»이 아니라서 거절하지 않고 실행기에 맡긴다
    w.runner.fail_secret_checks_with(Some(Error::Unavailable));
    done(w.call("c1", "agenda.mail", "{}"));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(!refused(&w));
    // 자격을 선언하지 않은 도구는 보관소를 묻지 않는다
    let before = w.runner.secret_checks().len();
    done(w.call("c2", "agenda.list", "{}"));
    assert_eq!(w.runner.secret_checks().len(), before);
}

#[test]
fn only_new_calls_are_checked_a_recorded_call_continues_as_recorded() {
    let w = secret_world(Risk::Read);
    let first = done(w.call("c1", "agenda.mail", "{}"));
    // 그 뒤 자격이 사라졌다: 끝난 호출의 재전달은 기록한 결과를 그대로 돌려준다(이미 끝난 결과를 숨기지 않는다)
    w.runner.miss_secret("mail-token");
    assert_eq!(done(w.call("c1", "agenda.mail", "{}")), first);
    assert_eq!(w.runner.jobs().len(), 1);
    // 같은 도구의 새 호출은 거절된다
    assert!(matches!(
        w.call("c2", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(!w.ledger.has_call("r1", "c2"));
    assert!(refused(&w));
}

#[test]
fn the_argument_check_comes_first_so_a_bad_input_stays_invalid() {
    struct Picky;
    impl Sealer for Picky {
        fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error> {
            if args.0.contains("bad") {
                return Err(Error::Invalid);
            }
            FakeSealer.seal(tool, args)
        }
    }
    let w = secret_world(Risk::Read);
    w.runner.miss_secret("mail-token");
    let picky = w.bare_with(Arc::new(Picky));
    // 자격도 없고 인자도 틀렸다: 모델이 고칠 수 있는 인자 거절이 먼저다(고정 판본의 입력 검사는 보관소와 무관하다)
    assert!(matches!(
        w.call_on(&picky, "r1", 1, "c1", "agenda.mail", r#"{"x":"bad"}"#),
        Err(Error::Invalid)
    ));
    assert!(!refused(&w));
    assert!(w.runner.secret_checks().is_empty());
    assert!(matches!(
        w.call_on(&picky, "r1", 1, "c2", "agenda.mail", r#"{"x":"ok"}"#),
        Err(Error::Denied)
    ));
    assert!(refused(&w));
}

fn denied(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.deny".into(), word.into()))
}

#[test]
fn a_name_the_call_may_never_use_is_refused_without_the_store_being_read_even_once() {
    // 호스트 전용 이름(프로필이 목록에 올려도 도구에는 주지 않는다)
    let w = World::new();
    let mut reserved = tool(Risk::Read, &["calendar.read"]);
    reserved
        .secrets
        .insert("TOKEN".into(), SecretName::parse("private-intake").unwrap());
    w.publish(
        base("r1")
            .secret("private-intake")
            .tool("main", "agenda.mail", reserved)
            .build(),
    );
    w.runner.miss_secret("private-intake"); // 읽었다면 «없음»으로 답했을 것이다
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(denied(&w, "secret_reserved") && !refused(&w));
    assert!(w.runner.secret_checks().is_empty(), "보관소 읽기 0");
    assert!(w.runner.jobs().is_empty());
    // 프로필이 허용하지 않은 이름
    let w = World::new();
    w.publish(
        base("r1")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(denied(&w, "secret_not_allowed") && !refused(&w));
    assert!(w.runner.secret_checks().is_empty(), "보관소 읽기 0");
    // 허용했지만 없는 이름과 쓸 수 없는 이름을 함께 선언해도, 쓸 수 없는 이름이 먼저 판정되고 어느 이름도 읽지 않는다
    let w = World::new();
    let mut both = needy(Risk::Read);
    both.secrets
        .insert("OTHER".into(), SecretName::parse("not-listed").unwrap());
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", both)
            .build(),
    );
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(denied(&w, "secret_not_allowed") && !refused(&w));
    assert!(w.runner.secret_checks().is_empty(), "보관소 읽기 0");
    // 허용한 이름만 선언하면 그제야 보관소를 확인한다
    let w = secret_world(Risk::Read);
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(refused(&w) && !denied(&w, "secret_not_allowed"));
    assert_eq!(w.runner.secret_checks().len(), 1);
}

#[test]
fn a_requester_whose_role_was_revoked_never_makes_the_store_be_read() {
    let w = secret_world(Risk::Read);
    w.runner.miss_secret("mail-token"); // 읽었다면 «없음»으로 답했을 것이다
    // 현재 판본에서 요청자의 `calendar.read`가 철회됐다(고정 판본에는 남아 있다)
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    // 인가 거절로 기록과 함께 닫히고 자격 거절이 아니다. 보관소 조회 0, 속도 제한 자리도 남지 않는다
    assert!(w.ledger.has_call("r1", "c1"));
    assert!(!refused(&w));
    assert!(w.runner.secret_checks().is_empty(), "보관소 읽기 0");
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_call_over_the_rate_limit_never_makes_the_store_be_read_and_a_missing_secret_still_counts_against_it()
 {
    // 한꺼번에 둘까지: 자격이 없는 호출도 속도 제한을 거친다
    let w = World::with(cfg_with_rate(60, 2, 4));
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    w.runner.miss_secret("mail-token");
    for call in ["c1", "c2"] {
        assert!(matches!(
            w.call(call, "agenda.mail", "{}"),
            Err(Error::Denied)
        ));
        assert!(!w.ledger.has_call("r1", call));
    }
    assert_eq!(w.runner.secret_checks().len(), 2);
    // 한도를 넘으면 자격을 거절하기 전에 속도 제한이 먼저 돌려보낸다: 보관소 조회가 늘지 않고 기록도 없다
    assert!(matches!(
        w.call("c3", "agenda.mail", "{}"),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.runner.secret_checks().len(), 2, "보관소 읽기 그대로");
    assert!(!w.ledger.has_call("r1", "c3"));
    // 시간이 지나 자리가 돌아오면 같은 호출을 다시 판단한다
    w.clock.advance(Duration::from_secs(60));
    assert!(matches!(
        w.call("c3", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.secret_checks().len(), 3);
}

#[test]
fn a_new_call_that_passes_the_checks_keeps_one_rate_slot_for_the_whole_call() {
    // 미리 잡은 속도 제한 자리를 본 흐름이 그대로 쓴다: 한 호출이 한도를 두 번 쓰지 않는다
    let w = World::with(cfg_with_rate(60, 2, 4));
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    done(w.call("c1", "agenda.mail", "{}"));
    done(w.call("c2", "agenda.mail", "{}"));
    assert!(matches!(
        w.call("c3", "agenda.mail", "{}"),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.runner.jobs().len(), 2);
}

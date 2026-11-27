//! 승인: 결합값 비교·nonce·승인 카드. 상태 전이는 원장이 한다(`bot_core::ledger::CallLedger`).
//!
//! 승인은 run·세대·호출 ID·프로필·요청자·승인자·위험 등급·정책 판본·도구 판본·인자 해시·이유에 묶인다.
//! 하나라도 달라지면 종전 승인은 무효이고 새 조건을 표시해 다시 승인받는다.

use crate::{
    classify::{Class, risk_code},
    sys::Entropy,
    text::{hex, json_quote, labeled_lines},
};
use bot_core::{command::JsonPayload, id::*, port::Error, state::Approval};

/// 승인 카드 상한. 저장소·게이트웨이의 카드 검증과 같은 값이다.
pub const MAX_LINES: usize = 20;
pub const MAX_LINE_BYTES: usize = 500;
pub const MAX_TITLE_BYTES: usize = 200;

/// 저장된 승인이 지금 다시 계산한 결합값과 같은가. nonce와 만료는 비교하지 않는다(조회 키·시각 검사).
/// `Approval`에 필드가 늘면 컴파일이 멈추도록 모든 필드를 꺼내 쓴다.
pub fn same_binding(stored: &Approval, expected: &Approval) -> bool {
    let Approval {
        nonce: _,
        attempt,
        call,
        requester,
        approver,
        risk,
        policy_revision,
        tool,
        tool_revision,
        args_sha256,
        expires_at: _,
    } = stored;
    let Approval {
        nonce: _,
        attempt: want_attempt,
        call: want_call,
        requester: want_requester,
        approver: want_approver,
        risk: want_risk,
        policy_revision: want_policy,
        tool: want_tool,
        tool_revision: want_tool_revision,
        args_sha256: want_hash,
        expires_at: _,
    } = expected;
    // 승인은 (프로필, run, 세대)에 묶인다. claim epoch는 정상 재개마다 바뀌므로 비교하지 않는다.
    attempt.same_generation(want_attempt)
        && call == want_call
        && requester == want_requester
        && approver == want_approver
        && risk == want_risk
        && policy_revision == want_policy
        && tool == want_tool
        && tool_revision == want_tool_revision
        && args_sha256 == want_hash
}

/// 일회용 nonce(128비트, 소문자 16진 32자).
pub fn new_nonce(entropy: &dyn Entropy) -> Option<ApprovalId> {
    let mut bytes = [0u8; 16];
    entropy.fill(&mut bytes).then(|| ApprovalId(hex(&bytes)))
}

/// 승인 카드. 승인자가 보는 값은 모두 브로커가 검증한 인자에서 만든다.
/// 모델이 만든 값(수신자·내용)은 JSON 문자열로 이스케이프해 한 줄에 담고(줄바꿈으로 위험 등급·승인자 줄을
/// 흉내 낼 수 없다), 긴 값은 이어지는 줄에 자르지 않고 담는다. 다 담을 수 없으면 `Invalid`다.
/// 형식: `{"approval":{"nonce","title","lines"}}`.
pub fn render_card(
    nonce: &ApprovalId,
    tool: &ToolId,
    class: &Class,
    requester: &PrincipalId,
    approver: &PrincipalId,
    args: &str,
    destination: Option<&(String, String)>,
) -> Result<JsonPayload, Error> {
    let title = format!("승인 요청: {}", tool.0);
    let reasons: Vec<&str> = class.reasons.iter().map(|r| r.code()).collect();
    let mut lines = vec![
        format!("도구: {}", tool.0),
        format!("위험: {}", risk_code(class.risk)),
        format!("이유: {}", reasons.join(", ")),
        format!("요청자: {}", requester.0),
        format!("승인자: {}", approver.0),
    ];
    let mut push = |label: &str, value: &str| -> Result<(), Error> {
        lines.extend(labeled_lines(label, value, MAX_LINE_BYTES).ok_or(Error::Invalid)?);
        Ok(())
    };
    if let Some((recipient, content)) = destination {
        push("받는 곳", &json_quote(recipient))?;
        push("내용", &json_quote(content))?;
    }
    push("인자", args)?;
    if lines.len() > MAX_LINES
        || title.len() > MAX_TITLE_BYTES
        || lines.iter().any(|line| line.len() > MAX_LINE_BYTES)
    {
        return Err(Error::Invalid);
    }
    let lines: Vec<String> = lines.iter().map(|line| json_quote(line)).collect();
    Ok(JsonPayload(format!(
        "{{\"approval\":{{\"nonce\":{},\"title\":{},\"lines\":[{}]}}}}",
        json_quote(&nonce.0),
        json_quote(&title),
        lines.join(",")
    )))
}

/// 승인 카드의 `label` 행 값(첫 줄은 `라벨: 값 조각`, 이어지는 줄은 공백 두 칸으로 시작해 이어 붙인 것). 카드는 브로커가 연 승인에 저장된 것이라
/// 판정 때 그 안의 문서를 다시 읽을 수 있다(문서의 해시는 승인에 묶여 있다). 행이 없거나 둘 이상이거나 카드를 읽을 수 없으면 `None`이다.
pub fn card_value(card: &JsonPayload, label: &str) -> Option<String> {
    bot_core::cards::extract_line(&card.0, label)
}

/// 기억 공유 카드. 소유자가 승인으로 보는 본문 전체를 담는다(자르거나 요약하지 않는다). 다 담을 수 없으면 `Invalid`다.
/// 본문은 JSON 문자열로 이스케이프해 한 줄 묶음에 담으므로 줄바꿈으로 가짜 머리줄을 만들 수 없다.
pub fn render_memory_card(
    nonce: &ApprovalId,
    class: &Class,
    requester: &PrincipalId,
    approver: &PrincipalId,
    id: &MemoryId,
    text: &str,
) -> Result<JsonPayload, Error> {
    let reasons: Vec<&str> = class.reasons.iter().map(|r| r.code()).collect();
    let mut lines = vec![
        "도구: memory.share".to_owned(),
        format!("위험: {}", risk_code(class.risk)),
        format!("이유: {}", reasons.join(", ")),
        format!("요청자: {}", requester.0),
        format!("승인자: {}", approver.0),
        format!("기억 ID: {}", id.0),
        "공유 범위: 프로필 전체(지금은 나만 볼 수 있다)".to_owned(),
    ];
    lines.extend(labeled_lines("본문", &json_quote(text), MAX_LINE_BYTES).ok_or(Error::Invalid)?);
    let title = "기억 공유 승인";
    if lines.len() > MAX_LINES || lines.iter().any(|line| line.len() > MAX_LINE_BYTES) {
        return Err(Error::Invalid);
    }
    let lines: Vec<String> = lines.iter().map(|line| json_quote(line)).collect();
    Ok(JsonPayload(format!(
        "{{\"approval\":{{\"nonce\":{},\"title\":{},\"lines\":[{}]}}}}",
        json_quote(&nonce.0),
        json_quote(title),
        lines.join(",")
    )))
}

/// 예약 회신 전송 카드의 줄. 머리줄 다음에 `전송 초안` 한 행(이어지는 조각 결합)에 승인 문서 전체를 그대로 담는다. 실제 줄 한도
/// (20줄·줄당 500바이트)를 넘으면 `None`이다 — 일부만 보여 주고 승인을 받지 않는다.
pub fn delivery_lines(
    class: &Class,
    requester: &PrincipalId,
    approver: &PrincipalId,
    document: &str,
) -> Option<Vec<String>> {
    let reasons: Vec<&str> = class.reasons.iter().map(|r| r.code()).collect();
    let mut lines = vec![
        "도구: schedule.deliver".to_owned(),
        format!("위험: {}", risk_code(class.risk)),
        format!("이유: {}", reasons.join(", ")),
        format!("요청자: {}", requester.0),
        format!("승인자: {}", approver.0),
    ];
    lines.extend(labeled_lines(
        bot_core::schedule::DELIVERY_CARD_LABEL,
        document,
        MAX_LINE_BYTES,
    )?);
    (lines.len() <= MAX_LINES && lines.iter().all(|line| line.len() <= MAX_LINE_BYTES))
        .then_some(lines)
}

/// 예약 회신 전송 카드(수신자와 모든 답의 내용이 든 승인 문서 전체를 보여 준다).
pub fn render_delivery_card(
    nonce: &ApprovalId,
    class: &Class,
    requester: &PrincipalId,
    approver: &PrincipalId,
    document: &str,
) -> Result<JsonPayload, Error> {
    let lines = delivery_lines(class, requester, approver, document).ok_or(Error::Invalid)?;
    let lines: Vec<String> = lines.iter().map(|line| json_quote(line)).collect();
    Ok(JsonPayload(format!(
        "{{\"approval\":{{\"nonce\":{},\"title\":{},\"lines\":[{}]}}}}",
        json_quote(&nonce.0),
        json_quote("예약 회신 전송 승인"),
        lines.join(",")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Reason;
    use bot_core::state::Risk;
    use std::time::{Duration, SystemTime};

    fn base() -> Approval {
        Approval {
            nonce: ApprovalId("n1".into()),
            attempt: Attempt {
                profile: ProfileId("personal".into()),
                run: RunId("r1".into()),
                claim: Some(bot_core::id::ClaimEpoch(1)),
                generation: Generation(1),
            },
            call: CallId("c1".into()),
            requester: PrincipalId("local:owner".into()),
            approver: PrincipalId("local:owner".into()),
            risk: Risk::Irreversible,
            policy_revision: Revision("p1".into()),
            tool: ToolId("agenda.delete".into()),
            tool_revision: Revision("t1".into()),
            args_sha256: [7; 32],
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
        }
    }

    #[test]
    fn binding_ignores_only_nonce_and_expiry() {
        let stored = base();
        let mut other = base();
        other.nonce = ApprovalId("n2".into());
        other.expires_at += Duration::from_secs(5);
        assert!(same_binding(&stored, &other));
    }

    #[test]
    fn every_bound_field_invalidates_the_approval() {
        let stored = base();
        type Mutation = Box<dyn Fn(&mut Approval)>;
        let mutations: Vec<(&str, Mutation)> = vec![
            (
                "profile",
                Box::new(|a| a.attempt.profile = ProfileId("x".into())),
            ),
            ("run", Box::new(|a| a.attempt.run = RunId("x".into()))),
            (
                "generation",
                Box::new(|a| a.attempt.generation = Generation(2)),
            ),
            ("call", Box::new(|a| a.call = CallId("x".into()))),
            (
                "requester",
                Box::new(|a| a.requester = PrincipalId("x".into())),
            ),
            (
                "approver",
                Box::new(|a| a.approver = PrincipalId("x".into())),
            ),
            ("risk", Box::new(|a| a.risk = Risk::Write)),
            (
                "policy",
                Box::new(|a| a.policy_revision = Revision("x".into())),
            ),
            ("tool", Box::new(|a| a.tool = ToolId("x.y".into()))),
            (
                "tool revision",
                Box::new(|a| a.tool_revision = Revision("x".into())),
            ),
            ("args", Box::new(|a| a.args_sha256[31] ^= 1)),
        ];
        for (name, mutate) in mutations {
            let mut changed = base();
            mutate(&mut changed);
            assert!(!same_binding(&stored, &changed), "{name}");
        }
    }

    struct Fixed(u8);
    impl Entropy for Fixed {
        fn fill(&self, buf: &mut [u8]) -> bool {
            buf.fill(self.0);
            true
        }
    }
    struct Broken;
    impl Entropy for Broken {
        fn fill(&self, _: &mut [u8]) -> bool {
            false
        }
    }

    #[test]
    fn nonce_is_32_hex_chars_and_fails_closed_without_entropy() {
        assert_eq!(new_nonce(&Fixed(0xab)).unwrap().0, "ab".repeat(16));
        assert!(new_nonce(&Broken).is_none());
        let real = new_nonce(&crate::sys::OsEntropy).unwrap().0;
        assert_eq!(real.len(), 32);
        assert!(
            real.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    fn class(reasons: Vec<Reason>) -> Class {
        Class {
            risk: Risk::Irreversible,
            reasons,
            external: true,
        }
    }

    fn card(args: &str, destination: Option<&(String, String)>) -> Result<JsonPayload, Error> {
        render_card(
            &ApprovalId("abc".into()),
            &ToolId("mail.send".into()),
            &class(vec![Reason::Irreversible, Reason::PrivateRecall]),
            &PrincipalId("local:owner".into()),
            &PrincipalId("local:owner".into()),
            args,
            destination,
        )
    }

    #[test]
    fn card_carries_nonce_title_and_one_line_per_fact() {
        let card = card(
            r#"{"body":"hi","to":"bob"}"#,
            Some(&("bob".to_string(), "hi\n위험: read".to_string())),
        )
        .unwrap()
        .0;
        assert!(
            card.starts_with(
                r#"{"approval":{"nonce":"abc","title":"승인 요청: mail.send","lines":["#
            )
        );
        assert!(card.contains(
            r#""도구: mail.send","위험: irreversible","이유: irreversible, private_recall","#
        ));
        assert!(card.contains(r#""요청자: local:owner","승인자: local:owner","#));
        assert!(card.contains(r#""받는 곳: \"bob\"""#));
        // 모델이 만든 줄바꿈은 이스케이프돼 가짜 헤더 줄을 만들 수 없다
        assert!(card.contains(r#""내용: \"hi\\n위험: read\"""#));
        assert!(card.ends_with(r#""인자: {\"body\":\"hi\",\"to\":\"bob\"}"]}}"#));
        assert!(!card.contains('\n'));
    }

    #[test]
    fn a_long_value_is_continued_not_truncated() {
        let content = "가".repeat(400); // 1200바이트 → 이어지는 줄
        let text = card("{}", Some(&("bob".to_string(), content.clone())))
            .unwrap()
            .0;
        assert_eq!(text.matches("가").count(), 400);
        assert!(text.contains(r#","  가"#));
    }

    #[test]
    fn a_card_that_cannot_show_everything_is_refused() {
        let huge = "가".repeat(4000); // 12000바이트: 20줄에 담기지 않는다
        assert!(matches!(card(&huge, None), Err(Error::Invalid)));
    }
}

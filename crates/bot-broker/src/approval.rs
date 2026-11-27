//! Approval binding comparison, nonce generation, and card rendering.

use crate::{
    classify::{Class, risk_code},
    sys::Entropy,
    text::{hex, json_quote, labeled_lines},
};
use bot_core::{
    cards::{
        LABEL_APPROVER, LABEL_ARGS, LABEL_BODY, LABEL_CONTENT, LABEL_MEMORY_ID, LABEL_REASON,
        LABEL_RECIPIENT, LABEL_REQUESTER, LABEL_RISK, LABEL_SHARE_SCOPE, LABEL_TOOL,
    },
    command::JsonPayload,
    id::*,
    port::Error,
    state::Approval,
};

/// Card size limits shared with store and gateway validation.
pub const MAX_LINES: usize = 20;
pub const MAX_LINE_BYTES: usize = 500;
pub const MAX_TITLE_BYTES: usize = 200;

/// Checks whether a stored approval matches the recomputed binding (ignoring `nonce` and `expires_at`).
/// Destructures every field so adding a field to `Approval` forces updating this check.
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
    // Bound to (profile, run, generation); claim epoch changes across normal resumes.
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

/// Generates a 128-bit lowercase hex approval nonce.
pub fn new_nonce(entropy: &dyn Entropy) -> Option<ApprovalId> {
    let mut bytes = [0u8; 16];
    entropy.fill(&mut bytes).then(|| ApprovalId(hex(&bytes)))
}

/// Renders a tool approval card (`{"approval":{"nonce","title","lines"}}`).
/// Model-supplied fields are JSON-escaped to prevent forged header lines; long values wrap onto
/// continuation lines without truncation and fail with `Invalid` if the card exceeds limits.
pub fn render_card(
    nonce: &ApprovalId,
    tool: &ToolId,
    class: &Class,
    requester: &PrincipalId,
    approver: &PrincipalId,
    args: &str,
    destination: Option<&(String, String)>,
) -> Result<JsonPayload, Error> {
    let title = format!("Approval request: {}", tool.0);
    let reasons: Vec<&str> = class.reasons.iter().map(|r| r.code()).collect();
    let mut lines = vec![
        format!("{LABEL_TOOL}: {}", tool.0),
        format!("{LABEL_RISK}: {}", risk_code(class.risk)),
        format!("{LABEL_REASON}: {}", reasons.join(", ")),
        format!("{LABEL_REQUESTER}: {}", requester.0),
        format!("{LABEL_APPROVER}: {}", approver.0),
    ];
    let mut push = |label: &str, value: &str| -> Result<(), Error> {
        lines.extend(labeled_lines(label, value, MAX_LINE_BYTES).ok_or(Error::Invalid)?);
        Ok(())
    };
    if let Some((recipient, content)) = destination {
        push(LABEL_RECIPIENT, &json_quote(recipient))?;
        push(LABEL_CONTENT, &json_quote(content))?;
    }
    push(LABEL_ARGS, args)?;
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

/// Extracts the reassembled value for a single `label` from a rendered card.
pub fn card_value(card: &JsonPayload, label: &str) -> Option<String> {
    bot_core::cards::extract_line(&card.0, label)
}

/// Renders a memory share approval card containing the full JSON-escaped body; fails with `Invalid`
/// rather than truncating so the approver reviews the exact hash-bound text.
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
        format!("{LABEL_TOOL}: memory.share"),
        format!("{LABEL_RISK}: {}", risk_code(class.risk)),
        format!("{LABEL_REASON}: {}", reasons.join(", ")),
        format!("{LABEL_REQUESTER}: {}", requester.0),
        format!("{LABEL_APPROVER}: {}", approver.0),
        format!("{LABEL_MEMORY_ID}: {}", id.0),
        format!("{LABEL_SHARE_SCOPE}: Profile-wide (currently private to you)"),
    ];
    lines.extend(
        labeled_lines(LABEL_BODY, &json_quote(text), MAX_LINE_BYTES).ok_or(Error::Invalid)?,
    );
    let title = "Memory share approval";
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

/// Builds scheduled delivery card lines containing the entire approval document; returns `None` if limits are exceeded.
pub fn delivery_lines(
    class: &Class,
    requester: &PrincipalId,
    approver: &PrincipalId,
    document: &str,
) -> Option<Vec<String>> {
    let reasons: Vec<&str> = class.reasons.iter().map(|r| r.code()).collect();
    let mut lines = vec![
        format!("{LABEL_TOOL}: schedule.deliver"),
        format!("{LABEL_RISK}: {}", risk_code(class.risk)),
        format!("{LABEL_REASON}: {}", reasons.join(", ")),
        format!("{LABEL_REQUESTER}: {}", requester.0),
        format!("{LABEL_APPROVER}: {}", approver.0),
    ];
    lines.extend(labeled_lines(
        bot_core::schedule::DELIVERY_CARD_LABEL,
        document,
        MAX_LINE_BYTES,
    )?);
    (lines.len() <= MAX_LINES && lines.iter().all(|line| line.len() <= MAX_LINE_BYTES))
        .then_some(lines)
}

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
        json_quote("Scheduled delivery approval"),
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
            Some(&("bob".to_string(), "hi\nRisk: read".to_string())),
        )
        .unwrap()
        .0;
        assert!(card.starts_with(
            r#"{"approval":{"nonce":"abc","title":"Approval request: mail.send","lines":["#
        ));
        assert!(card.contains(
            r#""Tool: mail.send","Risk: irreversible","Reason: irreversible, private_recall","#
        ));
        assert!(card.contains(r#""Requester: local:owner","Approver: local:owner","#));
        assert!(card.contains(r#""Recipient: \"bob\"""#));
        assert!(card.contains(r#""Content: \"hi\\nRisk: read\"""#));
        assert!(card.ends_with(r#""Args: {\"body\":\"hi\",\"to\":\"bob\"}"]}}"#));
        assert!(!card.contains('\n'));
    }

    #[test]
    fn a_long_value_is_continued_not_truncated() {
        let content = "€".repeat(400); // 1200 bytes across continuation lines
        let text = card("{}", Some(&("bob".to_string(), content.clone())))
            .unwrap()
            .0;
        assert_eq!(text.matches("€").count(), 400);
        assert!(text.contains(r#","  €"#));
    }

    #[test]
    fn a_card_that_cannot_show_everything_is_refused() {
        let huge = "€".repeat(4000); // 12000 bytes exceeds the 20-line cap
        assert!(matches!(card(&huge, None), Err(Error::Invalid)));
    }
}

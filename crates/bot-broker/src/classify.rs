//! Tool risk classification and approval reason derivation.
//!
//! - `Irreversible` tools and `Write` tools matching `policy.approve` require approval.
//! - Tools with an explicit non-reply destination (`destination: {recipient, content}`) are promoted to `Irreversible` and `external`.
//! - Once private memory has been read in a run, every `external` tool (including reads) requires per-call approval.

use bot_core::{
    profile::{Destination, Scopes, Tool},
    state::Risk,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Reason {
    Irreversible,
    Policy,
    PrivateRecall,
}

impl Reason {
    pub fn code(self) -> &'static str {
        match self {
            Reason::Irreversible => "irreversible",
            Reason::Policy => "policy",
            Reason::PrivateRecall => "private_recall",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Class {
    pub risk: Risk,
    /// Sorted, deduplicated approval reasons; empty when no approval is required.
    pub reasons: Vec<Reason>,
    /// True when the tool sends input to a third party (declared `external` or explicit destination).
    pub external: bool,
}

impl Class {
    pub fn needs_approval(&self) -> bool {
        !self.reasons.is_empty()
    }

    pub fn codes(&self) -> Vec<String> {
        self.reasons.iter().map(|r| r.code().to_owned()).collect()
    }
}

pub fn risk_code(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Write => "write",
        Risk::Irreversible => "irreversible",
    }
}

pub fn classify(tool: &Tool, approve: &Scopes, private_recall: bool) -> Class {
    let explicit = matches!(tool.destination, Some(Destination::Explicit { .. }));
    let risk = if explicit {
        Risk::Irreversible
    } else {
        tool.class
    };
    let external = tool.external || explicit;
    let mut reasons = BTreeSet::new();
    if risk == Risk::Irreversible {
        reasons.insert(Reason::Irreversible);
    }
    if risk == Risk::Write && tool.scopes.iter().any(|scope| approve.contains(scope)) {
        reasons.insert(Reason::Policy);
    }
    if external && private_recall {
        reasons.insert(Reason::PrivateRecall);
    }
    Class {
        risk,
        reasons: reasons.into_iter().collect(),
        external,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tool;

    fn approve(items: &[&str]) -> Scopes {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn explicit() -> Option<Destination> {
        Some(Destination::Explicit {
            recipient: "args.to".into(),
            content: "args.body".into(),
        })
    }

    #[test]
    fn read_needs_no_approval_and_irreversible_always_does() {
        let read = classify(&tool(Risk::Read, &["a"]), &approve(&["a"]), false);
        assert!(!read.needs_approval());
        assert_eq!(read.risk, Risk::Read);
        let delete = classify(&tool(Risk::Irreversible, &["a"]), &approve(&[]), false);
        assert_eq!(delete.reasons, vec![Reason::Irreversible]);
        assert_eq!(delete.codes(), vec!["irreversible".to_string()]);
    }

    #[test]
    fn write_needs_approval_only_when_policy_names_one_of_its_scopes() {
        let t = tool(Risk::Write, &["calendar.write"]);
        assert!(!classify(&t, &approve(&[]), false).needs_approval());
        assert!(!classify(&t, &approve(&["other"]), false).needs_approval());
        let gated = classify(&t, &approve(&["calendar.write"]), false);
        assert_eq!(gated.reasons, vec![Reason::Policy]);
        assert_eq!(gated.risk, Risk::Write);
    }

    #[test]
    fn policy_approve_does_not_gate_reads() {
        let t = tool(Risk::Read, &["calendar.read"]);
        assert!(!classify(&t, &approve(&["calendar.read"]), false).needs_approval());
    }

    #[test]
    fn private_recall_gates_external_tools_even_when_they_only_read() {
        let mut search = tool(Risk::Read, &["web.search"]);
        search.external = true;
        assert!(!classify(&search, &approve(&[]), false).needs_approval());
        let after = classify(&search, &approve(&[]), true);
        assert_eq!(after.reasons, vec![Reason::PrivateRecall]);
        assert_eq!(after.risk, Risk::Read);
        assert!(after.external);
    }

    #[test]
    fn private_recall_does_not_gate_local_tools() {
        let local = tool(Risk::Read, &["calendar.read"]);
        assert!(!classify(&local, &approve(&[]), true).needs_approval());
    }

    #[test]
    fn explicit_recipient_is_irreversible_and_external() {
        let mut send = tool(Risk::Write, &["mail.send"]);
        send.destination = explicit();
        let class = classify(&send, &approve(&[]), false);
        assert_eq!(class.risk, Risk::Irreversible);
        assert!(class.external);
        assert_eq!(class.reasons, vec![Reason::Irreversible]);
        let after = classify(&send, &approve(&["mail.send"]), true);
        assert_eq!(
            after.reasons,
            vec![Reason::Irreversible, Reason::PrivateRecall]
        );
    }

    #[test]
    fn reply_destination_stays_a_plain_write() {
        let mut post = tool(Risk::Write, &["chat.post"]);
        post.destination = Some(Destination::Reply);
        let class = classify(&post, &approve(&[]), true);
        assert_eq!(class.risk, Risk::Write);
        assert!(!class.external);
        assert!(!class.needs_approval());
    }

    #[test]
    fn reasons_are_sorted_and_codes_are_stable() {
        let mut both = tool(Risk::Irreversible, &["x"]);
        both.external = true;
        let class = classify(&both, &approve(&["x"]), true);
        assert_eq!(class.codes(), vec!["irreversible", "private_recall"]);
        assert_eq!(risk_code(Risk::Write), "write");
    }
}

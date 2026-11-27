//! Operator-registered host catalog used to validate profile aliases at activation time.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct SlackWorkspace {
    pub channels: BTreeSet<String>,
    pub dm: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HostCatalog {
    pub servers: BTreeSet<String>,
    /// Profile -> allowed model aliases; missing profile denies all models.
    pub models: BTreeMap<String, BTreeSet<String>>,
    pub cli_accounts: BTreeSet<String>,
    pub slack: BTreeMap<String, SlackWorkspace>,
    /// Authenticated webhook HTTP endpoints (`kind: webhook`, including legacy omitted kind).
    pub http_endpoints: BTreeSet<String>,
    /// OpenAI-compatible endpoints (`kind: openai`): alias -> key-slot subjects (never key material).
    pub openai_endpoints: BTreeMap<String, BTreeSet<String>>,
    /// Profile -> egress cap (`host:port`); absent means uncapped beyond tool declarations.
    pub egress: BTreeMap<String, BTreeSet<String>>,
    /// Remote (Streamable HTTP) server alias -> target `host:port` (also present in `servers`).
    pub remote_servers: BTreeMap<String, String>,
    /// Upper bound for effective tool call timeouts in seconds (`None` defaults to 60s).
    pub tool_call_max_secs: Option<u32>,
    /// Profile -> allowed super-admin principals; absent defers to profile `super_admins`.
    pub super_admins: BTreeMap<String, BTreeSet<String>>,
}

impl HostCatalog {
    /// Host catalog matching `examples/profiles/` (`personal`/`owner` and `team`/`team`).
    pub fn demo() -> Self {
        let set = |items: &[&str]| {
            items
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<BTreeSet<_>>()
        };
        Self {
            servers: set(&["demo-agenda"]),
            models: BTreeMap::from([
                ("personal".to_owned(), set(&["assistant"])),
                ("team".to_owned(), set(&["assistant"])),
            ]),
            cli_accounts: set(&["owner", "team"]),
            ..Self::default()
        }
    }

    pub fn tool_call_cap(&self) -> u32 {
        self.tool_call_max_secs
            .unwrap_or(bot_core::profile::DEFAULT_TOOL_CALL_SECS)
    }

    pub fn allows_model(&self, profile: &str, alias: &str) -> bool {
        self.models.get(profile).is_some_and(|m| m.contains(alias))
    }

    pub fn models_of(&self, profile: &str) -> Vec<&str> {
        self.models
            .get(profile)
            .map(|m| m.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn effective_egress(&self, profile: &str, declared: &BTreeSet<String>) -> BTreeSet<String> {
        match self.egress.get(profile) {
            Some(cap) => declared.intersection(cap).cloned().collect(),
            None => declared.clone(),
        }
    }

    pub fn allows_super_admin(&self, profile: &str, principal: &str) -> bool {
        self.super_admins
            .get(profile)
            .is_none_or(|allowed| allowed.contains(principal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_is_intersection_when_capped() {
        let mut host = HostCatalog::demo();
        let declared: BTreeSet<String> = ["a.test:443", "b.test:443"].map(String::from).into();
        assert_eq!(host.effective_egress("personal", &declared), declared);
        host.egress
            .insert("personal".into(), ["b.test:443".to_owned()].into());
        assert_eq!(host.effective_egress("personal", &declared).len(), 1);
        assert!(
            host.allows_model("personal", "assistant") && !host.allows_model("other", "assistant")
        );
    }
}

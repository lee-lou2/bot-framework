//! Logical IDs (stable across retries), ownership generations, and claim epochs.
macro_rules! identifiers {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
            pub struct $name(pub String);
        )+
    };
}

identifiers!(
    ProfileId,
    AgentId,
    PrincipalId,
    BindingId,
    RequestId,
    RunId,
    CallId,
    ToolId,
    EffectId,
    HoldId,
    ApprovalId,
    Revision,
    MemoryId,
    ScheduleId,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Generation(pub u64);

/// Monotonic per-run fence issued on every claim, including normal wake-up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaimEpoch(pub u64);

impl CallId {
    /// Identifies a CLI tool slot by the turn's first session record sequence and position.
    pub fn cli(turn: u64, slot: u32) -> Self {
        Self(format!("cli-{turn}-{slot}"))
    }
}

impl ToolId {
    /// Maps `skill.tool` to its reversible model-relay MCP name.
    pub fn mcp_name(&self) -> String {
        self.0.replacen('.', "_", 1)
    }

    pub fn from_mcp_name(name: &str) -> Option<Self> {
        let (skill, tool) = name.split_once('_')?;
        (is_name(skill) && is_name(tool)).then(|| Self(format!("{skill}.{tool}")))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attempt {
    pub profile: ProfileId,
    pub run: RunId,
    pub generation: Generation,
    /// None is identity-only (approval, UI, trace); it cannot authorize worker operations.
    pub claim: Option<ClaimEpoch>,
}

/// Valid single-segment ASCII kebab-case identifier (<= 63 bytes).
pub fn is_name(name: &str) -> bool {
    matches!(name.as_bytes().first(), Some(b'a'..=b'z'))
        && name.len() <= 63
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Single-segment vault-relative name; forbids separators, paths, and arbitrary namespaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretName(String);

impl SecretName {
    pub fn parse(name: &str) -> Option<Self> {
        is_name(name).then(|| Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Attempt {
    pub fn same_generation(&self, other: &Self) -> bool {
        self.profile == other.profile
            && self.run == other.run
            && self.generation == other.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_tools_have_one_reversible_relay_name() {
        for id in ["my-skill.do-it", "memory.share", "a.b-2"] {
            let tool = ToolId(id.into());
            assert_eq!(ToolId::from_mcp_name(&tool.mcp_name()), Some(tool));
        }
        for name in [
            "delegate",
            "a_b_c",
            "a.b_c",
            "_read",
            "a_b--c",
            "mcp__other__a_b",
        ] {
            assert!(ToolId::from_mcp_name(name).is_none(), "{name}");
        }
    }
}

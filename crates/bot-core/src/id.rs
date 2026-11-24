//! 논리 ID는 재시도에서 유지하고 generation은 소유권 인계 때 바뀐다.
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
    /// 한 run 안에서 저장된 모델 차례의 첫 세션 레코드와 도구 순번을 가리킨다.
    pub fn cli(turn: u64, slot: u32) -> Self {
        Self(format!("cli-{turn}-{slot}"))
    }
}

impl ToolId {
    /// 프로필 도구의 skill.tool을 모델 릴레이의 MCP 이름으로 바꾼다.
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

/// 프로필·역할·스킬·도구·비밀 이름 한 조각에 쓰는 ASCII kebab-case.
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

/// 보관소 상대 이름 한 조각. 구분자·경로·임의 namespace를 표현할 수 없다.
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

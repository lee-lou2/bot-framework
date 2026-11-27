//! Size and count limits for profile loading; exceeding any limit rejects the entire profile.

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_profiles: usize,
    pub max_files: usize,
    pub max_dirs: usize,
    pub max_profile_bytes: u64,
    /// Sum of instructions, knowledge, skills, and memory across all assembled agents (including inherited copies).
    pub max_assembled_bytes: u64,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_yaml_bytes: u64,
    pub max_yaml_depth: usize,
    pub max_yaml_nodes: usize,
    pub max_yaml_scalar: usize,
    pub max_agents: usize,
    /// Root agent has depth 1.
    pub max_agent_depth: usize,
    pub max_skills: usize,
    /// Byte limit for total visible skill bodies (excluding front matter) per agent.
    pub max_skill_bytes: usize,
    pub max_tools: usize,
    pub max_triggers: usize,
    pub max_bindings: usize,
    pub max_members: usize,
    pub max_env_vars: usize,
    pub max_env_value: usize,
    pub max_memory_seeds: usize,
    pub max_issues: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_profiles: 64,
            max_files: 2_000,
            max_dirs: 1_000,
            max_profile_bytes: 16 * 1024 * 1024,
            max_assembled_bytes: 128 * 1024 * 1024,
            max_depth: 24,
            max_file_bytes: 1024 * 1024,
            max_yaml_bytes: 256 * 1024,
            max_yaml_depth: 32,
            max_yaml_nodes: 20_000,
            max_yaml_scalar: 64 * 1024,
            max_agents: 128,
            max_agent_depth: 8,
            max_skills: 64,
            max_skill_bytes: SKILL_MAX_BYTES,
            max_tools: 128,
            max_triggers: 128,
            max_bindings: 64,
            max_members: 1_000,
            max_env_vars: 128,
            max_env_value: 4 * 1024,
            max_memory_seeds: 1_000,
            max_issues: 100,
        }
    }
}

pub const KNOWLEDGE_MAX_DOCS: usize = 8;
pub const KNOWLEDGE_MAX_BYTES: usize = 32 * 1024;
/// Per-agent cap on total visible `SKILL.md` body bytes (excluding front matter).
pub const SKILL_MAX_BYTES: usize = 64 * 1024;

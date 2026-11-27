//! 입력 크기·개수 한도. 넘으면 읽던 것을 버리고 거절한다(일부만 받아들이지 않는다).

#[derive(Clone, Debug)]
pub struct Limits {
    /// BOT_PROFILES 아래 프로필 폴더 수
    pub max_profiles: usize,
    /// 프로필 하나의 파일·폴더 수와 총 바이트
    pub max_files: usize,
    pub max_dirs: usize,
    pub max_profile_bytes: u64,
    /// 상속을 적용한 뒤 모든 에이전트가 각자 들고 있는 지침·지식·스킬·기억의 합(상속으로 불어나는 복사본 포함)
    pub max_assembled_bytes: u64,
    /// 프로필 폴더 기준 경로 깊이(구성 요소 수)
    pub max_depth: usize,
    /// 파일 하나. YAML은 더 작게 본다.
    pub max_file_bytes: u64,
    pub max_yaml_bytes: u64,
    /// YAML 안의 중첩·노드 수·문자열 길이(별칭을 막았으므로 확장은 없다)
    pub max_yaml_depth: usize,
    pub max_yaml_nodes: usize,
    pub max_yaml_scalar: usize,
    /// 에이전트 수와 계보 깊이(루트가 1)
    pub max_agents: usize,
    pub max_agent_depth: usize,
    /// 범위(프로필 또는 에이전트) 하나의 스킬 수, 스킬 하나의 도구 수, 에이전트 하나의 트리거 수
    pub max_skills: usize,
    /// 에이전트 하나에게 보이는 스킬 본문(front matter 제외) 합계 상한(바이트)
    pub max_skill_bytes: usize,
    pub max_tools: usize,
    pub max_triggers: usize,
    pub max_bindings: usize,
    pub max_members: usize,
    /// env.yaml 항목 수·값 길이
    pub max_env_vars: usize,
    pub max_env_value: usize,
    /// 에이전트 하나의 기억 시드 수
    pub max_memory_seeds: usize,
    /// 한 번에 모으는 항목 수
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

/// 한 요청이 읽는 지식 문서의 상한(profile-spec). 검색 구현이 이 값을 쓴다.
pub const KNOWLEDGE_MAX_DOCS: usize = 8;
pub const KNOWLEDGE_MAX_BYTES: usize = 32 * 1024;
/// 에이전트 하나에게 보이는 스킬 본문(front matter 제외) 합계 상한(profile-spec).
/// 한국어·마크다운 실무 프로필의 멀티 스킬 여유를 확보하면서도 시스템 프롬프트 비대화를 막기 위해 64 KiB로 둔다.
pub const SKILL_MAX_BYTES: usize = 64 * 1024;

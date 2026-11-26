//! 프로필 하나의 파이프라인: 구조 → 파일 읽기 → 상속 조립 → 참조 검사 → 불변 판본.
//! 단계마다 오류를 모으고, 오류가 있으면 다음 단계로 가지 않는다.
use crate::{
    assemble, convert,
    issue::Issues,
    layout,
    limits::Limits,
    model::Built,
    parse::{self, SkillMode},
    resolve,
    scan::Tree,
    snapshot::{self, BundleFormat},
};
use bot_core::profile::Snapshot;

pub struct Output {
    pub snapshot: Snapshot,
    /// 검증한 바이트 묶음(저장용). 해시가 `snapshot.revision`이다.
    pub bundle: Vec<u8>,
}

/// 새 프로필 폴더를 현행(`Strict`) 규칙으로 검증해 판본을 만든다.
/// `SKILL.md` front matter를 쓰는 프로필은 `BOTPROFILE2`로 묶어 구판(`BOTPROFILE1`)과 판본을 가르고,
/// front matter가 없는 프로필은 `BOTPROFILE1`을 유지해 기존 배포의 판본을 바꾸지 않는다.
pub fn build(
    tree: &Tree,
    folder: Option<&str>,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Output> {
    build_inner(tree, folder, SkillMode::Strict, None, limits, issues)
}

/// 저장한 묶음 복원(`from_bundle`)이나 지정 판본 보충(`read_legacy_bundle`)용: 묶음 판본에 맞는 해석 규칙으로 검증한다.
pub fn build_for_bundle(
    format: BundleFormat,
    tree: &Tree,
    folder: Option<&str>,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Output> {
    let mode = match format {
        BundleFormat::V1 => SkillMode::Legacy,
        BundleFormat::V2 => SkillMode::Strict,
    };
    build_inner(tree, folder, mode, Some(format), limits, issues)
}

fn build_inner(
    tree: &Tree,
    folder: Option<&str>,
    mode: SkillMode,
    fixed_format: Option<BundleFormat>,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Output> {
    let layout = layout::discover(tree, limits, issues);
    if issues.has_errors() {
        return None;
    }
    let parsed = parse::parse_all_with(tree, &layout, folder, mode, limits, issues)?;
    let has_skill_front_matter = parsed.has_skill_front_matter;
    let built = assemble::assemble(parsed, issues)?;
    check_assembled(&built, mode, limits, issues);
    resolve::resolve(&built, issues);
    if issues.has_errors() {
        return None;
    }
    let format = fixed_format.unwrap_or(if has_skill_front_matter {
        BundleFormat::V2
    } else {
        BundleFormat::V1
    });
    let bundle = snapshot::encode_with(tree, format);
    let (revision, policy) = (
        snapshot::revision_of(&bundle),
        snapshot::policy_revision(&built),
    );
    Some(Output {
        snapshot: convert::to_core(built, revision, policy),
        bundle,
    })
}

/// 지식·스킬·지침은 상속으로 에이전트마다 복사본이 생긴다. 파일 크기 한도를 지켜도 에이전트가 많으면 불어나므로 합을 따로 막는다.
fn check_assembled(built: &Built, mode: SkillMode, limits: &Limits, issues: &mut Issues) {
    let total: u64 = built
        .agents
        .values()
        .map(|a| {
            let skills: usize = a
                .skills
                .iter()
                .map(|s| {
                    s.instructions.text.len()
                        + s.tools.iter().map(|t| t.input.0.len()).sum::<usize>()
                })
                .sum();
            a.parts.iter().map(String::len).sum::<usize>()
                + a.env.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
                + a.knowledge.iter().map(|d| d.text.len()).sum::<usize>()
                + a.memory
                    .iter()
                    .map(|m| m.document.text.len())
                    .sum::<usize>()
                + skills
        })
        .map(|n| n as u64)
        .sum();
    if total > limits.max_assembled_bytes {
        issues.error(
            "assembled-too-large",
            "",
            format!(
                "상속을 적용하면 에이전트별 복사본의 합이 {total} 바이트라 한도({})를 넘어요. 지식·스킬을 필요한 에이전트 아래로 옮기세요",
                limits.max_assembled_bytes
            ),
        );
    }
    if mode == SkillMode::Legacy {
        return;
    }
    let skill_bytes = |a: &crate::model::Agent| -> usize {
        a.skills.iter().map(|s| s.instructions.text.len()).sum()
    };
    for agent in built.agents.values() {
        let bytes = skill_bytes(agent);
        if bytes > limits.max_skill_bytes {
            // 부모가 이미 같은 합계로 한도를 넘겼으면 자식에게 중복 보고하지 않는다
            let inherited_same = agent
                .parent
                .as_ref()
                .and_then(|p| built.agents.get(p))
                .is_some_and(|parent| skill_bytes(parent) == bytes);
            if inherited_same {
                continue;
            }
            issues.error(
                "skill-too-large",
                &layout::agent_dir(&agent.id.0),
                format!(
                    "에이전트 {}에게 보이는 스킬 본문 합계가 {bytes} 바이트라 한도({} 바이트)를 넘어요. SKILL.md를 줄이거나 필요한 하위 에이전트 아래로 옮기세요",
                    agent.id.0, limits.max_skill_bytes
                ),
            );
        }
    }
}

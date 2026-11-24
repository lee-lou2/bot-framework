//! 프로필 하나의 파이프라인: 구조 → 파일 읽기 → 상속 조립 → 참조 검사 → 불변 판본.
//! 단계마다 오류를 모으고, 오류가 있으면 다음 단계로 가지 않는다.
use crate::{
    assemble, convert, issue::Issues, layout, limits::Limits, model::Built, parse, resolve,
    scan::Tree, snapshot,
};
use bot_core::profile::Snapshot;

pub struct Output {
    pub snapshot: Snapshot,
    /// 검증한 바이트 묶음(저장용). 해시가 `snapshot.revision`이다.
    pub bundle: Vec<u8>,
}

/// `folder`는 폴더 이름(profile.yaml의 id와 맞춰 본다). 묶음에서 다시 만들 때는 프로필 ID를 준다.
pub fn build(
    tree: &Tree,
    folder: Option<&str>,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Output> {
    let layout = layout::discover(tree, limits, issues);
    if issues.has_errors() {
        return None;
    }
    let parsed = parse::parse_all(tree, &layout, folder, limits, issues)?;
    let built = assemble::assemble(parsed, issues)?;
    check_assembled(&built, limits, issues);
    resolve::resolve(&built, issues);
    if issues.has_errors() {
        return None;
    }
    let bundle = snapshot::encode(tree);
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
fn check_assembled(built: &Built, limits: &Limits, issues: &mut Issues) {
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
}

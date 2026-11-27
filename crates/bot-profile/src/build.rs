//! Single-profile build pipeline from scanned file tree to immutable snapshot and bundle.
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
    /// Validated canonical bundle bytes whose SHA-256 is `snapshot.revision`.
    pub bundle: Vec<u8>,
}

/// Validates a profile under strict rules; uses `BOTPROFILE2` when `SKILL.md` front matter is
/// present and preserves `BOTPROFILE1` otherwise so existing deployments keep stable revisions.
pub fn build(
    tree: &Tree,
    folder: Option<&str>,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Output> {
    build_inner(tree, folder, SkillMode::Strict, None, limits, issues)
}

/// Validates a bundle tree using the skill rules matching its `BundleFormat`.
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
    resolve::resolve(&built, mode, issues);
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

/// Caps total inherited bytes across agents and per-agent visible skill bytes.
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
                "inherited agent copies total {total} bytes, exceeding the limit ({}); move knowledge or skills to narrower agents",
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
            // Skip duplicate report when the parent already exceeds the limit with the same byte total.
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
                    "visible skill instructions for agent {} total {bytes} bytes, exceeding the limit ({} bytes); trim SKILL.md or move skills to a narrower child agent",
                    agent.id.0, limits.max_skill_bytes
                ),
            );
        }
    }
}

//! Parsing and semantic validation of trigger `preapproved` and `human_approval` declarations.
use crate::{
    issue::Issues,
    model::{Binding, ProfileParts, Source, Tool, Trigger, TriggerTarget},
    names,
};
use bot_core::{
    grant::{
        HumanApproval, MAX_PAIRS, MAX_PER_DAY, Preapproval, piece_ok,
        reason::{INVALID, UNENFORCEABLE},
    },
    id::PrincipalId,
    profile::Adapter,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

#[derive(Default)]
pub struct Declared {
    pub preapproved: Option<Preapproval>,
    pub human_approval: Option<HumanApproval>,
}

const MAX_APPROVERS: usize = 16;

pub fn parse(obj: &Map<String, Value>, path: &str, at: &str, issues: &mut Issues) -> Declared {
    Declared {
        preapproved: obj
            .get("preapproved")
            .and_then(|v| preapproved(v, path, &format!("{at}/preapproved"), issues)),
        human_approval: obj
            .get("human_approval")
            .and_then(|v| human(v, path, &format!("{at}/human_approval"), issues)),
    }
}

fn bad(issues: &mut Issues, path: &str, at: &str, message: &str) -> Option<Preapproval> {
    issues.error_at(INVALID, path, at, message);
    None
}

fn preapproved(value: &Value, path: &str, at: &str, issues: &mut Issues) -> Option<Preapproval> {
    let Some(map) = value.as_object() else {
        return bad(
            issues,
            path,
            at,
            "preapproved must be a {max_per_day, targets} object",
        );
    };
    if let Some(extra) = map
        .keys()
        .find(|k| !["max_per_day", "targets"].contains(&k.as_str()))
    {
        return bad(
            issues,
            path,
            at,
            &format!(
                "preapproved has unknown key `{extra}` (only max_per_day and targets are allowed)"
            ),
        );
    }
    let Some(max) = map
        .get("max_per_day")
        .and_then(crate::schema::as_exact_u64)
        .filter(|n| (1..=u64::from(MAX_PER_DAY)).contains(n))
    else {
        return bad(
            issues,
            path,
            at,
            &format!("max_per_day must be an integer in 1..={MAX_PER_DAY}"),
        );
    };
    let Some(list) = map
        .get("targets")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty() && l.len() <= MAX_PAIRS)
    else {
        return bad(
            issues,
            path,
            at,
            &format!("targets must be a list of 1..={MAX_PAIRS} {{operation, target}} entries"),
        );
    };
    let mut targets = BTreeSet::new();
    for item in list {
        let pair = item.as_object().filter(|o| o.len() == 2).and_then(|o| {
            let operation = o.get("operation")?.as_str()?;
            let target = o.get("target")?.as_str()?;
            (piece_ok(operation) && piece_ok(target))
                .then(|| (operation.to_owned(), target.to_owned()))
        });
        let Some(pair) = pair else {
            return bad(
                issues,
                path,
                at,
                "each target entry must be an exact {operation, target} pair (no wildcards, prefixes, URLs, whitespace, or credential values)",
            );
        };
        if !targets.insert(pair) {
            return bad(
                issues,
                path,
                at,
                "targets contains a duplicate (operation, target) pair",
            );
        }
    }
    Some(Preapproval {
        max_per_day: u32::try_from(max).ok()?,
        targets: targets.into_iter().collect(),
    })
}

fn human(value: &Value, path: &str, at: &str, issues: &mut Issues) -> Option<HumanApproval> {
    let mut fail = |message: &str| {
        issues.error_at("invalid_human_approval", path, at, message);
        None
    };
    let Some(map) = value.as_object() else {
        return fail("human_approval must be an {approvers: [...]} object");
    };
    if map.len() != 1 {
        return fail("human_approval must only contain approvers");
    }
    let Some(list) = map
        .get("approvers")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty() && l.len() <= MAX_APPROVERS)
    else {
        return fail("approvers must be a list of 1..=16 human identities");
    };
    let mut seen = BTreeSet::new();
    for item in list {
        let Some(who) = item.as_str() else {
            return fail("each approver must be a `slack:<workspace>/<user>` string");
        };
        if !seen.insert(who.to_owned()) {
            return fail("approvers contains a duplicate identity");
        }
    }
    Some(HumanApproval {
        approvers: seen.into_iter().map(PrincipalId).collect(),
    })
}

pub fn check(
    p: &ProfileParts,
    t: &Trigger,
    tool: Option<&Tool>,
    binding: Option<&Binding>,
    (file, at): (&str, &str),
    issues: &mut Issues,
) {
    if t.preapproved.is_some() {
        preapproved_rules(t, tool, (file, at), issues);
    }
    if let Some(human) = &t.human_approval {
        human_rules(p, t, human, binding, (file, at), issues);
    }
}

fn preapproved_rules(
    t: &Trigger,
    tool: Option<&Tool>,
    (file, at): (&str, &str),
    issues: &mut Issues,
) {
    let mut invalid = |why: &str| issues.error_at(INVALID, file, at, why);
    if !matches!(t.on, Source::Cron { beat: false, .. }) {
        invalid("preapproved is only allowed on non-beat cron triggers");
    }
    if !matches!(t.target, TriggerTarget::Tool { .. }) {
        invalid(
            "preapproved is only allowed on direct tool triggers (`target.tool`) without a model",
        );
    }
    if !t.input.is_empty() {
        invalid("preapproved cannot be combined with input mappings (args must be static)");
    }
    if t.private {
        invalid("preapproved is not allowed on private triggers");
    }
    if t.human_approval.is_some() {
        invalid("preapproved and human_approval cannot be used together");
    }
    if let Some(tool) = tool {
        // Requires an effect-ledger tool to guarantee revision pinning, target binding, and `effect.request` mediation.
        if !tool.effects {
            issues.error_at(
                UNENFORCEABLE,
                file,
                at,
                "preapproved requires a tool with `effects: true` (all writes routed through effect.request)",
            );
        }
    }
}

fn human_rules(
    p: &ProfileParts,
    t: &Trigger,
    human: &HumanApproval,
    binding: Option<&Binding>,
    (file, at): (&str, &str),
    issues: &mut Issues,
) {
    let mut invalid = |why: &str| issues.error_at("invalid_human_approval", file, at, why);
    // Only mention/DM messages, replies in bot-participating threads, and reactions can act as human approval.
    let confirm = match &t.on {
        Source::Message { .. } | Source::ThreadReply { .. } => Some("body.text"),
        Source::Reaction { .. } => Some("body.reaction"),
        Source::Cron { .. } | Source::Http { .. } => None,
    };
    if confirm.is_none() {
        invalid("human_approval is only allowed on Slack event triggers (`on.event`)");
    }
    // Require an exact confirmation phrase (`body.text`) or emoji (`body.reaction`) so arbitrary events cannot approve.
    if let Some(path) = confirm {
        let pinned = t.matches.iter().any(|m| {
            m.path == path
                && m.value
                    .as_str()
                    .is_some_and(|literal| !literal.trim().is_empty())
        });
        if !pinned {
            invalid(&format!(
                "human_approval triggers must pin an exact confirmation phrase or emoji via `match: {{{path}: <exact value>}}`"
            ));
        }
    }
    if !matches!(t.target, TriggerTarget::Tool { .. }) {
        invalid(
            "human_approval is only allowed on direct tool triggers (`target.tool`) without a model",
        );
    }
    if t.private {
        invalid("human_approval is not allowed on private triggers");
    }
    let workspace = match binding.map(|b| &b.kind) {
        Some(Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace }) => {
            Some(workspace.as_str())
        }
        _ => None,
    };
    for who in &human.approvers {
        let kind = names::identity_kind(&who.0);
        let prefix = workspace.map(|w| format!("slack:{w}/"));
        if kind != Some(names::IdentityKind::Slack)
            || !prefix.is_some_and(|prefix| who.0.starts_with(&prefix))
        {
            invalid(&format!(
                "approver `{}` must be a Slack human identity in this binding's workspace (`slack:<workspace>/<user>`)",
                who.0
            ));
            continue;
        }
        if !p.members.contains_key(who) {
            invalid(&format!(
                "approver `{}` is not listed in profile.yaml members",
                who.0
            ));
        }
    }
}

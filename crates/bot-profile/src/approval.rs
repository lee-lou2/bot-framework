//! triggers.yaml의 `preapproved`(운영자가 사전 승인한 정적 호출)와 `human_approval`(지정한 사람의 메시지·반응 승인).
//! 모양은 파일 안에서(`parse`), 도구·바인딩·멤버가 얽힌 규칙은 해석 단계에서(`check`) 본다. 코드는 계약의 고정 사유를 그대로 쓴다.
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

/// 한 트리거에서 읽은 두 선언.
#[derive(Default)]
pub struct Declared {
    pub preapproved: Option<Preapproval>,
    pub human_approval: Option<HumanApproval>,
}

/// 지정한 사람의 승인(`human_approval`) 승인자 목록의 최대 길이.
const MAX_APPROVERS: usize = 16;

/// `preapproved`·`human_approval`의 모양을 읽는다. 어긋나면 고정 코드로 보고하고 해당 선언은 없는 것으로 둔다(오류가 이미 났다).
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
            "preapproved는 {max_per_day, targets} 객체여야 해요",
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
            &format!("preapproved에 알 수 없는 키 `{extra}`가 있어요(max_per_day·targets만 써요)"),
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
            &format!("max_per_day는 1..={MAX_PER_DAY} 정수여야 해요"),
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
            &format!("targets는 1..={MAX_PAIRS}개의 {{operation, target}} 목록이어야 해요"),
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
                "targets의 각 항목은 정확한 {operation, target}이어야 해요(와일드카드·접두어·URL·공백·자격 값은 쓸 수 없어요)",
            );
        };
        if !targets.insert(pair) {
            return bad(
                issues,
                path,
                at,
                "targets에 같은 (operation, target)이 두 번 있어요",
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
        return fail("human_approval은 {approvers: [...]} 객체여야 해요");
    };
    if map.len() != 1 {
        return fail("human_approval은 approvers만 가져요");
    }
    let Some(list) = map
        .get("approvers")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty() && l.len() <= MAX_APPROVERS)
    else {
        return fail("approvers는 1..=16명의 사람 신원 목록이어야 해요");
    };
    let mut seen = BTreeSet::new();
    for item in list {
        let Some(who) = item.as_str() else {
            return fail("approvers의 각 항목은 `slack:<워크스페이스>/<사용자>` 문자열이어야 해요");
        };
        if !seen.insert(who.to_owned()) {
            return fail("approvers에 같은 사람이 두 번 있어요");
        }
    }
    Some(HumanApproval {
        approvers: seen.into_iter().map(PrincipalId).collect(),
    })
}

/// 도구·바인딩·멤버를 아는 해석 단계의 규칙. `tool`은 트리거가 부르는 고정 도구(있으면).
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
        invalid("preapproved는 beat가 아닌 cron 트리거에만 쓸 수 있어요");
    }
    if !matches!(t.target, TriggerTarget::Tool { .. }) {
        invalid("preapproved는 모델 없이 고정 도구를 부르는 트리거(target.tool)에만 쓸 수 있어요");
    }
    if !t.input.is_empty() {
        invalid("preapproved는 input 매핑과 함께 쓸 수 없어요(인자는 고정이어야 해요)");
    }
    if t.private {
        invalid("preapproved는 private 트리거에 쓸 수 없어요");
    }
    if t.human_approval.is_some() {
        invalid("preapproved와 human_approval은 함께 쓸 수 없어요");
    }
    if let Some(tool) = tool {
        // 판본 고정·대상 결박·쓰기 경유(`effect.request`)를 보증하려면 단계 원장을 쓰는 도구여야 한다
        if !tool.effects {
            issues.error_at(
                UNENFORCEABLE,
                file,
                at,
                "preapproved는 `effects: true`인 도구(모든 쓰기가 effect.request를 거치는 도구)에만 쓸 수 있어요",
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
    // 멘션·DM 메시지, 봇이 참여한 스레드의 답글, 반응(행위자 = 반응한 사람) 이벤트 트리거만 승인이 될 수 있다
    let confirm = match &t.on {
        Source::Message { .. } | Source::ThreadReply { .. } => Some("body.text"),
        Source::Reaction { .. } => Some("body.reaction"),
        Source::Cron { .. } | Source::Http { .. } => None,
    };
    if confirm.is_none() {
        invalid("human_approval은 Slack 이벤트 트리거(on.event)에만 쓸 수 있어요");
    }
    // 아무 이벤트나 승인이 되지 않게 정확한 확인 문구(메시지·답글: body.text)나 이모지(반응: body.reaction)를 match로 고정해야 한다
    if let Some(path) = confirm {
        let pinned = t.matches.iter().any(|m| {
            m.path == path
                && m.value
                    .as_str()
                    .is_some_and(|literal| !literal.trim().is_empty())
        });
        if !pinned {
            invalid(&format!(
                "human_approval 트리거는 정확한 확인 문구나 이모지를 `match: {{{path}: <정확한 값>}}`으로 지정해야 해요"
            ));
        }
    }
    if !matches!(t.target, TriggerTarget::Tool { .. }) {
        invalid(
            "human_approval은 모델 없이 고정 도구를 부르는 트리거(target.tool)에만 쓸 수 있어요",
        );
    }
    if t.private {
        invalid("human_approval은 private 트리거에 쓸 수 없어요");
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
                "승인자 `{}`는 이 바인딩의 워크스페이스의 Slack 사람 신원(`slack:<워크스페이스>/<사용자>`)이어야 해요",
                who.0
            ));
            continue;
        }
        if !p.members.contains_key(who) {
            invalid(&format!(
                "승인자 `{}`가 profile.yaml의 members에 없어요",
                who.0
            ));
        }
    }
}

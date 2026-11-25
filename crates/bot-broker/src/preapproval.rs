//! 사전 승인(F1)·지정한 사람의 승인(F4)의 순수 판정 재료.
//! 활성화(운영자가 `--activate`할 때 승인 ID를 발급하는 쪽)와 호출 시작(브로커)이 **같은 계산**으로 지문을 만든다.
//! 어느 쪽도 자격 값을 보지 않는다: 대상 매핑은 선언한 자격 이름·선택 환경·egress·목적지의 지문이다.
//! 같은 이름의 비밀이 다른 목적지로 재지정되면 이름이 그대로라 이 지문이 바뀌지 않는다 — 운영자가 자격 이름을 바꿔야 새 승인이 필요해진다.

use crate::resolve::find_tool;
use bot_core::{
    canon::{canonicalize, sha256},
    grant::{Authority, GrantInputs, GrantSpec, Preapproval},
    id::*,
    json::parse,
    profile::{Destination, Snapshot, Tool, Trigger, TriggerSource, TriggerTarget},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// 도구가 해석하는 대상 매핑의 지문(선언한 자격 **이름**·선택한 환경 이름과 값·egress·목적지·서버 별칭).
pub fn mapping_fingerprint(tool: &Tool, env: &[(String, String)]) -> [u8; 32] {
    let destination = match &tool.destination {
        None => json!(null),
        Some(Destination::Reply) => json!("reply"),
        Some(Destination::Explicit { recipient, content }) => {
            json!({"recipient": recipient, "content": content})
        }
    };
    let material = json!({
        "v": 1,
        "server": tool.server,
        "secrets": tool.secrets.iter().map(|(env, name)| (env.clone(), name.as_str().to_owned())).collect::<std::collections::BTreeMap<_, _>>(),
        "env": env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
        "egress": tool.egress,
        "destination": destination,
    });
    Sha256::digest(material.to_string().as_bytes()).into()
}

/// 정적 cron 트리거의 승인 지문 재료. `trigger.preapproved`가 없거나 고정 도구 cron이 아니면 `None`이다.
/// `scopes`는 트리거 주체의 실행 스코프(활성화 때 정한 값), `image`는 설치한 실행물 판본, `env`는 도구가 고른 환경.
pub fn grant_spec(
    snapshot: &Snapshot,
    agent: &AgentId,
    trigger: &Trigger,
    scopes: &BTreeSet<String>,
    image: &str,
    env: &[(String, String)],
) -> Option<GrantSpec> {
    let pre: &Preapproval = trigger.preapproved.as_ref()?;
    let TriggerSource::Cron {
        schedule,
        beat: false,
        principal,
        ..
    } = &trigger.on
    else {
        return None;
    };
    let TriggerTarget::Tool { tool: id, args } = &trigger.target else {
        return None;
    };
    if !trigger.input.is_empty() || trigger.private || trigger.human_approval.is_some() {
        return None;
    }
    let tool = find_tool(snapshot, agent, id)?;
    if !tool.effects {
        return None;
    }
    let args_sha256 = sha256(&canonicalize(&parse(&args.0)?)?);
    let source = format!("cron:{schedule}@{};beat=false", snapshot.timezone);
    let revision = Revision(format!("{}@{image}", tool.revision.0));
    Some(GrantSpec::new(&GrantInputs {
        profile: &snapshot.id,
        agent,
        trigger: &trigger.id,
        source: &source,
        principal,
        scopes,
        tool: id,
        tool_revision: &revision,
        args_sha256,
        preapproval: pre,
        mapping: mapping_fingerprint(tool, env),
    }))
}

/// 호출을 시작할 때 쓸 수 있는 권한 선언의 종류.
#[derive(Debug, Eq, PartialEq)]
pub enum Claim {
    /// 사전 승인된 정적 직접 호출(지문은 지금 선언·설치 판본에서 다시 계산한 값).
    Grant(Authority),
    /// 지정한 사람의 증거로 승인을 대신할 수 있는 직접 호출(승인자 확인은 호출자가 한다).
    Human,
}

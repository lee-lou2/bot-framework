//! 도구 선언 해석. 호출이 가리키는 도구를 실행이 고정한 판본과 지금 활성 판본에서 찾아 합친다.

use crate::egress::parse_target;
use bot_core::{
    id::*,
    port::Error,
    profile::{Destination, Snapshot, Tool},
};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

impl HostPort {
    /// `host:port` 선언을 나눈다. 정규화할 수 없으면 `None`.
    pub fn parse(text: &str) -> Option<Self> {
        parse_target(text).map(|(host, port)| Self { host, port })
    }
}

/// 실행에 쓰는 도구. `tool`은 고정 판본과 현재 판본을 합친 선언이다.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub id: ToolId,
    /// MCP 도구 이름(tools.yaml tools[].id). 도구 ID `skill.tool`의 `tool` 부분이다.
    pub name: String,
    pub tool: Tool,
    pub egress: Vec<HostPort>,
}

impl Resolved {
    pub fn new(id: ToolId, tool: Tool) -> Option<Self> {
        let name = id.0.split_once('.')?.1.to_owned();
        let egress = tool
            .egress
            .iter()
            .filter_map(|e| HostPort::parse(e))
            .collect();
        Some(Self {
            id,
            name,
            tool,
            egress,
        })
    }
}

/// 승인에 묶이는 도구 판본에 설치한 서버 판본(이미지)을 덧붙인다. 승인 뒤 설치가 바뀌면 판본이 달라져 승인이 무효가 된다.
pub fn pinned_to_image(mut tool: Tool, image: &str) -> Tool {
    tool.revision = Revision(format!("{}@{image}", tool.revision.0));
    tool
}

/// 에이전트가 볼 수 있는 도구(조상에게서 물려받은 스킬 포함)를 찾는다.
pub fn find_tool<'a>(snapshot: &'a Snapshot, agent: &AgentId, tool: &ToolId) -> Option<&'a Tool> {
    snapshot
        .agents
        .get(agent)?
        .skills
        .values()
        .find_map(|skill| skill.tools.get(tool))
}

fn same_destination(a: &Option<Destination>, b: &Option<Destination>) -> bool {
    match (a, b) {
        (None, None) | (Some(Destination::Reply), Some(Destination::Reply)) => true,
        (
            Some(Destination::Explicit {
                recipient: ar,
                content: ac,
            }),
            Some(Destination::Explicit {
                recipient: br,
                content: bc,
            }),
        ) => ar == br && ac == bc,
        _ => false,
    }
}

/// 도구가 고른 환경 이름(`Tool.env`)의 값을 호출하는 에이전트의 환경(`Agent.env`, 상속을 마친 값)에서 읽는다. 고정 판본과 현재 판본
/// **둘 다**에 이름이 있고 값이 같을 때만 값을 준다(어느 쪽에 없거나 값이 다르면 `None`: 선택한 값의 변화는 승인한 대상의 변경이라
/// 이전 실행의 도구 시작을 거절한다). 호스트 환경(`process.env`)으로는 채우지 않는다. 이름 순이다.
pub fn selected_env(
    tool: &Tool,
    agent: &AgentId,
    pinned: &Snapshot,
    current: &Snapshot,
) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    for name in &tool.env {
        let then = pinned.agents.get(agent)?.env.get(name)?;
        let now = current.agents.get(agent)?.env.get(name)?;
        if then != now {
            return None;
        }
        out.push((name.clone(), then.clone()));
    }
    Some(out)
}

/// 실행이 고정한 판본(`pinned`)과 지금 활성인 판본(`current`)의 선언을 합친다.
/// 권한 쪽 필드는 더 엄격한 쪽을 따르고(등급은 높은 쪽, 스코프는 합집합, egress·자격은 교집합),
/// 실행 정의(서버·입력 스키마·목적지·콜백)가 다르면 거절한다. 그 실행은 새 판본으로 다시 시작해야 한다.
pub fn effective_tool(pinned: &Tool, current: &Tool) -> Result<Tool, Error> {
    if pinned.revision == current.revision {
        return Ok(pinned.clone());
    }
    let same_definition = pinned.server == current.server
        && pinned.effects == current.effects
        && same_destination(&pinned.destination, &current.destination)
        && pinned.input == current.input
        && pinned.env == current.env;
    if !same_definition {
        return Err(Error::Conflict);
    }
    let mut merged = pinned.clone();
    merged.revision = Revision(format!("{}~{}", pinned.revision.0, current.revision.0));
    merged.class = pinned.class.max(current.class);
    merged.scopes = pinned.scopes.union(&current.scopes).cloned().collect();
    merged.external = pinned.external || current.external;
    merged.egress = pinned
        .egress
        .intersection(&current.egress)
        .cloned()
        .collect::<BTreeSet<_>>();
    merged
        .secrets
        .retain(|env, name| current.secrets.get(env) == Some(name));
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tool;
    use bot_core::{command::JsonPayload, state::Risk};

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mcp_name_is_the_part_after_the_skill() {
        let resolved =
            Resolved::new(ToolId("agenda.delete".into()), tool(Risk::Read, &["a"])).unwrap();
        assert_eq!(resolved.name, "delete");
        assert!(Resolved::new(ToolId("nodot".into()), tool(Risk::Read, &["a"])).is_none());
    }

    #[test]
    fn egress_entries_are_normalized_and_bad_ones_dropped() {
        let mut t = tool(Risk::Read, &["a"]);
        t.egress = set(&["API.example:443", "bad host:1", "x.example:0"]);
        let resolved = Resolved::new(ToolId("a.b".into()), t).unwrap();
        assert_eq!(
            resolved.egress,
            vec![HostPort {
                host: "api.example".into(),
                port: 443
            }]
        );
    }

    #[test]
    fn same_revision_keeps_the_pinned_tool() {
        let pinned = tool(Risk::Read, &["a"]);
        let merged = effective_tool(&pinned, &pinned.clone()).unwrap();
        assert_eq!(merged.revision, pinned.revision);
        assert_eq!(merged.scopes, pinned.scopes);
    }

    #[test]
    fn changed_revision_takes_the_stricter_authority() {
        let mut pinned = tool(Risk::Write, &["a"]);
        pinned.egress = set(&["api.example:443", "old.example:443"]);
        pinned.secrets.insert(
            "TOKEN".into(),
            bot_core::id::SecretName::parse("reader").unwrap(),
        );
        pinned.secrets.insert(
            "OTHER".into(),
            bot_core::id::SecretName::parse("other").unwrap(),
        );
        let mut current = pinned.clone();
        current.revision = Revision("t2".into());
        current.class = Risk::Irreversible;
        current.scopes = set(&["b"]);
        current.external = true;
        current.egress = set(&["api.example:443"]);
        current.secrets.remove("OTHER");
        let merged = effective_tool(&pinned, &current).unwrap();
        assert_eq!(merged.class, Risk::Irreversible);
        assert_eq!(merged.scopes, set(&["a", "b"]));
        assert!(merged.external);
        assert_eq!(merged.egress, set(&["api.example:443"]));
        assert_eq!(merged.secrets.keys().collect::<Vec<_>>(), vec!["TOKEN"]);
        assert_eq!(merged.revision.0, "t1~t2");
    }

    #[test]
    fn a_changed_definition_cannot_be_merged() {
        let pinned = tool(Risk::Read, &["a"]);
        let changes: Vec<fn(&mut Tool)> = vec![
            |t| t.server = "other".into(),
            |t| t.effects = true,
            |t| t.input = JsonPayload("{}".into()),
            |t| t.destination = Some(Destination::Reply),
        ];
        for change in changes {
            let mut current = pinned.clone();
            current.revision = Revision("t2".into());
            change(&mut current);
            assert!(matches!(
                effective_tool(&pinned, &current),
                Err(Error::Conflict)
            ));
        }
        // 목적지 경로가 달라져도 합치지 않는다
        let mut a = pinned.clone();
        a.destination = Some(Destination::Explicit {
            recipient: "args.to".into(),
            content: "args.body".into(),
        });
        let mut b = a.clone();
        b.revision = Revision("t2".into());
        b.destination = Some(Destination::Explicit {
            recipient: "args.cc".into(),
            content: "args.body".into(),
        });
        assert!(matches!(effective_tool(&a, &b), Err(Error::Conflict)));
    }
}

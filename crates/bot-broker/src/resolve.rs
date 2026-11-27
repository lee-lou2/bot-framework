//! Tool resolution merging pinned and active profile revisions under stricter-authority rules.

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
    pub fn parse(text: &str) -> Option<Self> {
        parse_target(text).map(|(host, port)| Self { host, port })
    }
}

/// Resolved tool ready for execution (`tool` merges pinned and current revisions).
#[derive(Clone, Debug)]
pub struct Resolved {
    pub id: ToolId,
    /// Server-exposed MCP tool name (`tools.yaml` `name`, or the `<tool>` suffix of `skill.tool`).
    pub name: String,
    pub tool: Tool,
    pub egress: Vec<HostPort>,
}

impl Resolved {
    pub fn new(id: ToolId, tool: Tool) -> Option<Self> {
        let name = match &tool.mcp_name {
            Some(given) => given.clone(),
            None => id.0.split_once('.')?.1.to_owned(),
        };
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

/// Appends the installed server revision to the tool revision so server upgrades invalidate prior approvals.
pub fn pinned_to_image(mut tool: Tool, image: &str) -> Tool {
    tool.revision = Revision(format!("{}@{image}", tool.revision.0));
    tool
}

/// Looks up a tool visible to `agent` (including inherited skills).
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

/// Reads `tool.env` entries from `agent.env`, requiring identical values in both `pinned` and `current` snapshots.
/// Never falls back to host `process.env`; returns `None` if any selected key is missing or changed.
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

/// Merges `pinned` and `current` tool declarations using the stricter authority (max risk, union scopes,
/// OR external, intersect egress/secrets); returns `Conflict` if execution definitions differ.
pub fn effective_tool(pinned: &Tool, current: &Tool) -> Result<Tool, Error> {
    if pinned.revision == current.revision {
        return Ok(pinned.clone());
    }
    let same_definition = pinned.server == current.server
        && pinned.effects == current.effects
        && same_destination(&pinned.destination, &current.destination)
        && pinned.input == current.input
        && pinned.definition == current.definition
        && pinned.mcp_name == current.mcp_name
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
    fn an_explicit_server_name_wins_over_the_id_part() {
        let mut named = tool(Risk::Read, &["a"]);
        named.mcp_name = Some("get_forecast".into());
        let resolved = Resolved::new(ToolId("weather.get-forecast".into()), named).unwrap();
        assert_eq!(resolved.name, "get_forecast");
        assert_eq!(resolved.id.0, "weather.get-forecast");
        let plain = Resolved::new(
            ToolId("weather.get-forecast".into()),
            tool(Risk::Read, &["a"]),
        )
        .unwrap();
        assert_eq!(plain.name, "get-forecast");
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
            |t| t.definition = Some("b".repeat(64)),
            |t| t.mcp_name = Some("get_forecast".into()),
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

    #[test]
    fn a_pinned_definition_hash_must_match_exactly_to_merge() {
        let mut pinned = tool(Risk::Read, &["a"]);
        pinned.definition = Some("a".repeat(64));
        let mut same = pinned.clone();
        same.revision = Revision("t2".into());
        same.scopes = set(&["b"]);
        let merged = effective_tool(&pinned, &same).unwrap();
        assert_eq!(merged.definition, pinned.definition);
        for other in [Some("b".repeat(64)), None] {
            let mut current = same.clone();
            current.definition = other;
            assert!(matches!(
                effective_tool(&pinned, &current),
                Err(Error::Conflict)
            ));
        }
        let mut inline = pinned.clone();
        inline.definition = None;
        let mut locked = inline.clone();
        locked.revision = Revision("t2".into());
        locked.definition = Some("a".repeat(64));
        assert!(matches!(
            effective_tool(&inline, &locked),
            Err(Error::Conflict)
        ));
    }
}

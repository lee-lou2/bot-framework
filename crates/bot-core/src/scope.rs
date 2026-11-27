//! Scope authorization: requester scopes ∩ agent ceiling ∩ profile ceiling must cover every required tool scope.

use crate::{
    id::*,
    profile::{Scopes, Snapshot},
    state::RunContext,
};
use std::collections::BTreeMap;

#[derive(Debug, Eq, PartialEq)]
pub enum Denial {
    /// Tool declared no scopes; fail closed for everyone.
    Undeclared,
    Member,
    Agent,
    Missing(Vec<String>),
}

impl Denial {
    pub fn code(&self) -> &'static str {
        match self {
            Denial::Undeclared => "scope_undeclared",
            Denial::Member => "member_unknown",
            Denial::Agent => "agent_unknown",
            Denial::Missing(_) => "scope_missing",
        }
    }
}

/// Union of scopes from the principal's current roles, or `None` if not a member.
pub fn role_scopes(snapshot: &Snapshot, principal: &PrincipalId) -> Option<Scopes> {
    let roles = snapshot.members.get(principal)?;
    Some(
        roles
            .iter()
            .filter_map(|role| snapshot.roles.get(role))
            .flatten()
            .cloned()
            .collect(),
    )
}

/// Intersects policy ceilings along all ancestor segments of `agent` (inclusive); a child cannot exceed its parent.
pub fn agent_ceiling(agents: &BTreeMap<AgentId, Scopes>, agent: &AgentId) -> Option<Scopes> {
    let mut ceiling: Option<Scopes> = None;
    let mut path = String::new();
    for part in agent.0.split('/') {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(part);
        if let Some(limit) = agents.get(&AgentId(path.clone())) {
            ceiling = Some(match ceiling {
                None => limit.clone(),
                Some(upper) => upper.intersection(limit).cloned().collect(),
            });
        }
    }
    ceiling
}

/// Current-revision scopes for `(principal, agent)` (roles ∩ profile policy ∩ agent ceiling); used to pin scopes at intake.
pub fn effective(snapshot: &Snapshot, principal: &PrincipalId, agent: &AgentId) -> Option<Scopes> {
    let roles = role_scopes(snapshot, principal)?;
    let own = snapshot.agents.get(agent)?;
    let ceiling = agent_ceiling(&snapshot.policy.agents, agent);
    Some(
        roles
            .into_iter()
            .filter(|scope| {
                snapshot.policy.scopes.contains(scope)
                    && own.scopes.contains(scope)
                    && ceiling.as_ref().is_none_or(|upper| upper.contains(scope))
            })
            .collect(),
    )
}

/// Verifies `required` against both `snapshot` (active) and `pinned` (run's fixed revision).
/// Enforces `Agent.scopes` on both revisions so neither stale intake scopes nor widened policies can escalate access.
pub fn authorize(
    required: &Scopes,
    ctx: &RunContext,
    snapshot: &Snapshot,
    pinned: &Snapshot,
) -> Result<(), Denial> {
    if required.is_empty() {
        return Err(Denial::Undeclared);
    }
    let roles = role_scopes(snapshot, &ctx.principal).ok_or(Denial::Member)?;
    let now = snapshot.agents.get(&ctx.agent).ok_or(Denial::Agent)?;
    let then = pinned.agents.get(&ctx.agent).ok_or(Denial::Agent)?;
    let ceiling = agent_ceiling(&snapshot.policy.agents, &ctx.agent);
    let missing: Vec<String> = required
        .iter()
        .filter(|scope| {
            !(ctx.scopes.contains(*scope)
                && roles.contains(*scope)
                && snapshot.policy.scopes.contains(*scope)
                && now.scopes.contains(*scope)
                && then.scopes.contains(*scope)
                && ceiling.as_ref().is_none_or(|upper| upper.contains(*scope)))
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Denial::Missing(missing))
    }
}

/// Baseline scope required to run an agent, including delegated child runs.
pub const RUN_SCOPE: &str = "run";

/// Authorizes delegation to a direct child and returns its narrowed scope set (used on start, resume, and result recall).
/// Requires direct parentage in both `current` and `pinned`, active membership, and `run` inside `ctx.scopes` ∩ child effective scopes of both revisions.
pub fn delegation_scopes(
    ctx: &RunContext,
    current: &Snapshot,
    pinned: &Snapshot,
    child: &AgentId,
) -> Result<Scopes, Denial> {
    let direct = |snapshot: &Snapshot| {
        snapshot
            .agents
            .get(child)
            .is_some_and(|agent| agent.parent.as_ref() == Some(&ctx.agent))
    };
    if !direct(current) || !direct(pinned) {
        return Err(Denial::Agent);
    }
    let now = effective(current, &ctx.principal, child).ok_or(Denial::Member)?;
    let then = effective(pinned, &ctx.principal, child).ok_or(Denial::Member)?;
    let scopes: Scopes = ctx
        .scopes
        .iter()
        .filter(|scope| now.contains(*scope) && then.contains(*scope))
        .cloned()
        .collect();
    if scopes.contains(RUN_SCOPE) {
        Ok(scopes)
    } else {
        Err(Denial::Missing(vec![RUN_SCOPE.to_owned()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command::{JsonPayload, Target, Work},
        profile::{Agent, Policy},
        state::{Conversation, ConversationKind, RunOrigin},
    };
    use std::collections::BTreeSet;

    fn set(items: &[&str]) -> Scopes {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn agent(parent: Option<&str>, scopes: Scopes) -> Agent {
        Agent {
            description: String::new(),
            builtin: BTreeSet::new(),
            parent: parent.map(|p| AgentId(p.to_owned())),
            model: "m".to_owned(),
            instructions: String::new(),
            env: BTreeMap::new(),
            scopes,
            skills: BTreeMap::new(),
            knowledge: Vec::new(),
            memory_seeds: Vec::new(),
            triggers: Vec::new(),
        }
    }

    struct WorldBuilder {
        snapshot: Snapshot,
    }

    impl WorldBuilder {
        fn new(profile: &str, revision: &str) -> Self {
            Self {
                snapshot: Snapshot {
                    id: ProfileId(profile.to_owned()),
                    revision: Revision(revision.to_owned()),
                    policy_revision: Revision(revision.to_owned()),
                    timezone: "UTC".to_owned(),
                    model: "m".to_owned(),
                    root: AgentId("main".to_owned()),
                    agents: BTreeMap::new(),
                    bindings: BTreeMap::new(),
                    roles: BTreeMap::new(),
                    members: BTreeMap::new(),
                    super_admins: BTreeSet::new(),
                    policy: Policy {
                        scopes: BTreeSet::new(),
                        agents: BTreeMap::new(),
                        approve: BTreeSet::new(),
                    },
                    secrets: Vec::new(),
                },
            }
        }

        fn role(mut self, name: &str, scopes: &[&str]) -> Self {
            self.snapshot.roles.insert(name.to_owned(), set(scopes));
            self
        }

        fn member(mut self, principal: &str, roles: &[&str]) -> Self {
            self.snapshot
                .members
                .insert(PrincipalId(principal.to_owned()), set(roles));
            self
        }

        fn policy_scopes(mut self, scopes: &[&str]) -> Self {
            self.snapshot.policy.scopes = set(scopes);
            self
        }

        fn agent_with(mut self, id: &str, scopes: Scopes) -> Self {
            let parent = id.rsplit_once('/').map(|(p, _)| p);
            self.snapshot
                .agents
                .insert(AgentId(id.to_owned()), agent(parent, scopes));
            self
        }

        fn agent(self, id: &str) -> Self {
            let scopes = self.snapshot.policy.scopes.clone();
            self.agent_with(id, scopes)
        }

        fn agent_scopes(self, id: &str, scopes: &[&str]) -> Self {
            self.agent_with(id, set(scopes))
        }

        fn agent_limit(mut self, id: &str, scopes: &[&str]) -> Self {
            self.snapshot
                .policy
                .agents
                .insert(AgentId(id.to_owned()), set(scopes));
            if !self.snapshot.agents.contains_key(&AgentId(id.to_owned())) {
                let all = self.snapshot.policy.scopes.clone();
                self = self.agent_with(id, all);
            }
            self
        }

        fn build(mut self) -> Snapshot {
            if !self.snapshot.agents.contains_key(&AgentId("main".into()))
                && !self.snapshot.policy.scopes.is_empty()
            {
                let all = self.snapshot.policy.scopes.clone();
                self = self.agent_with("main", all);
            }
            self.snapshot
        }
    }

    fn world(profile: &[&str], owner: &[&str]) -> Snapshot {
        WorldBuilder::new("personal", "r1")
            .role("owner", owner)
            .member("local:owner", &["owner"])
            .policy_scopes(profile)
            .agent("main/research")
            .build()
    }

    fn run_context(
        profile: &str,
        run: &str,
        generation: u64,
        principal: &str,
        agent: &str,
        scopes: &[&str],
    ) -> RunContext {
        RunContext {
            attempt: Attempt {
                profile: ProfileId(profile.to_owned()),
                run: RunId(run.to_owned()),
                claim: Some(ClaimEpoch(1)),
                generation: Generation(generation),
            },
            root: RunId(run.to_owned()),
            origin: RunOrigin::Message,
            agent: AgentId(agent.to_owned()),
            principal: PrincipalId(principal.to_owned()),
            binding: BindingId("cli".to_owned()),
            api: None,
            conversation: Conversation {
                kind: ConversationKind::Cli,
                id: "c".to_owned(),
                thread: None,
            },
            scopes: set(scopes),
            private_memory_read: false,
            work: Work {
                revision: Revision("r1".to_owned()),
                target: Target::Agent {
                    agent: AgentId(agent.to_owned()),
                    instruction: String::new(),
                },
                input: JsonPayload("{}".to_owned()),
                attachments: Vec::new(),
                steps: Vec::new(),
                pass_pos: None,
            },
            approver: None,
            trigger: None,
        }
    }

    fn ctx(scopes: &[&str]) -> RunContext {
        run_context("personal", "r1", 1, "local:owner", "main", scopes)
    }

    #[test]
    fn allows_only_when_every_set_covers_every_required_scope() {
        let snap = world(
            &["run", "calendar.read", "calendar.delete"],
            &["run", "calendar.read", "calendar.delete"],
        );
        let requester = ctx(&["run", "calendar.read"]);
        assert_eq!(
            authorize(&set(&["calendar.read"]), &requester, &snap, &snap),
            Ok(())
        );
        assert_eq!(
            authorize(&set(&["calendar.delete"]), &requester, &snap, &snap),
            Err(Denial::Missing(vec!["calendar.delete".into()]))
        );
    }

    #[test]
    fn a_tool_needing_two_scopes_needs_both() {
        let snap = world(&["a", "b"], &["a", "b"]);
        assert_eq!(
            authorize(&set(&["a", "b"]), &ctx(&["a"]), &snap, &snap),
            Err(Denial::Missing(vec!["b".into()]))
        );
        assert_eq!(
            authorize(&set(&["a", "b"]), &ctx(&["a", "b"]), &snap, &snap),
            Ok(())
        );
    }

    #[test]
    fn profile_ceiling_beats_a_generous_requester() {
        let snap = world(&["calendar.read"], &["calendar.read", "calendar.delete"]);
        let requester = ctx(&["calendar.read", "calendar.delete"]);
        assert!(matches!(
            authorize(&set(&["calendar.delete"]), &requester, &snap, &snap),
            Err(Denial::Missing(_))
        ));
    }

    #[test]
    fn a_role_revoked_after_acceptance_stops_working_at_once() {
        // calendar.delete was present at intake, but has been revoked from the role.
        let snap = world(&["calendar.read", "calendar.delete"], &["calendar.read"]);
        let requester = ctx(&["calendar.read", "calendar.delete"]);
        assert!(authorize(&set(&["calendar.delete"]), &requester, &snap, &snap).is_err());
        assert_eq!(
            authorize(&set(&["calendar.read"]), &requester, &snap, &snap),
            Ok(())
        );
    }

    #[test]
    fn a_principal_who_left_the_profile_is_denied() {
        let snap = WorldBuilder::new("personal", "r1")
            .role("owner", &["a"])
            .policy_scopes(&["a"])
            .build();
        assert_eq!(
            authorize(&set(&["a"]), &ctx(&["a"]), &snap, &snap),
            Err(Denial::Member)
        );
    }

    #[test]
    fn an_agent_missing_from_the_current_revision_is_denied() {
        let snap = world(&["a"], &["a"]);
        let child = run_context("personal", "r1", 1, "local:owner", "main/gone", &["a"]);
        assert_eq!(
            authorize(&set(&["a"]), &child, &snap, &snap),
            Err(Denial::Agent)
        );
    }

    #[test]
    fn agent_ceiling_limits_a_child_and_inherits_the_parent_limit() {
        let snap = WorldBuilder::new("personal", "r1")
            .role("owner", &["run", "a", "b", "c", "d"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["run", "a", "b", "c", "d"])
            .agent_limit("main", &["run", "a", "b", "c"])
            .agent_limit("main/research", &["b", "c", "d"])
            .build();
        let all = ["run", "a", "b", "c", "d"];
        let child = run_context("personal", "r1", 1, "local:owner", "main/research", &all);
        let root = run_context("personal", "r1", 1, "local:owner", "main", &all);
        // main/research: {b, c, d} ∩ {run, a, b, c} = {b, c}; d exceeds the parent ceiling.
        assert_eq!(
            agent_ceiling(&snap.policy.agents, &AgentId("main/research".into())),
            Some(set(&["b", "c"]))
        );
        assert_eq!(authorize(&set(&["b"]), &child, &snap, &snap), Ok(()));
        assert!(authorize(&set(&["d"]), &child, &snap, &snap).is_err());
        assert!(authorize(&set(&["a"]), &child, &snap, &snap).is_err());
        assert_eq!(authorize(&set(&["a"]), &root, &snap, &snap), Ok(()));
    }

    #[test]
    fn the_agent_ceilings_of_both_revisions_apply() {
        let ours = ["a", "b"];
        let build = |revision: &str, main: &[&str]| {
            WorldBuilder::new("personal", revision)
                .role("owner", &ours)
                .member("local:owner", &["owner"])
                .policy_scopes(&ours)
                .agent_scopes("main", main)
                .build()
        };
        let (wide, narrow) = (build("r1", &["a", "b"]), build("r2", &["a"]));
        let requester = ctx(&["a", "b"]);
        // Pinned allows b, but current's agent ceiling is narrower (and vice versa).
        assert_eq!(authorize(&set(&["a"]), &requester, &narrow, &wide), Ok(()));
        assert!(authorize(&set(&["b"]), &requester, &narrow, &wide).is_err());
        assert!(authorize(&set(&["b"]), &requester, &wide, &narrow).is_err());
        assert_eq!(authorize(&set(&["b"]), &requester, &wide, &wide), Ok(()));
        let empty = WorldBuilder::new("personal", "r3").build();
        let missing = run_context("personal", "r1", 1, "local:owner", "main/gone", &["a"]);
        assert_eq!(
            authorize(&set(&["a"]), &missing, &wide, &empty),
            Err(Denial::Agent)
        );
    }

    #[test]
    fn unlisted_descendants_inherit_the_nearest_listed_ancestor() {
        let agents: BTreeMap<AgentId, Scopes> = [(AgentId("main".into()), set(&["a"]))]
            .into_iter()
            .collect();
        assert_eq!(
            agent_ceiling(&agents, &AgentId("main/x/y".into())),
            Some(set(&["a"]))
        );
        assert_eq!(agent_ceiling(&agents, &AgentId("other".into())), None);
    }

    #[test]
    fn a_tool_without_declared_scopes_is_never_callable() {
        let snap = world(&["a"], &["a"]);
        assert_eq!(
            authorize(&set(&[]), &ctx(&["a"]), &snap, &snap),
            Err(Denial::Undeclared)
        );
    }

    #[test]
    fn denial_codes_are_stable() {
        assert_eq!(Denial::Undeclared.code(), "scope_undeclared");
        assert_eq!(Denial::Member.code(), "member_unknown");
        assert_eq!(Denial::Agent.code(), "agent_unknown");
        assert_eq!(Denial::Missing(vec![]).code(), "scope_missing");
    }
}

//! 인가: 요청자 권한 ∩ 에이전트 상한 ∩ 프로필 상한이 도구가 요구하는 스코프 전체를 덮어야 한다.
//!
//! 요청자 권한은 접수 때 고정한 스냅샷(`RunContext.scopes`, 위임 때 좁아짐)과 지금 판본에서 그 주체의
//! 역할이 주는 스코프의 교집합이다(역할 회수가 바로 반영된다). 프로필 정책은 호출마다 현재 값으로 본다.

use crate::{
    id::*,
    profile::{Scopes, Snapshot},
    state::RunContext,
};
use std::collections::BTreeMap;

#[derive(Debug, Eq, PartialEq)]
pub enum Denial {
    /// 도구가 스코프를 하나도 선언하지 않았다. 선언 오류이므로 누구에게도 열지 않는다.
    Undeclared,
    /// 지금 판본의 멤버가 아니다.
    Member,
    /// 지금 판본에 없는 에이전트다.
    Agent,
    /// 부족한 스코프(정렬됨).
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

/// 이 주체의 역할이 지금 판본에서 주는 스코프의 합집합. 멤버가 아니면 `None`.
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

/// 에이전트 상한. 경로(`main/research`)의 조상 항목(자기 자신 포함)을 모두 교집합한다.
/// 자식이 부모보다 넓게 지정돼 있어도 부모 상한을 넘지 못한다. 지정된 항목이 없으면 `None`이다.
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

/// 이 주체가 이 에이전트에서 지금 판본 기준으로 쓸 수 있는 스코프(역할 ∩ 프로필 상한 ∩ 에이전트 상한).
/// 멤버가 아니거나 에이전트가 없으면 `None`. 접수 시점에 요청자 권한을 고정할 때 쓴다.
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

/// `required`(도구가 요구하는 스코프 전체)가 모든 상한에 들어 있는가.
/// `snapshot`은 지금 활성인 판본, `pinned`는 실행이 고정한 판본이다. 에이전트 상한(`Agent.scopes`)은 두 판본 모두 지킨다
/// (접수한 `ctx.scopes`가 고정 판본의 에이전트 상한까지 담는다는 보장이 없고, 현재 정책만 다시 계산하면 권한이 넓어질 수 있다).
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

/// 에이전트를 실행하는 데 필요한 기본 스코프. 위임으로 시작하는 자식 실행도 이 스코프가 있어야 한다.
pub const RUN_SCOPE: &str = "run";

/// 직계 자식에게 위임할 때의 인가(시작과 재개, 저장된 자식 결과를 돌려줄 때 모두 같은 함수를 쓴다). 돌려주는 값은 자식에게 넘길 범위다.
///
/// 현재 판본과 실행이 고정한 판본 **둘 다**에서: 자식이 이 에이전트의 직계로 있어야 하고, 요청자가 지금도 멤버이며 자식 에이전트에서
/// (역할 ∩ 정책 ∩ 에이전트 상한으로) `run`을 쓸 수 있어야 한다. 범위는 실행이 고정한 범위(`ctx.scopes`) ∩ 두 판본에서 자식에게
/// 허용되는 범위이고, 그 안에 `run`이 없으면 자식을 시작할 수 없다.
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
        // 접수 때는 calendar.delete가 있었지만 지금 판본의 역할에는 없다.
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
        // main/research: {b, c, d} ∩ {run, a, b, c} = {b, c}. d는 부모 상한 밖이라 열리지 않는다.
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
        // 고정 판본은 b를 허용하지만 지금 판본의 에이전트 상한이 좁다(역도 마찬가지다)
        assert_eq!(authorize(&set(&["a"]), &requester, &narrow, &wide), Ok(()));
        assert!(authorize(&set(&["b"]), &requester, &narrow, &wide).is_err());
        assert!(authorize(&set(&["b"]), &requester, &wide, &narrow).is_err());
        assert_eq!(authorize(&set(&["b"]), &requester, &wide, &wide), Ok(()));
        // 고정 판본에 에이전트가 없으면 거절한다
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

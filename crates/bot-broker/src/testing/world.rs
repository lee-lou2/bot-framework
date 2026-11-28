//! Test builders for profile snapshots, run contexts, and tool declarations.

use bot_core::{
    command::{JsonPayload, Target, Work},
    id::*,
    profile::*,
    state::{Conversation, ConversationKind, Risk, RunContext},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

pub fn tool(class: Risk, scopes: &[&str]) -> Tool {
    Tool {
        server: "demo-agenda".into(),
        revision: Revision("t1".into()),
        class,
        scopes: set(scopes),
        input: JsonPayload("{\"type\":\"object\"}".into()),
        description: None,
        env: Default::default(),
        secrets: BTreeMap::new(),
        egress: BTreeSet::new(),
        effects: false,
        external: false,
        destination: None,
        definition: None,
        mcp_name: None,
        timeout_secs: None,
    }
}

/// Builds a CLI-origin `RunContext` pinned at `r1` with requester as approver.
pub fn run_context(
    profile: &str,
    run: &str,
    generation: u64,
    principal: &str,
    agent: &str,
    scopes: &[&str],
) -> RunContext {
    let principal = PrincipalId(principal.into());
    RunContext {
        attempt: Attempt {
            profile: ProfileId(profile.into()),
            run: RunId(run.into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(generation),
        },
        root: RunId(run.into()),
        origin: bot_core::state::RunOrigin::Unknown,
        principal: principal.clone(),
        agent: AgentId(agent.into()),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "desk".into(),
            thread: None,
        },
        scopes: set(scopes),
        private_memory_read: false,
        work: Work {
            revision: Revision("r1".into()),
            target: Target::Agent {
                agent: AgentId(agent.into()),
                instruction: "x".into(),
            },
            input: JsonPayload("{}".into()),
            attachments: vec![],
            steps: vec![],
            pass_pos: None,
        },
        approver: Some(principal),
        trigger: None,
        binding: bot_core::id::BindingId("desk".into()),
        api: None,
        intake: bot_core::state::IntakeMeta::default(),
    }
}

fn blank_agent(parent: Option<AgentId>) -> Agent {
    Agent {
        description: String::new(),
        builtin: BTreeSet::new(),
        parent,
        model: "assistant".into(),
        instructions: String::new(),
        env: BTreeMap::new(),
        scopes: BTreeSet::new(),
        skills: BTreeMap::new(),
        knowledge: vec![],
        memory_seeds: vec![],
        triggers: vec![],
    }
}

pub struct SnapshotBuilder {
    snapshot: Snapshot,
    agent_scopes: BTreeMap<AgentId, BTreeSet<String>>,
}

/// Starts an empty snapshot with root agent `main` and policy revision `p-<revision>`.
pub fn snapshot(profile: &str, revision: &str) -> SnapshotBuilder {
    let root = AgentId("main".into());
    let snapshot = Snapshot {
        id: ProfileId(profile.into()),
        revision: Revision(revision.into()),
        policy_revision: Revision(format!("p-{revision}")),
        timezone: "UTC".into(),
        model: "assistant".into(),
        root: root.clone(),
        agents: [(root, blank_agent(None))].into_iter().collect(),
        bindings: BTreeMap::new(),
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
        super_admins: BTreeSet::new(),
        policy: Policy {
            scopes: BTreeSet::new(),
            agents: BTreeMap::new(),
            approve: BTreeSet::new(),
        },
        secrets: vec![],
    };
    SnapshotBuilder {
        snapshot,
        agent_scopes: BTreeMap::new(),
    }
}

impl SnapshotBuilder {
    pub fn role(mut self, name: &str, scopes: &[&str]) -> Self {
        self.snapshot.roles.insert(name.into(), set(scopes));
        self
    }

    pub fn member(mut self, identity: &str, roles: &[&str]) -> Self {
        self.snapshot
            .members
            .insert(PrincipalId(identity.into()), set(roles));
        self
    }

    pub fn super_admin(mut self, identity: &str) -> Self {
        self.snapshot
            .super_admins
            .insert(PrincipalId(identity.into()));
        self
    }

    pub fn policy_scopes(mut self, scopes: &[&str]) -> Self {
        self.snapshot.policy.scopes = set(scopes);
        self
    }

    pub fn approve(mut self, scopes: &[&str]) -> Self {
        self.snapshot.policy.approve = set(scopes);
        self
    }

    pub fn agent_limit(mut self, path: &str, scopes: &[&str]) -> Self {
        self.snapshot
            .policy
            .agents
            .insert(AgentId(path.into()), set(scopes));
        self.agent(path)
    }

    pub fn slack_dm_binding(mut self, id: &str, agent: &str) -> Self {
        self.snapshot.bindings.insert(
            BindingId(id.into()),
            Binding {
                agent: AgentId(agent.into()),
                source: Adapter::SlackDm {
                    workspace: "home".into(),
                },
            },
        );
        self
    }

    pub fn slack_channel_binding(mut self, id: &str, agent: &str) -> Self {
        self.snapshot.bindings.insert(
            BindingId(id.into()),
            Binding {
                agent: AgentId(agent.into()),
                source: Adapter::SlackChannel {
                    workspace: "home".into(),
                    channel: "planning".into(),
                    events: bot_core::slack::default_channel_events(),
                },
            },
        );
        self
    }

    pub fn openai_binding(
        mut self,
        id: &str,
        agent: &str,
        endpoint: &str,
        model: &str,
        approvers: &[(&str, bot_core::profile::ApprovalRoute)],
    ) -> Self {
        self.snapshot.bindings.insert(
            BindingId(id.into()),
            Binding {
                agent: AgentId(agent.into()),
                source: Adapter::OpenAi {
                    endpoint: endpoint.into(),
                    model: model.into(),
                    approvers: approvers
                        .iter()
                        .map(|(principal, route)| (PrincipalId((*principal).into()), route.clone()))
                        .collect(),
                },
            },
        );
        self
    }

    pub fn secret(mut self, name: &str) -> Self {
        self.snapshot
            .secrets
            .push(SecretName::parse(name).expect("secret name"));
        self
    }

    pub fn timezone(mut self, timezone: &str) -> Self {
        self.snapshot.timezone = timezone.into();
        self
    }

    pub fn policy_revision(mut self, revision: &str) -> Self {
        self.snapshot.policy_revision = Revision(revision.into());
        self
    }

    pub fn agent_env(mut self, path: &str, name: &str, value: &str) -> Self {
        self = self.agent(path);
        self.snapshot
            .agents
            .get_mut(&AgentId(path.into()))
            .expect("agent")
            .env
            .insert(name.into(), value.into());
        self
    }

    pub fn agent(mut self, path: &str) -> Self {
        let mut prefix = String::new();
        let mut parent: Option<AgentId> = None;
        for part in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let id = AgentId(prefix.clone());
            self.snapshot
                .agents
                .entry(id.clone())
                .or_insert_with(|| blank_agent(parent.clone()));
            parent = Some(id);
        }
        self
    }

    pub fn tool(mut self, agent: &str, id: &str, tool: Tool) -> Self {
        self = self.agent(agent);
        let skill = id.split('.').next().expect("tool ID").to_owned();
        let agent = self
            .snapshot
            .agents
            .get_mut(&AgentId(agent.into()))
            .expect("agent");
        agent
            .skills
            .entry(skill)
            .or_insert_with(|| Skill {
                description: None,
                instructions: Document {
                    relative_path: "SKILL.md".into(),
                    text: String::new(),
                },
                tools: BTreeMap::new(),
            })
            .tools
            .insert(ToolId(id.into()), tool);
        self
    }

    /// Overrides the derived `Agent.scopes` (normally the intersection of policy and ancestor ceilings).
    pub fn agent_scopes(mut self, path: &str, scopes: &[&str]) -> Self {
        self = self.agent(path);
        self.agent_scopes.insert(AgentId(path.into()), set(scopes));
        self
    }

    pub fn clear_triggers(mut self, agent: &str) -> Self {
        self = self.agent(agent);
        self.snapshot
            .agents
            .get_mut(&AgentId(agent.into()))
            .expect("agent")
            .triggers
            .clear();
        self
    }

    pub fn private_trigger(mut self, agent: &str, id: &str, binding: &str, tool: &str) -> Self {
        self = self.agent(agent);
        self.snapshot
            .agents
            .get_mut(&AgentId(agent.into()))
            .expect("agent")
            .triggers
            .push(Trigger {
                id: id.into(),
                on: TriggerSource::Event {
                    binding: BindingId(binding.into()),
                    event: "message".into(),
                },
                order: 0,
                conditions: BTreeMap::new(),
                input: BTreeMap::new(),
                private: true,
                private_pass: false,
                target: TriggerTarget::Tool {
                    tool: ToolId(tool.into()),
                    args: JsonPayload("{}".into()),
                },
                preapproved: None,
                human_approval: None,
            });
        self
    }

    pub fn cron_trigger(
        mut self,
        agent: &str,
        id: &str,
        principal: &str,
        approver: Option<&str>,
    ) -> Self {
        self = self.agent(agent);
        self.snapshot
            .agents
            .get_mut(&AgentId(agent.into()))
            .expect("agent")
            .triggers
            .push(Trigger {
                id: id.into(),
                on: TriggerSource::Cron {
                    schedule: "* * * * *".into(),
                    beat: false,
                    principal: PrincipalId(principal.into()),
                    approver: approver.map(|a| PrincipalId(a.into())),
                },
                order: 0,
                conditions: BTreeMap::new(),
                input: BTreeMap::new(),
                private: false,
                private_pass: false,
                target: TriggerTarget::Tool {
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                },
                preapproved: None,
                human_approval: None,
            });
        self
    }

    pub fn build(mut self) -> Arc<Snapshot> {
        // Profile contract: `Agent.scopes` already reflects profile and ancestor ceilings.
        let paths: Vec<AgentId> = self.snapshot.agents.keys().cloned().collect();
        for path in paths {
            let derived = match self.agent_scopes.remove(&path) {
                Some(explicit) => explicit,
                None => {
                    let ceiling =
                        bot_core::scope::agent_ceiling(&self.snapshot.policy.agents, &path);
                    self.snapshot
                        .policy
                        .scopes
                        .iter()
                        .filter(|scope| ceiling.as_ref().is_none_or(|c| c.contains(*scope)))
                        .cloned()
                        .collect()
                }
            };
            self.snapshot.agents.get_mut(&path).expect("agent").scopes = derived;
        }
        Arc::new(self.snapshot)
    }
}

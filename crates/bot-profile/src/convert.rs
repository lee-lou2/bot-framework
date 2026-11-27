//! 검증을 마친 중간 표현을 `bot_core::profile::Snapshot`으로 바꾼다.
//! core 타입은 데이터뿐이라 상속(지침·env·스킬·지식·스코프)이 이미 적용된 값을 담는다.
use crate::{
    hash::canonical_json,
    model::{self, Built, Match, Source, TriggerTarget},
};
use bot_core::{
    command::JsonPayload,
    id::{ProfileId, Revision, SecretName},
    profile::{
        Agent, Binding, ResponseMode, Skill, Snapshot, Tool, Trigger, TriggerSource,
        TriggerTarget as Target,
    },
};
use serde_json::Value;

pub fn to_core(built: Built, revision: Revision, policy_revision: Revision) -> Snapshot {
    let Built {
        parts,
        agents,
        root,
    } = built;
    Snapshot {
        id: ProfileId(parts.id),
        revision,
        policy_revision,
        timezone: parts.timezone,
        model: parts.model,
        root,
        agents: agents.into_iter().map(|(id, a)| (id, agent(&a))).collect(),
        bindings: parts
            .bindings
            .into_iter()
            .map(|(id, b)| {
                (
                    id,
                    Binding {
                        agent: b.agent,
                        source: b.kind,
                    },
                )
            })
            .collect(),
        roles: parts.roles,
        members: parts
            .members
            .into_iter()
            .map(|(id, m)| (id, m.roles))
            .collect(),
        super_admins: parts.super_admins,
        policy: parts.policy,
        secrets: parts
            .secrets
            .iter()
            .filter_map(|s| SecretName::parse(s))
            .collect(),
    }
}

fn agent(a: &model::Agent) -> Agent {
    Agent {
        description: a.description.clone(),
        builtin: a.builtin.clone(),
        parent: a.parent.clone(),
        model: a.model.clone(),
        instructions: a.parts.join("\n\n"),
        env: a.env.clone(),
        scopes: a.ceiling.clone(),
        skills: a
            .skills
            .iter()
            .map(|s| {
                let tools = s.tools.iter().map(|t| (t.id.clone(), tool(t))).collect();
                (
                    s.name.clone(),
                    Skill {
                        description: s.description.clone(),
                        instructions: s.instructions.clone(),
                        tools,
                    },
                )
            })
            .collect(),
        knowledge: a.knowledge.clone(),
        memory_seeds: a.memory.clone(),
        triggers: a.triggers.iter().map(trigger).collect(),
    }
}

fn tool(t: &model::Tool) -> Tool {
    Tool {
        server: t.server.clone(),
        revision: t.revision.clone(),
        // 수신자를 인자로 정하는 전송 도구는 irreversible로 올린 실효 등급을 넘긴다(선언 등급은 판본 해시에 남는다)
        class: t.risk(),
        scopes: t.scopes.clone(),
        input: t.input.clone(),
        description: t.description.clone(),
        env: t.env.clone(),
        secrets: t.secrets.clone(),
        egress: t.egress.clone(),
        effects: t.effects,
        external: t.external,
        destination: t.destination.clone(),
        definition: t.definition.clone(),
        mcp_name: t.mcp_name.clone(),
        timeout_secs: t.timeout_secs,
    }
}

fn trigger(t: &model::Trigger) -> Trigger {
    let on = match &t.on {
        Source::Cron {
            cron,
            beat,
            principal,
            approver,
        } => TriggerSource::Cron {
            schedule: cron.as_str().to_owned(),
            beat: *beat,
            principal: principal.clone(),
            approver: approver.clone(),
        },
        Source::Message { binding } => TriggerSource::Event {
            binding: binding.clone(),
            event: "message".to_owned(),
        },
        Source::ThreadReply { binding } => TriggerSource::Event {
            binding: binding.clone(),
            event: "thread-reply".to_owned(),
        },
        Source::Reaction { binding } => TriggerSource::Event {
            binding: binding.clone(),
            event: "reaction".to_owned(),
        },
        Source::Http {
            method,
            path,
            binding,
            sync,
        } => TriggerSource::Http {
            binding: binding.clone(),
            method: method.clone(),
            path: path.clone(),
            response: if *sync {
                ResponseMode::Sync
            } else {
                ResponseMode::Accepted
            },
        },
    };
    let target = match &t.target {
        TriggerTarget::Instruction(text) => Target::Instruction(text.clone()),
        TriggerTarget::Tool { tool, args } => Target::Tool {
            tool: tool.clone(),
            args: JsonPayload(canonical_json(&Value::Object(args.clone()))),
        },
    };
    Trigger {
        id: t.id.clone(),
        on,
        order: t.order,
        conditions: t
            .matches
            .iter()
            .map(|Match { path, value }| (path.clone(), JsonPayload(canonical_json(value))))
            .collect(),
        input: t.input.clone(),
        private: t.private,
        private_pass: t.private_pass,
        target,
        preapproved: t.preapproved.clone(),
        human_approval: t.human_approval.clone(),
    }
}

//! 가짜 프로필 목록: 개인 비서 예시(personal)와 비밀·외부 전송·HTTP 트리거를 쓰는 합성 프로필(work).

use bot_core::{command::JsonPayload, id::*, profile::*, state::Risk};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub struct StaticCatalog(pub BTreeMap<ProfileId, Arc<Snapshot>>);

impl ProfileCatalog for StaticCatalog {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>> {
        Arc::new(self.0.clone())
    }

    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>> {
        self.0
            .get(profile)
            .filter(|s| &s.revision == revision)
            .cloned()
    }
}

/// personal + work.
pub fn demo_catalog() -> StaticCatalog {
    let all = [personal(), work()];
    StaticCatalog(
        all.into_iter()
            .map(|s| (s.id.clone(), Arc::new(s)))
            .collect(),
    )
}

fn set<const N: usize>(v: [&str; N]) -> BTreeSet<String> {
    v.into_iter().map(str::to_owned).collect()
}

fn doc(path: &str) -> Document {
    Document {
        relative_path: path.to_owned(),
        text: String::new(),
    }
}

fn tool(server: &str, class: Risk, scopes: BTreeSet<String>) -> Tool {
    Tool {
        server: server.to_owned(),
        revision: Revision("t1-0123456789ab".into()),
        class,
        scopes,
        input: JsonPayload(r#"{"type":"object"}"#.to_owned()),
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

fn agent(
    parent: Option<&str>,
    skills: BTreeMap<String, Skill>,
    triggers: Vec<Trigger>,
    scopes: BTreeSet<String>,
) -> Agent {
    Agent {
        description: String::new(),
        builtin: BTreeSet::new(),
        parent: parent.map(|p| AgentId(p.to_owned())),
        model: "assistant".to_owned(),
        instructions: String::new(),
        env: BTreeMap::new(),
        scopes,
        skills,
        knowledge: Vec::new(),
        memory_seeds: Vec::new(),
        triggers,
    }
}

fn snapshot(id: &str, agents: Vec<(&str, Agent)>, bindings: Vec<(&str, Binding)>) -> Snapshot {
    Snapshot {
        id: ProfileId(id.to_owned()),
        revision: Revision(format!("rev-{id}-0123456789")),
        policy_revision: Revision(format!("pol-{id}-0123456789")),
        timezone: "UTC".to_owned(),
        model: "assistant".to_owned(),
        root: AgentId("main".to_owned()),
        agents: agents
            .into_iter()
            .map(|(k, v)| (AgentId(k.to_owned()), v))
            .collect(),
        bindings: bindings
            .into_iter()
            .map(|(k, v)| (BindingId(k.to_owned()), v))
            .collect(),
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
        super_admins: BTreeSet::new(),
        policy: Policy {
            scopes: BTreeSet::new(),
            agents: BTreeMap::new(),
            approve: BTreeSet::new(),
        },
        secrets: Vec::new(),
    }
}

pub fn personal() -> Snapshot {
    let agenda = || Skill {
        description: None,
        instructions: doc("agents/main/skills/agenda/SKILL.md"),
        tools: BTreeMap::from([
            (
                ToolId("agenda.list".into()),
                tool("demo-agenda", Risk::Read, set(["calendar.read"])),
            ),
            (
                ToolId("agenda.delete".into()),
                tool("demo-agenda", Risk::Irreversible, set(["calendar.delete"])),
            ),
        ]),
    };
    let morning = Trigger {
        id: "morning".into(),
        on: TriggerSource::Cron {
            schedule: "* * * * *".into(),
            beat: false,
            principal: PrincipalId("service:morning".into()),
            approver: None,
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
    };
    let skills = || BTreeMap::from([("agenda".to_owned(), agenda())]);
    let mut s = snapshot(
        "personal",
        vec![
            (
                "main",
                agent(
                    None,
                    skills(),
                    vec![morning],
                    set(["run", "calendar.read", "calendar.delete"]),
                ),
            ),
            (
                "main/research",
                agent(
                    Some("main"),
                    skills(),
                    Vec::new(),
                    set(["run", "calendar.read"]),
                ),
            ),
        ],
        vec![(
            "desk",
            Binding {
                agent: AgentId("main".into()),
                source: Adapter::Cli {
                    account: "owner".into(),
                },
            },
        )],
    );
    if let Some(main) = s.agents.get_mut(&AgentId("main".into())) {
        main.description = "오늘 일정을 조회하고 변경 전에는 정확한 일정을 확인한다.".into();
        main.builtin = BTreeSet::from([Builtin::Read, Builtin::Grep]);
    }
    s.roles = BTreeMap::from([
        (
            "owner".into(),
            set(["run", "calendar.read", "calendar.delete"]),
        ),
        ("reader".into(), set(["calendar.read"])),
    ]);
    s.members = BTreeMap::from([
        (PrincipalId("local:owner".into()), set(["owner"])),
        (PrincipalId("service:morning".into()), set(["reader"])),
    ]);
    s.policy.scopes = set(["run", "calendar.read", "calendar.delete"]);
    s.policy.agents = BTreeMap::from([(
        AgentId("main/research".into()),
        set(["run", "calendar.read"]),
    )]);
    s
}

pub fn work() -> Snapshot {
    let mut send = tool("demo-mail", Risk::Write, set(["mail.send"]));
    send.external = true;
    send.effects = true;
    send.egress = set(["mail.example:443"]);
    send.secrets = BTreeMap::from([(
        "MAIL_TOKEN".to_owned(),
        SecretName::parse("mail-sender").expect("이름"),
    )]);
    send.destination = Some(Destination::Explicit {
        recipient: "args.to".into(),
        content: "args.message".into(),
    });
    let mail = Skill {
        description: None,
        instructions: doc("agents/main/skills/mail/SKILL.md"),
        tools: BTreeMap::from([(ToolId("mail.send".into()), send)]),
    };
    let inbox = Trigger {
        id: "cancel".into(),
        on: TriggerSource::Http {
            binding: BindingId("inbox".into()),
            method: "POST".into(),
            path: "/mail".into(),
            response: ResponseMode::Accepted,
        },
        order: 10,
        conditions: BTreeMap::new(),
        input: BTreeMap::new(),
        private: false,
        private_pass: false,
        target: TriggerTarget::Instruction("handle".into()),
        preapproved: None,
        human_approval: None,
    };
    let mut s = snapshot(
        "work",
        vec![(
            "main",
            agent(
                None,
                BTreeMap::from([("mail".to_owned(), mail)]),
                vec![inbox],
                set(["run", "mail.send"]),
            ),
        )],
        vec![
            (
                "team",
                Binding {
                    agent: AgentId("main".into()),
                    source: Adapter::SlackChannel {
                        workspace: "home".into(),
                        channel: "planning".into(),
                        events: bot_core::slack::default_channel_events(),
                    },
                },
            ),
            (
                "inbox",
                Binding {
                    agent: AgentId("main".into()),
                    source: Adapter::Http {
                        endpoint: "reminders".into(),
                    },
                },
            ),
        ],
    );
    s.roles = BTreeMap::from([("owner".into(), set(["run", "mail.send"]))]);
    s.members = BTreeMap::from([(PrincipalId("local:owner".into()), set(["owner"]))]);
    s.policy.scopes = set(["run", "mail.send"]);
    s.policy.approve = set(["mail.send"]);
    s.secrets = vec![SecretName::parse("mail-sender").expect("이름")];
    s
}

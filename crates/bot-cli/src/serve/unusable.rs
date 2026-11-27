//! Startup scan warning on tools whose declared secrets are missing or unreadable.
use bot_core::{
    id::{ProfileId, SecretName, ToolId},
    port::{Error, ProfileSecrets, SecretStore},
    profile::Snapshot,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Why {
    Missing,
    Unreadable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unusable {
    pub profile: ProfileId,
    pub tool: ToolId,
    pub secret: String,
    pub why: Why,
}

impl Unusable {
    /// Operator warning line containing secret names only (never values).
    pub fn warning(&self) -> String {
        let (profile, tool, secret) = (&self.profile.0, &self.tool.0, &self.secret);
        match self.why {
            Why::Missing => {
                format!(
                    "warning: profile {profile} tool {tool}: secret {secret} is missing -> unusable"
                )
            }
            Why::Unreadable => format!(
                "warning: profile {profile} tool {tool}: failed to read secret {secret} (store error) -> may be unavailable"
            ),
        }
    }
}

async fn check(handle: &Result<Box<dyn ProfileSecrets>, Error>, name: &str) -> Option<Why> {
    let secret = SecretName::parse(name)?;
    match handle {
        Ok(handle) => match handle.resolve(secret).await {
            Ok(_) => None,
            Err(Error::Missing) => Some(Why::Missing),
            Err(_) => Some(Why::Unreadable),
        },
        Err(_) => Some(Why::Unreadable),
    }
}

pub async fn unusable_tools(
    secrets: &dyn SecretStore,
    active: &BTreeMap<ProfileId, Arc<Snapshot>>,
) -> Vec<Unusable> {
    let mut found = Vec::new();
    for (profile, snapshot) in active {
        let mut wanted: BTreeMap<ToolId, BTreeSet<String>> = BTreeMap::new();
        for agent in snapshot.agents.values() {
            for skill in agent.skills.values() {
                for (id, tool) in &skill.tools {
                    wanted
                        .entry(id.clone())
                        .or_default()
                        .extend(tool.secrets.values().map(|name| name.as_str().to_owned()));
                }
            }
        }
        if wanted.values().all(BTreeSet::is_empty) {
            continue;
        }
        let handle = secrets.for_profile(profile.clone()).await;
        let mut seen: BTreeMap<String, Option<Why>> = BTreeMap::new();
        for (tool, names) in wanted {
            for name in names {
                let verdict = match seen.get(&name) {
                    Some(verdict) => *verdict,
                    None => {
                        let verdict = check(&handle, &name).await;
                        seen.insert(name.clone(), verdict);
                        verdict
                    }
                };
                if let Some(why) = verdict {
                    found.push(Unusable {
                        profile: profile.clone(),
                        tool: tool.clone(),
                        secret: name,
                        why,
                    });
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_broker::testing::FakeSecrets;
    use bot_core::{
        command::JsonPayload,
        id::{AgentId, Revision},
        profile::{Document, Skill, Tool},
        state::Risk,
    };
    use bot_gateway::testing::profile;
    use std::collections::BTreeSet as Set;

    fn tool(secrets: &[(&str, &str)]) -> Tool {
        Tool {
            server: "agenda".into(),
            revision: Revision("t1".into()),
            class: Risk::Read,
            scopes: Default::default(),
            input: JsonPayload("{}".into()),
            description: None,
            env: Default::default(),
            secrets: secrets
                .iter()
                .map(|(var, name)| ((*var).to_owned(), SecretName::parse(name).unwrap()))
                .collect(),
            egress: Set::new(),
            effects: false,
            external: false,
            destination: None,
            definition: None,
            mcp_name: None,
            timeout_secs: None,
        }
    }

    type Declared<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    fn declaring(tools: &[Declared]) -> BTreeMap<ProfileId, Arc<Snapshot>> {
        let mut snapshot = (*profile("personal").build()).clone();
        let agent = snapshot.agents.get_mut(&AgentId("main".into())).unwrap();
        for (skill, id, secrets) in tools {
            agent
                .skills
                .entry((*skill).to_owned())
                .or_insert_with(|| Skill {
                    description: None,
                    instructions: Document {
                        relative_path: format!("skills/{skill}/SKILL.md"),
                        text: String::new(),
                    },
                    tools: BTreeMap::new(),
                })
                .tools
                .insert(ToolId((*id).into()), tool(secrets));
        }
        BTreeMap::from([(snapshot.id.clone(), Arc::new(snapshot))])
    }

    fn store(entries: &[(&str, &str, &str)]) -> Arc<dyn SecretStore> {
        Arc::new(FakeSecrets::new(entries))
    }

    #[tokio::test]
    async fn only_tools_whose_declared_secret_is_missing_are_unusable_and_the_warning_has_names_only()
     {
        let active = declaring(&[
            ("agenda", "agenda.list", &[("CAL", "calendar-reader")]),
            ("agenda", "agenda.delete", &[("CAL", "calendar-writer")]),
            ("plain", "plain.ping", &[]),
        ]);
        let secrets = store(&[("personal", "calendar-reader", "value-never-shown")]);
        let found = unusable_tools(&*secrets, &active).await;
        assert_eq!(
            found,
            [Unusable {
                profile: ProfileId("personal".into()),
                tool: ToolId("agenda.delete".into()),
                secret: "calendar-writer".into(),
                why: Why::Missing,
            }]
        );
        let line = found[0].warning();
        assert!(
            line.contains("agenda.delete")
                && line.contains("calendar-writer")
                && line.contains("unusable"),
            "{line}"
        );
        assert!(!line.contains("value-never-shown"), "{line}");
    }

    #[tokio::test]
    async fn the_same_name_is_read_once_per_profile_and_a_tool_lists_each_missing_name() {
        let active = declaring(&[
            ("agenda", "agenda.list", &[("A", "key-a"), ("B", "key-b")]),
            ("agenda", "agenda.delete", &[("A", "key-a")]),
        ]);
        let fake = Arc::new(FakeSecrets::new(&[]));
        let secrets: Arc<dyn SecretStore> = fake.clone();
        let found = unusable_tools(&*secrets, &active).await;
        let named: Vec<(String, String)> = found
            .iter()
            .map(|u| (u.tool.0.clone(), u.secret.clone()))
            .collect();
        assert_eq!(
            named,
            [
                ("agenda.delete".to_owned(), "key-a".to_owned()),
                ("agenda.list".to_owned(), "key-a".to_owned()),
                ("agenda.list".to_owned(), "key-b".to_owned()),
            ]
        );
        assert_eq!(
            fake.resolved().len(),
            2,
            "each secret name is read once per profile"
        );
    }

    struct Broken;

    struct BrokenHandle;

    impl ProfileSecrets for BrokenHandle {
        fn resolve(&self, _: SecretName) -> bot_core::port::Reply<'_, bot_core::port::SecretValue> {
            Box::pin(std::future::ready(Err(Error::Unavailable)))
        }
    }

    impl SecretStore for Broken {
        fn for_profile(&self, _: ProfileId) -> bot_core::port::Reply<'_, Box<dyn ProfileSecrets>> {
            Box::pin(std::future::ready(Ok(
                Box::new(BrokenHandle) as Box<dyn ProfileSecrets>
            )))
        }
    }

    #[tokio::test]
    async fn an_unreadable_store_is_reported_differently_from_a_missing_name() {
        let active = declaring(&[("agenda", "agenda.list", &[("CAL", "calendar-reader")])]);
        let secrets: Arc<dyn SecretStore> = Arc::new(Broken);
        let found = unusable_tools(&*secrets, &active).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].why, Why::Unreadable);
        assert!(found[0].warning().contains("failed to read"));
        assert!(found[0].warning().contains("calendar-reader"));
    }

    #[tokio::test]
    async fn profiles_without_declared_tool_secrets_cost_no_reads() {
        let active = declaring(&[("plain", "plain.ping", &[])]);
        let fake = Arc::new(FakeSecrets::new(&[]));
        let secrets: Arc<dyn SecretStore> = fake.clone();
        assert!(unusable_tools(&*secrets, &active).await.is_empty());
        assert!(fake.resolved().is_empty());
    }
}

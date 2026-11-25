//! 도구가 환경으로 받는 자격이 지금 보관소에 없는 도구를 기동 때 찾아 **이름만** 경고한다. 스킬 하나의 설정이 빠졌다고 봇 전체가 서면 안 되므로
//! worker는 그대로 시작한다(`bot validate`는 이런 프로필을 엄격하게 실패시킨다). 값은 읽어 보되 어디에도 남기지 않는다.
//! 그 도구의 호출은 브로커가 **호출을 기록하기 전에** 고정 단어(`call.refuse secret_missing`)로 거절한다(`ToolRunner::admit_secrets`) — 승인 카드도
//! 열지 않고, 재시도 순환도 없다.
use bot_core::{
    id::{ProfileId, SecretName, ToolId},
    port::{Error, ProfileSecrets, SecretStore},
    profile::Snapshot,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// 자격을 쓸 수 없는 이유
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Why {
    /// 보관소에 그 이름이 없다
    Missing,
    /// 보관소를 읽지 못했다(키체인 잠김 등 — 지금만일 수 있다)
    Unreadable,
}

/// 쓸 수 없는 도구 하나와 그 이유가 된 자격 이름 하나
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unusable {
    pub profile: ProfileId,
    pub tool: ToolId,
    pub secret: String,
    pub why: Why,
}

impl Unusable {
    /// 운영자에게 보일 경고 한 줄. 이름만 싣는다.
    pub fn warning(&self) -> String {
        let (profile, tool, secret) = (&self.profile.0, &self.tool.0, &self.secret);
        match self.why {
            Why::Missing => {
                format!("경고: 프로필 {profile}의 도구 {tool}: 자격 이름 {secret} 없음 → 사용 불가")
            }
            Why::Unreadable => format!(
                "경고: 프로필 {profile}의 도구 {tool}: 자격 이름 {secret}을(를) 읽지 못했어요(보관소 오류) → 지금은 사용할 수 없을 수 있어요"
            ),
        }
    }
}

async fn check(handle: &Result<Box<dyn ProfileSecrets>, Error>, name: &str) -> Option<Why> {
    // 이름 규칙에 맞지 않는 이름은 프로필 검증이 이미 거절했다
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

/// 활성 프로필의 도구 중 선언한 자격을 지금 쓸 수 없는 것들(프로필·도구 ID·이름 순). 같은 이름은 프로필마다 한 번만 읽는다.
pub async fn unusable_tools(
    secrets: &dyn SecretStore,
    active: &BTreeMap<ProfileId, Arc<Snapshot>>,
) -> Vec<Unusable> {
    let mut found = Vec::new();
    for (profile, snapshot) in active {
        // 같은 도구 ID를 여러 에이전트·스킬이 선언할 수 있다: 선언한 자격 이름을 모두 모은다
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
        }
    }

    /// (스킬 이름, 도구 ID, (환경 변수, 자격 이름)들)
    type Declared<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// 도구들을 선언한 `personal` 프로필
    fn declaring(tools: &[Declared]) -> BTreeMap<ProfileId, Arc<Snapshot>> {
        let mut snapshot = (*profile("personal").build()).clone();
        let agent = snapshot.agents.get_mut(&AgentId("main".into())).unwrap();
        for (skill, id, secrets) in tools {
            agent
                .skills
                .entry((*skill).to_owned())
                .or_insert_with(|| Skill {
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
                && line.contains("사용 불가"),
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
        assert_eq!(fake.resolved().len(), 2, "이름마다 한 번만 읽는다");
    }

    /// 읽을 때마다 보관소 오류를 내는 보관소
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
        assert!(found[0].warning().contains("읽지 못했어요"));
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

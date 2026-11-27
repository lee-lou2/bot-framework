//! Approval-only broker wrapper for gateway and admin roles that do not run tools.
use bot_broker::{
    ports::{Ran, ToolJob, ToolRunner},
    seal::JcsSealer,
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
};
use bot_core::{
    command::{CallResult, ToolCall},
    id::{ApprovalId, PrincipalId, Revision, ToolId},
    ledger::CallLedger,
    port::{AuditLog, Broker, EffectJournal, Error, MemoryStore, Reply},
    profile::{ProfileCatalog, Snapshot},
    schedule::{ScheduleRoutes, ScheduledReplies, UserSchedules},
    trace::Tracer,
};
use bot_profile::{Registry, SchemaValidator};
use bot_store::Store;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
};

#[derive(Clone, Default)]
pub(super) struct LiveInstalled(Arc<RwLock<BTreeMap<String, String>>>);

impl LiveInstalled {
    pub(super) fn from_revisions(revisions: &BTreeMap<String, Revision>) -> Self {
        let map = revisions
            .iter()
            .map(|(alias, revision)| (alias.clone(), revision.0.clone()))
            .collect();
        Self(Arc::new(RwLock::new(map)))
    }

    pub(super) fn update(&self, revisions: &BTreeMap<String, Revision>) {
        let next: BTreeMap<String, String> = revisions
            .iter()
            .map(|(alias, revision)| (alias.clone(), revision.0.clone()))
            .collect();
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = next;
    }

    fn get(&self, server: &str) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server)
            .cloned()
    }

    fn keys(&self) -> BTreeSet<String> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

pub(super) struct InstalledOnly(pub(super) LiveInstalled);

impl ToolRunner for InstalledOnly {
    fn installed(&self, server: &str) -> Option<String> {
        self.0.get(server)
    }

    fn run(&self, _: ToolJob) -> Reply<'_, Ran> {
        Box::pin(std::future::ready(Ok(Ran::NotSent)))
    }

    fn reap(&self, _: String) -> Reply<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

fn declared_servers<'a>(snapshot: &'a Snapshot, tool: &ToolId) -> BTreeSet<&'a str> {
    snapshot
        .agents
        .values()
        .flat_map(|agent| agent.skills.values())
        .filter_map(|skill| skill.tools.get(tool))
        .map(|declared| declared.server.as_str())
        .collect()
}

fn holds(snapshot: &Snapshot, tool: &ToolId, installed: &BTreeSet<String>) -> bool {
    declared_servers(snapshot, tool)
        .iter()
        .any(|alias| !installed.contains(*alias))
}

/// Holds approvals for external tools whose server install revision is unknown on this host so
/// `BrokerService` does not falsely mark valid pending cards as `Superseded`; denials and built-ins always proceed.
struct Decisions {
    inner: Arc<dyn Broker>,
    ledger: Arc<dyn CallLedger>,
    catalog: Arc<dyn ProfileCatalog>,
    installed: LiveInstalled,
}

impl Broker for Decisions {
    fn call(&self, _: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(std::future::ready(Err(Error::Unavailable)))
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        Box::pin(async move {
            if allow {
                let stored = self.ledger.approval(nonce.clone()).await?;
                let approval = &stored.approval;
                let known = self.installed.keys();
                let unknown = self
                    .catalog
                    .current(&approval.attempt.profile)
                    .is_some_and(|snapshot| holds(&snapshot, &approval.tool, &known));
                if unknown {
                    return Err(Error::Unavailable);
                }
            }
            self.inner.decide(nonce, actor, allow).await
        })
    }
}

pub fn broker(
    registry: Arc<Registry>,
    store: Arc<Store>,
    installed: LiveInstalled,
    tracer: Arc<dyn Tracer>,
    routes: Arc<dyn ScheduleRoutes>,
) -> Arc<dyn Broker> {
    let ledger = store.clone() as Arc<dyn CallLedger>;
    let schedules = store.clone() as Arc<dyn UserSchedules>;
    let replies = store.clone() as Arc<dyn ScheduledReplies>;
    let service = BrokerService::new(
        Parts {
            catalog: registry.clone() as Arc<dyn ProfileCatalog>,
            ledger: ledger.clone(),
            effects: store.clone() as Arc<dyn EffectJournal>,
            memory: store.clone() as Arc<dyn MemoryStore>,
            audit: store as Arc<dyn AuditLog>,
            runner: Arc::new(InstalledOnly(installed.clone())),
            sealer: Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new()))),
            tracer,
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(schedules)
    .with_replies(replies)
    .with_routes(routes);
    Arc::new(Decisions {
        inner: Arc::new(service),
        ledger,
        catalog: registry as Arc<dyn ProfileCatalog>,
        installed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{
        command::JsonPayload,
        id::{AgentId, ProfileId},
        ledger::ApprovalState,
        profile::{Document, Skill, Tool},
        state::Risk,
    };
    use bot_gateway::testing::{
        TestCatalog,
        ports::{MemoryBroker, MemoryLedger},
        profile,
    };

    fn tool(server: &str) -> Tool {
        Tool {
            server: server.into(),
            revision: Revision("t1".into()),
            class: Risk::Irreversible,
            scopes: Default::default(),
            input: JsonPayload("{}".into()),
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

    fn declaring(servers: &[&str]) -> Arc<Snapshot> {
        let mut snapshot = (*profile("personal").build()).clone();
        let agent = snapshot.agents.get_mut(&AgentId("main".into())).unwrap();
        for (n, server) in servers.iter().enumerate() {
            agent.skills.insert(
                format!("skill-{n}"),
                Skill {
                    description: None,
                    instructions: Document {
                        relative_path: format!("skills/{n}/SKILL.md"),
                        text: String::new(),
                    },
                    tools: BTreeMap::from([(ToolId("agenda.delete".into()), tool(server))]),
                },
            );
        }
        Arc::new(snapshot)
    }

    fn installed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn only_external_tools_whose_server_install_is_unknown_are_held() {
        let delete = ToolId("agenda.delete".into());
        assert!(!holds(
            &declaring(&["agenda"]),
            &delete,
            &installed(&["agenda"])
        ));
        assert!(holds(&declaring(&["agenda"]), &delete, &installed(&[])));
        assert!(holds(
            &declaring(&["agenda"]),
            &delete,
            &installed(&["other"])
        ));
        assert!(holds(
            &declaring(&["agenda", "extra"]),
            &delete,
            &installed(&["agenda"])
        ));
        assert!(!holds(
            &declaring(&["agenda"]),
            &ToolId(bot_core::builtin::SHARE_TOOL.into()),
            &installed(&[])
        ));
        assert!(!holds(&declaring(&[]), &delete, &installed(&[])));
    }

    async fn rig(
        snapshot: Arc<Snapshot>,
        known: &[&str],
    ) -> (Decisions, Arc<MemoryBroker>, Arc<MemoryLedger>) {
        let inner = MemoryBroker::new();
        let ledger = MemoryLedger::new();
        ledger.put(
            "n1",
            "personal",
            ApprovalState::Pending,
            r#"{"nonce":"n1","title":"Delete","lines":[]}"#,
        );
        let decisions = Decisions {
            inner: inner.clone(),
            ledger: ledger.clone(),
            catalog: TestCatalog::new([snapshot]),
            installed: LiveInstalled::from_revisions(
                &known
                    .iter()
                    .map(|k| ((*k).to_owned(), Revision("r1".into())))
                    .collect(),
            ),
        };
        (decisions, inner, ledger)
    }

    fn approve(decisions: &Decisions, allow: bool) -> Reply<'_, ()> {
        decisions.decide(
            ApprovalId("n1".into()),
            PrincipalId("slack:home/U1".into()),
            allow,
        )
    }

    #[tokio::test]
    async fn an_unknown_install_keeps_the_card_pending_but_a_denial_always_goes_through() {
        let (decisions, inner, _) = rig(declaring(&["agenda"]), &[]).await;
        assert!(matches!(
            approve(&decisions, true).await,
            Err(Error::Unavailable)
        ));
        assert_eq!(inner.attempts(), 0);
        approve(&decisions, false).await.unwrap();
        assert_eq!(inner.decisions().len(), 1);
        let call = ToolCall {
            attempt: bot_core::id::Attempt {
                profile: ProfileId("personal".into()),
                run: bot_core::id::RunId("r".into()),
                claim: None,
                generation: bot_core::id::Generation(1),
            },
            call: bot_core::id::CallId("c".into()),
            tool: ToolId("agenda.delete".into()),
            args: JsonPayload("{}".into()),
        };
        assert!(matches!(
            decisions.call(call).await,
            Err(Error::Unavailable)
        ));
    }

    #[tokio::test]
    async fn known_installs_and_builtin_decisions_reach_the_real_broker() {
        let (decisions, inner, _) = rig(declaring(&["agenda"]), &["agenda"]).await;
        approve(&decisions, true).await.unwrap();
        assert_eq!(inner.attempts(), 1);
        let (decisions, inner, _) = rig(declaring(&[]), &[]).await;
        approve(&decisions, true).await.unwrap();
        assert_eq!(inner.attempts(), 1);
    }

    #[test]
    fn the_runner_only_answers_installed_revisions_and_never_runs_tools() {
        let runner = InstalledOnly(LiveInstalled::from_revisions(&BTreeMap::from([(
            "agenda".to_owned(),
            Revision("r1".into()),
        )])));
        assert_eq!(runner.installed("agenda").as_deref(), Some("r1"));
        assert_eq!(runner.installed("other"), None);
    }
}

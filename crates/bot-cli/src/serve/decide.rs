//! 승인 결정 경로(gateway의 Slack 버튼·admin의 승인 판정)에 꽂는 브로커.
//!
//! 실제 `BrokerService`에 위임하되 이 프로세스는 도구를 실행하지 않으므로 실행기는 «설치 판본만 아는 것»이다(실행 요청은 미실행으로
//! 돌려준다). 판본은 호스트 `servers`의 설치본에서 계산한 것이라 도구를 실행하는 호스트가 같은 패키지를 배포했다면 같은 판본이고,
//! 달라졌으면 실제 브로커가 옛 카드를 조건 변경으로 거절한다(새 코드를 옛 승인으로 실행하지 못한다).
//!
//! 내장 판정(기억 공유 등 도구 서버가 없는 것)은 설치와 무관하므로 **항상** 실제 브로커가 판정한다. 외부 도구는 이 프로세스가 그 서버의
//! 설치 판본을 알 때만 맡기고, 모르면 카드를 그대로 두고 일시 실패로 보류한다 — 실제 브로커는 설치 정보가 없으면 «조건 변경»으로 읽어
//! 유효한 대기 카드를 `Superseded`로 만들기 때문이다. 거절 클릭은 설치 판본이 필요 없어 늘 맡긴다.
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
    sync::Arc,
};

/// 이 프로세스에서는 도구를 실행하지 않는다 — 설치 판본 조회에만 답한다.
pub(super) struct InstalledOnly(pub(super) BTreeMap<String, String>);

impl ToolRunner for InstalledOnly {
    fn installed(&self, server: &str) -> Option<String> {
        self.0.get(server).cloned()
    }

    fn run(&self, _: ToolJob) -> Reply<'_, Ran> {
        Box::pin(std::future::ready(Ok(Ran::NotSent)))
    }

    fn reap(&self, _: String) -> Reply<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// 이 도구를 선언한 서버 별칭들(모든 에이전트의 스킬을 훑는다). 내장 도구(기억 공유 등)는 어느 서버에도 선언돼 있지 않아 비어 있다.
fn declared_servers<'a>(snapshot: &'a Snapshot, tool: &ToolId) -> BTreeSet<&'a str> {
    snapshot
        .agents
        .values()
        .flat_map(|agent| agent.skills.values())
        .filter_map(|skill| skill.tools.get(tool))
        .map(|declared| declared.server.as_str())
        .collect()
}

/// 이 도구의 승인을 이 프로세스가 판정할 수 없는가: 선언한 서버 중 설치 판본을 모르는 것이 있다.
fn holds(snapshot: &Snapshot, tool: &ToolId, installed: &BTreeSet<String>) -> bool {
    declared_servers(snapshot, tool)
        .iter()
        .any(|alias| !installed.contains(*alias))
}

/// 설치 판본을 모르는 외부 도구의 승인은 보류하고, 나머지는 실제 브로커에 맡기는 결정 경로
struct Decisions {
    inner: Arc<dyn Broker>,
    ledger: Arc<dyn CallLedger>,
    catalog: Arc<dyn ProfileCatalog>,
    /// 설치 판본을 아는 서버 별칭
    installed: BTreeSet<String>,
}

impl Broker for Decisions {
    /// 이 프로세스는 도구를 호출하지 않는다.
    fn call(&self, _: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(std::future::ready(Err(Error::Unavailable)))
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        Box::pin(async move {
            if allow {
                let stored = self.ledger.approval(nonce.clone()).await?;
                let approval = &stored.approval;
                let unknown = self
                    .catalog
                    .current(&approval.attempt.profile)
                    .is_some_and(|snapshot| holds(&snapshot, &approval.tool, &self.installed));
                if unknown {
                    return Err(Error::Unavailable);
                }
            }
            self.inner.decide(nonce, actor, allow).await
        })
    }
}

/// 승인 결정을 맡길 브로커. 설치 판본(`revisions`)을 아는 서버의 외부 도구와 내장 판정은 실제 브로커가 판정한다.
pub fn broker(
    registry: Arc<Registry>,
    store: Arc<Store>,
    revisions: &BTreeMap<String, Revision>,
    tracer: Arc<dyn Tracer>,
    routes: Arc<dyn ScheduleRoutes>,
) -> Arc<dyn Broker> {
    let installed: BTreeMap<String, String> = revisions
        .iter()
        .map(|(alias, revision)| (alias.clone(), revision.0.clone()))
        .collect();
    let known = installed.keys().cloned().collect();
    let ledger = store.clone() as Arc<dyn CallLedger>;
    let schedules = store.clone() as Arc<dyn UserSchedules>;
    // 승인 클릭이 전송 카드의 수신자를 다시 보고, 빠졌으면 회신 저장소가 그 발생을 끝낸다(원래 대화 안내 한 번·옛 카드 무효·미발송 영수증)
    let replies = store.clone() as Arc<dyn ScheduledReplies>;
    let service = BrokerService::new(
        Parts {
            catalog: registry.clone() as Arc<dyn ProfileCatalog>,
            ledger: ledger.clone(),
            effects: store.clone() as Arc<dyn EffectJournal>,
            memory: store.clone() as Arc<dyn MemoryStore>,
            audit: store as Arc<dyn AuditLog>,
            runner: Arc::new(InstalledOnly(installed)),
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
        installed: known,
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

    /// `agenda.delete`를 이 서버들이 선언한 프로필 스냅샷
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
        // 설치 판본을 아는 서버의 도구는 맡긴다
        assert!(!holds(
            &declaring(&["agenda"]),
            &delete,
            &installed(&["agenda"])
        ));
        // 모르는 서버(설치 정보가 없는 호스트)의 도구는 보류한다
        assert!(holds(&declaring(&["agenda"]), &delete, &installed(&[])));
        assert!(holds(
            &declaring(&["agenda"]),
            &delete,
            &installed(&["other"])
        ));
        // 선언한 서버 중 하나라도 모르면 보류한다
        assert!(holds(
            &declaring(&["agenda", "extra"]),
            &delete,
            &installed(&["agenda"])
        ));
        // 어느 서버에도 선언돼 있지 않은 내장 판정(기억 공유 등)은 설치와 무관하게 맡긴다
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
            r#"{"nonce":"n1","title":"삭제","lines":[]}"#,
        );
        let decisions = Decisions {
            inner: inner.clone(),
            ledger: ledger.clone(),
            catalog: TestCatalog::new([snapshot]),
            installed: installed(known),
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
        // 승인: 보류(일시 실패) — 실제 브로커에는 가지도 않는다
        assert!(matches!(
            approve(&decisions, true).await,
            Err(Error::Unavailable)
        ));
        assert_eq!(inner.attempts(), 0);
        // 거절은 설치 판본이 필요 없다
        approve(&decisions, false).await.unwrap();
        assert_eq!(inner.decisions().len(), 1);
        // 도구 호출은 이 프로세스가 하지 않는다
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
        // 서버 설정이 전혀 없는 호스트에서도 내장 판정(어느 서버에도 선언 안 된 도구)은 맡긴다
        let (decisions, inner, _) = rig(declaring(&[]), &[]).await;
        approve(&decisions, true).await.unwrap();
        assert_eq!(inner.attempts(), 1);
    }

    #[test]
    fn the_runner_only_answers_installed_revisions_and_never_runs_tools() {
        let runner = InstalledOnly(BTreeMap::from([("agenda".to_owned(), "r1".to_owned())]));
        assert_eq!(runner.installed("agenda").as_deref(), Some("r1"));
        assert_eq!(runner.installed("other"), None);
    }
}

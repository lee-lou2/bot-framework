//! 모델 별칭 → 실행기(`providers`). 프로필이 허용한 별칭만 고르고, 데모 모델은 외부 키·VM 없이 결정적으로 실행한다.
//!
//! `claude-cli`는 모델 샌드박스(Apple VM 어댑터)와 상류 전송이 조립돼 실행기가 주어졌을 때만 시작한다(`claude`; worker가 호스트 설정의
//! `sandbox.apple`로 조립한다). 없으면 명시적으로 거절한다 — 별칭만 있다고 일반 실행기로 폴백하지 않는다.
//!
//! `anthropic-api`·`openai-api`는 VM·CLI 없이 워커가 상류 API를 직접 부르는 Step 실행기다(`bot_runtime::model::ApiRunner`). 서비스 키를 읽는 보관소(가림 장치를 거친다)와
//! 요청 직전의 활성화 판본 확인(`ActiveGate`)이 조립돼 있어야 시작한다.
use super::watched::Watched;
use crate::{
    execution::{ApiProvider, Provider},
    host::HostFile,
};
use bot_core::{
    id::{ProfileId, SecretName},
    port::Reply,
};
use bot_gateway::active::ActiveCheck;
use bot_runtime::{
    Models,
    model::{ApiEndpoint, ApiRunner, ApiSettings, DemoModel, ModelRunner, SendGate},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// 요청을 보내기 직전의 판본 확인을 이 프로세스의 활성화 판본 확인(`ActiveCheck`)에 잇는다: 구판은 철회된 상류로 대화도 키도 보내지 않는다.
pub struct ActiveGate(pub Arc<dyn ActiveCheck>);

impl SendGate for ActiveGate {
    fn verify(&self) -> Reply<'_, ()> {
        self.0.verify()
    }
}

/// API 직접 호출 실행기를 만들 때 쓰는 이 프로세스의 조각
#[derive(Clone)]
pub struct ApiParts {
    /// 서비스 키를 읽는 보관소(가림 장치를 거치는 것만 받는다)
    pub secrets: Arc<Watched>,
    /// 요청 직전의 판본 확인
    pub gate: Arc<dyn SendGate>,
}

pub struct ProviderModels {
    /// 프로필 → 허용한 모델 별칭
    allowed: BTreeMap<String, BTreeSet<String>>,
    runners: BTreeMap<String, Arc<dyn ModelRunner>>,
}

impl Models for ProviderModels {
    fn runner(&self, profile: &ProfileId, alias: &str) -> Option<Arc<dyn ModelRunner>> {
        self.allowed
            .get(&profile.0)
            .is_some_and(|aliases| aliases.contains(alias))
            .then(|| self.runners.get(alias).cloned())
            .flatten()
    }
}

/// API 직접 호출 별칭 하나의 실행기. 조각이 없거나 주소·키 설정이 규칙에 어긋나면 Err(기동 실패).
fn api_runner(
    alias: &str,
    config: &ApiProvider,
    parts: Option<&ApiParts>,
) -> Result<Arc<dyn ModelRunner>, String> {
    let parts = parts.ok_or_else(|| {
        format!("모델 별칭 {alias}: API 직접 호출 실행기가 조립되지 않아 시작하지 않아요")
    })?;
    let endpoint = ApiEndpoint::parse(&config.endpoint)
        .map_err(|reason| format!("providers.{alias}.endpoint: {reason}"))?;
    let secret =
        match config.secret.as_deref() {
            Some(name) => Some(SecretName::parse(name).ok_or_else(|| {
                format!("providers.{alias}.secret이 자격 이름 규칙에 맞지 않아요")
            })?),
            None => None,
        };
    let settings = ApiSettings {
        kind: config.kind,
        endpoint,
        model: config.model.clone(),
        secret,
        max_tokens: config.max_tokens,
        limit_field: config.limit_field,
    };
    let runner = ApiRunner::new(settings, parts.secrets.clone(), parts.gate.clone())
        .map_err(|reason| format!("모델 별칭 {alias}: {reason}"))?;
    Ok(Arc::new(runner))
}

/// 프로필이 허용한 모든 별칭의 실행기를 만든다. 실행 방법이 없거나 아직 쓸 수 없는 것이면 Err(기동 실패). `claude`는 `claude-cli`
/// 별칭이 쓸 CLI 실행기(조립돼 있을 때만), `api`는 API 직접 호출 별칭이 쓸 조각(조립돼 있을 때만)이다.
pub fn build(
    host: &HostFile,
    claude: Option<Arc<dyn ModelRunner>>,
    api: Option<&ApiParts>,
) -> Result<ProviderModels, String> {
    let mut runners: BTreeMap<String, Arc<dyn ModelRunner>> = BTreeMap::new();
    for (profile, aliases) in &host.catalog.models {
        for alias in aliases {
            if runners.contains_key(alias) {
                continue;
            }
            let provider = host.providers.get(alias).ok_or_else(|| {
                format!(
                    "프로필 {profile}이 허용한 모델 별칭 {alias}의 실행 방법이 없어요 — 호스트 설정의 providers.{alias}를 적어요(예: {{\"kind\": \"demo\"}})"
                )
            })?;
            let runner: Arc<dyn ModelRunner> = match provider {
                Provider::Demo => Arc::new(DemoModel),
                Provider::ClaudeCli { .. } => claude.clone().ok_or_else(|| {
                    format!(
                        "모델 별칭 {alias}: claude-cli 실행기가 조립되지 않아 시작하지 않아요(호스트 설정의 sandbox.apple이 필요해요)"
                    )
                })?,
                Provider::Api(config) => api_runner(alias, config, api)?,
            };
            runners.insert(alias.clone(), runner);
        }
    }
    Ok(ProviderModels {
        allowed: host.catalog.models.clone(),
        runners,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host, load};
    use bot_core::port::SecretStore;
    use bot_runtime::model::{ApiKind, LimitField};

    fn parsed(extra: &str) -> HostFile {
        host::parse(&format!(
            r#"{{"version": 1, "models": {{"personal": ["assistant"], "work": ["other"]}}{extra}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn only_the_aliases_a_profile_allows_resolve_and_each_needs_a_provider() {
        let host =
            parsed(r#", "providers": {"assistant": {"kind": "demo"}, "other": {"kind": "demo"}}"#);
        let models = build(&host, None, None).unwrap();
        let personal = ProfileId("personal".into());
        assert!(models.runner(&personal, "assistant").is_some());
        // 다른 프로필이 허용한 별칭·허용하지 않은 별칭·모르는 프로필은 실행기를 받지 못한다
        assert!(models.runner(&personal, "other").is_none());
        assert!(
            models
                .runner(&ProfileId("work".into()), "assistant")
                .is_none()
        );
        assert!(
            models
                .runner(&ProfileId("stranger".into()), "assistant")
                .is_none()
        );
        // 허용한 별칭의 실행 방법이 없으면 시작하지 않는다
        let missing = parsed(r#", "providers": {"assistant": {"kind": "demo"}}"#);
        let error = build(&missing, None, None).err().unwrap();
        assert!(
            error.contains("other") && error.contains("providers.other"),
            "{error}"
        );
        // 허용 별칭이 없는 호스트는 실행기도 없다(아무것도 요구하지 않는다)
        assert!(build(&host::parse(r#"{"version": 1}"#).unwrap(), None, None).is_ok());
    }

    struct NoSecrets;

    impl SecretStore for NoSecrets {
        fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn bot_core::port::ProfileSecrets>> {
            Box::pin(async { Err(bot_core::port::Error::Missing) })
        }
    }

    struct Open;

    impl SendGate for Open {
        fn verify(&self) -> Reply<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    fn parts() -> ApiParts {
        ApiParts {
            secrets: Watched::plain(Arc::new(NoSecrets)),
            gate: Arc::new(Open),
        }
    }

    #[test]
    fn api_aliases_start_only_with_the_assembled_parts_and_resolve_to_step_runners() {
        let host = parsed(
            r#", "providers": {
                "assistant": {"kind": "anthropic-api", "model": "m", "endpoint": "https://api.example", "secret": "key"},
                "other": {"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"}}"#,
        );
        // 조각(키 보관소·판본 확인)이 없으면 시작하지 않는다: 별칭만 있다고 다른 실행기로 폴백하지 않는다
        let error = build(&host, None, None).err().unwrap();
        assert!(
            error.contains("API 직접 호출") && error.contains("assistant"),
            "{error}"
        );
        let models = build(&host, None, Some(&parts())).unwrap();
        let personal = ProfileId("personal".into());
        let runner = models.runner(&personal, "assistant").unwrap();
        assert_eq!(runner.kind(), bot_runtime::model::RunnerKind::Step);
        // 허용하지 않은 별칭은 실행기를 받지 못한다(다른 프로필의 별칭)
        assert!(models.runner(&personal, "other").is_none());
        let work = models.runner(&ProfileId("work".into()), "other").unwrap();
        assert_eq!(work.kind(), bot_runtime::model::RunnerKind::Step);
    }

    #[test]
    fn an_api_alias_with_a_remote_endpoint_and_no_key_cannot_start() {
        // 호스트 설정 단계에서 이미 거절하지만(`parse_providers`) 실행기도 같은 규칙을 한 번 더 지킨다
        let host =
            parsed(r#", "providers": {"assistant": {"kind": "demo"}, "other": {"kind": "demo"}}"#);
        let mut forced = host;
        forced.providers.insert(
            "assistant".into(),
            Provider::Api(ApiProvider {
                kind: ApiKind::OpenAi,
                model: "m".into(),
                endpoint: "https://api.example/v1".into(),
                secret: None,
                max_tokens: None,
                limit_field: LimitField::MaxTokens,
            }),
        );
        let error = build(&forced, None, Some(&parts())).err().unwrap();
        assert!(
            error.contains("assistant") && error.contains("loopback"),
            "{error}"
        );
        forced.providers.insert(
            "assistant".into(),
            Provider::Api(ApiProvider {
                kind: ApiKind::OpenAi,
                model: "m".into(),
                endpoint: "http://api.example/v1".into(),
                secret: Some("key".into()),
                max_tokens: None,
                limit_field: LimitField::MaxTokens,
            }),
        );
        let error = build(&forced, None, Some(&parts())).err().unwrap();
        assert!(error.contains("providers.assistant.endpoint"), "{error}");
    }

    #[tokio::test]
    async fn the_send_gate_follows_the_processs_activation_check() {
        use bot_gateway::testing::ports::MemoryActive;
        let active = MemoryActive::new(Some("set-1"));
        let gate = ActiveGate(active.check("set-1"));
        assert!(gate.verify().await.is_ok());
        // 다른 프로세스가 집합을 바꿨다: 이 프로세스는 구판이라 보낼 수 없다
        active.set(Some("set-2"));
        assert!(matches!(
            gate.verify().await,
            Err(bot_core::port::Error::Stale)
        ));
    }

    #[test]
    fn a_claude_cli_alias_is_refused_without_an_assembled_cli_runner() {
        let host = parsed(
            r#", "providers": {"assistant": {"kind": "claude-cli", "model": "m", "endpoint": "https://up.example/v1", "secret": "key"},
                "other": {"kind": "demo"}}"#,
        );
        let error = build(&host, None, None).err().unwrap();
        assert!(
            error.contains("claude-cli") && error.contains("assistant"),
            "{error}"
        );
    }

    #[test]
    fn a_claude_cli_alias_runs_on_the_given_cli_runner_only() {
        let host = parsed(
            r#", "providers": {"assistant": {"kind": "claude-cli", "model": "m", "endpoint": "https://up.example/v1", "secret": "key"},
                "other": {"kind": "demo"}}"#,
        );
        let cli: Arc<dyn ModelRunner> = Arc::new(DemoModel);
        let models = build(&host, Some(cli.clone()), None).unwrap();
        let personal = ProfileId("personal".into());
        assert!(Arc::ptr_eq(
            &models.runner(&personal, "assistant").unwrap(),
            &cli
        ));
        // 데모 별칭은 그대로 데모 실행기이고 CLI 실행기를 받지 않는다
        assert!(!Arc::ptr_eq(
            &models.runner(&ProfileId("work".into()), "other").unwrap(),
            &cli
        ));
    }

    #[test]
    fn the_built_in_demo_host_runs_the_demo_model() {
        let demo =
            load::load_host(&crate::settings::Settings::read(&|_| None).unwrap(), 501).unwrap();
        let models = build(&demo, None, None).unwrap();
        assert!(
            models
                .runner(&ProfileId("personal".into()), "assistant")
                .is_some()
        );
    }
}

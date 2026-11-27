//! Resolves allowed profile model aliases into `DemoModel`, VM CLI runners (`claude-cli`, `codex-cli`, `agy-cli`), or `ApiRunner`.
use super::watched::Watched;
use crate::{
    execution::{ApiProvider, Provider},
    host::HostFile,
};
use bot_core::{
    active::ActiveCheck,
    id::{ProfileId, SecretName},
};
use bot_runtime::{
    Models,
    model::{ApiEndpoint, ApiRunner, ApiSettings, CliKind, DemoModel, ModelRunner},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone)]
pub struct ApiParts {
    /// Accepts `Watched` only so every read provider key enters the redactor before use.
    pub secrets: Arc<Watched>,
    pub gate: Arc<dyn ActiveCheck>,
}

pub struct ProviderModels {
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

fn api_runner(
    alias: &str,
    config: &ApiProvider,
    parts: Option<&ApiParts>,
) -> Result<Arc<dyn ModelRunner>, String> {
    let parts = parts.ok_or_else(|| {
        format!("model alias {alias}: direct API runner is not assembled; refusing to start")
    })?;
    let endpoint = ApiEndpoint::parse(&config.endpoint)
        .map_err(|reason| format!("providers.{alias}.endpoint: {reason}"))?;
    let secret = match config.secret.as_deref() {
        Some(name) => Some(
            SecretName::parse(name)
                .ok_or_else(|| format!("providers.{alias}.secret is not a valid secret name"))?,
        ),
        None => None,
    };
    let settings = ApiSettings {
        kind: config.kind,
        endpoint,
        model: config.model.clone(),
        secret,
        max_tokens: config.max_tokens,
        limit_field: config.limit_field,
        effort: config.effort,
    };
    let runner = ApiRunner::new(settings, parts.secrets.clone(), parts.gate.clone())
        .map_err(|reason| format!("model alias {alias}: {reason}"))?;
    Ok(Arc::new(runner))
}

/// Builds runners for every profile-allowed alias; refuses to start rather than falling back if a runner is missing.
pub fn build(
    host: &HostFile,
    cli_runners: &BTreeMap<CliKind, Arc<dyn ModelRunner>>,
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
                    "profile {profile} allows model alias {alias}, but providers.{alias} is missing from host config (e.g. {{\"kind\": \"demo\"}})"
                )
            })?;
            let runner: Arc<dyn ModelRunner> = match provider {
                Provider::Demo => Arc::new(DemoModel),
                Provider::Cli(cfg) => cli_runners.get(&cfg.kind).cloned().ok_or_else(|| {
                    format!(
                        "model alias {alias}: {} runner is not assembled (sandbox.apple is required in host config)",
                        cfg.kind.as_str()
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
    use bot_core::port::{Reply, SecretStore};
    use bot_runtime::model::{ApiKind, LimitField};

    fn parsed(extra: &str) -> HostFile {
        host::parse(&format!(
            r#"{{"version": 1, "models": {{"personal": ["assistant"], "work": ["other"]}}{extra}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn only_the_aliases_a_profile_allows_resolve_and_each_needs_a_provider() {
        let empty = BTreeMap::new();
        let host =
            parsed(r#", "providers": {"assistant": {"kind": "demo"}, "other": {"kind": "demo"}}"#);
        let models = build(&host, &empty, None).unwrap();
        let personal = ProfileId("personal".into());
        assert!(models.runner(&personal, "assistant").is_some());
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
        let missing = parsed(r#", "providers": {"assistant": {"kind": "demo"}}"#);
        let error = build(&missing, &empty, None).err().unwrap();
        assert!(
            error.contains("other") && error.contains("providers.other"),
            "{error}"
        );
        assert!(build(&host::parse(r#"{"version": 1}"#).unwrap(), &empty, None).is_ok());
    }

    struct NoSecrets;

    impl SecretStore for NoSecrets {
        fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn bot_core::port::ProfileSecrets>> {
            Box::pin(async { Err(bot_core::port::Error::Missing) })
        }
    }

    struct Open;

    impl ActiveCheck for Open {
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
        let empty = BTreeMap::new();
        let host = parsed(
            r#", "providers": {
                "assistant": {"kind": "anthropic-api", "model": "m", "endpoint": "https://api.example", "secret": "key"},
                "other": {"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"}}"#,
        );
        let error = build(&host, &empty, None).err().unwrap();
        assert!(
            error.contains("direct API") && error.contains("assistant"),
            "{error}"
        );
        let models = build(&host, &empty, Some(&parts())).unwrap();
        let personal = ProfileId("personal".into());
        let runner = models.runner(&personal, "assistant").unwrap();
        assert_eq!(runner.kind(), bot_runtime::model::RunnerKind::Step);
        assert!(models.runner(&personal, "other").is_none());
        let work = models.runner(&ProfileId("work".into()), "other").unwrap();
        assert_eq!(work.kind(), bot_runtime::model::RunnerKind::Step);
    }

    #[test]
    fn an_api_alias_with_a_remote_endpoint_and_no_key_cannot_start() {
        let empty = BTreeMap::new();
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
                effort: None,
            }),
        );
        let error = build(&forced, &empty, Some(&parts())).err().unwrap();
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
                effort: None,
            }),
        );
        let error = build(&forced, &empty, Some(&parts())).err().unwrap();
        assert!(error.contains("providers.assistant.endpoint"), "{error}");
    }

    #[tokio::test]
    async fn the_send_gate_follows_the_processs_activation_check() {
        use bot_gateway::testing::ports::MemoryActive;
        let active = MemoryActive::new(Some("set-1"));
        let gate = active.check("set-1");
        assert!(gate.verify().await.is_ok());
        active.set(Some("set-2"));
        assert!(matches!(
            gate.verify().await,
            Err(bot_core::port::Error::Stale)
        ));
    }

    #[test]
    fn cli_aliases_fail_without_their_matching_runner_and_dispatch_by_kind() {
        let empty = BTreeMap::new();
        let host = host::parse(
            r#"{"version": 1, "models": {"personal": ["claude", "codex", "agy"], "work": ["other"]},
                "providers": {
                    "claude": {"kind": "claude-cli", "model": "m", "endpoint": "https://up.example/v1", "secret": "key"},
                    "codex": {"kind": "codex-cli", "model": "m", "endpoint": "https://api.openai.com/v1", "secret": "key"},
                    "agy": {"kind": "agy-cli", "model": "m", "endpoint": "https://generativelanguage.googleapis.com", "secret": "key"},
                    "other": {"kind": "demo"}}}"#,
        )
        .unwrap();
        let error = build(&host, &empty, None).err().unwrap();
        assert!(
            error.contains("claude-cli")
                || error.contains("codex-cli")
                || error.contains("agy-cli"),
            "{error}"
        );
        let r_claude: Arc<dyn ModelRunner> = Arc::new(DemoModel);
        let r_codex: Arc<dyn ModelRunner> = Arc::new(DemoModel);
        let r_agy: Arc<dyn ModelRunner> = Arc::new(DemoModel);
        let partial = BTreeMap::from([(CliKind::Claude, r_claude.clone())]);
        assert!(build(&host, &partial, None).is_err());
        let all = BTreeMap::from([
            (CliKind::Claude, r_claude.clone()),
            (CliKind::Codex, r_codex.clone()),
            (CliKind::Agy, r_agy.clone()),
        ]);
        let models = build(&host, &all, None).unwrap();
        let personal = ProfileId("personal".into());
        assert!(Arc::ptr_eq(
            &models.runner(&personal, "claude").unwrap(),
            &r_claude
        ));
        assert!(Arc::ptr_eq(
            &models.runner(&personal, "codex").unwrap(),
            &r_codex
        ));
        assert!(Arc::ptr_eq(
            &models.runner(&personal, "agy").unwrap(),
            &r_agy
        ));
        assert!(!Arc::ptr_eq(
            &models.runner(&ProfileId("work".into()), "other").unwrap(),
            &r_claude
        ));
    }

    #[test]
    fn the_built_in_demo_host_runs_the_demo_model() {
        let demo =
            load::load_host(&crate::settings::Settings::read(&|_| None).unwrap(), 501).unwrap();
        let models = build(&demo, &BTreeMap::new(), None).unwrap();
        assert!(
            models
                .runner(&ProfileId("personal".into()), "assistant")
                .is_some()
        );
    }
}

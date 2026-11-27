//! Resolves allowed profile model aliases into `DemoModel`, VM CLI runners (`claude-cli`, `codex-cli`, `agy-cli`), or `ApiRunner`.
use super::watched::Watched;
use crate::{
    execution::{ApiProvider, Provider, fallback_chain, reachable_aliases},
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
    fallbacks: BTreeMap<String, Vec<String>>,
}

impl Models for ProviderModels {
    fn runner(&self, profile: &ProfileId, alias: &str) -> Option<Arc<dyn ModelRunner>> {
        self.allowed
            .get(&profile.0)
            .is_some_and(|aliases| aliases.contains(alias))
            .then(|| self.runners.get(alias).cloned())
            .flatten()
    }

    fn fallbacks(&self, profile: &ProfileId, alias: &str) -> Vec<String> {
        if !self
            .allowed
            .get(&profile.0)
            .is_some_and(|aliases| aliases.contains(alias))
        {
            return Vec::new();
        }
        self.fallbacks.get(alias).cloned().unwrap_or_default()
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

/// Builds runners for every profile-allowed alias and its fallback chain; refuses to start if any runner is missing.
pub fn build(
    host: &HostFile,
    cli_runners: &BTreeMap<CliKind, Arc<dyn ModelRunner>>,
    api: Option<&ApiParts>,
) -> Result<ProviderModels, String> {
    let mut allowed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut runners: BTreeMap<String, Arc<dyn ModelRunner>> = BTreeMap::new();
    let mut fallbacks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (profile, roots) in &host.catalog.models {
        let reachable = reachable_aliases(&host.providers, roots.iter().map(String::as_str));
        for alias in &reachable {
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
            let chain = fallback_chain(&host.providers, alias);
            if !chain.is_empty() {
                fallbacks.insert(alias.clone(), chain);
            }
            runners.insert(alias.clone(), runner);
        }
        allowed.insert(profile.clone(), reachable);
    }
    Ok(ProviderModels {
        allowed,
        runners,
        fallbacks,
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
                fallback: None,
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
                fallback: None,
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

    #[test]
    fn fallback_chain_resolves_runners_and_exposes_ordered_fallback_aliases() {
        let host = host::parse(
            r#"{"version": 1, "models": {"personal": ["primary"]},
                "providers": {
                    "primary": {"kind": "agy-cli", "model": "gemini-4-hydrogen", "endpoint": "https://generativelanguage.googleapis.com", "secret": "g-key", "effort": "xhigh", "fallback": "backup"},
                    "backup": {"kind": "claude-cli", "model": "claude-sonnet-5-5", "endpoint": "https://proxy.example", "secret": "c-key", "effort": "medium"}}}"#,
        )
        .unwrap();
        let r_agy: Arc<dyn ModelRunner> = Arc::new(DemoModel);
        let r_claude: Arc<dyn ModelRunner> = Arc::new(DemoModel);
        // Missing fallback CLI runner must fail startup.
        let only_agy = BTreeMap::from([(CliKind::Agy, r_agy.clone())]);
        assert!(build(&host, &only_agy, None).is_err());

        let both = BTreeMap::from([
            (CliKind::Agy, r_agy.clone()),
            (CliKind::Claude, r_claude.clone()),
        ]);
        let models = build(&host, &both, None).unwrap();
        let personal = ProfileId("personal".into());
        assert_eq!(models.fallbacks(&personal, "primary"), vec!["backup"]);
        assert!(
            models
                .fallbacks(&ProfileId("work".into()), "primary")
                .is_empty()
        );
        assert!(Arc::ptr_eq(
            &models.runner(&personal, "primary").unwrap(),
            &r_agy
        ));
        assert!(Arc::ptr_eq(
            &models.runner(&personal, "backup").unwrap(),
            &r_claude
        ));
    }
}

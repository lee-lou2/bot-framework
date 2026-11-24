//! 모델 별칭 → 실행기(`providers`). 프로필이 허용한 별칭만 고르고, 데모 모델은 외부 키·VM 없이 결정적으로 실행한다.
//!
//! `claude-cli`는 모델 샌드박스(Apple VM 어댑터)와 상류 전송이 조립돼 실행기가 주어졌을 때만 시작한다(`claude`; worker가 호스트 설정의
//! `sandbox.apple`로 조립한다). 없으면 명시적으로 거절한다 — 별칭만 있다고 일반 실행기로 폴백하지 않는다.
use crate::{execution::Provider, host::HostFile};
use bot_core::id::ProfileId;
use bot_runtime::{
    Models,
    model::{DemoModel, ModelRunner},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

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

/// 프로필이 허용한 모든 별칭의 실행기를 만든다. 실행 방법이 없거나 아직 쓸 수 없는 것이면 Err(기동 실패). `claude`는 `claude-cli`
/// 별칭이 쓸 CLI 실행기(조립돼 있을 때만)다.
pub fn build(
    host: &HostFile,
    claude: Option<Arc<dyn ModelRunner>>,
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
        let models = build(&host, None).unwrap();
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
        let error = build(&missing, None).err().unwrap();
        assert!(
            error.contains("other") && error.contains("providers.other"),
            "{error}"
        );
        // 허용 별칭이 없는 호스트는 실행기도 없다(아무것도 요구하지 않는다)
        assert!(build(&host::parse(r#"{"version": 1}"#).unwrap(), None).is_ok());
    }

    #[test]
    fn a_claude_cli_alias_is_refused_without_an_assembled_cli_runner() {
        let host = parsed(
            r#", "providers": {"assistant": {"kind": "claude-cli", "model": "m", "endpoint": "https://up.example/v1", "secret": "key"},
                "other": {"kind": "demo"}}"#,
        );
        let error = build(&host, None).err().unwrap();
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
        let models = build(&host, Some(cli.clone())).unwrap();
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
        let models = build(&demo, None).unwrap();
        assert!(
            models
                .runner(&ProfileId("personal".into()), "assistant")
                .is_some()
        );
    }
}

//! 관측(OTLP 추적·로그·이슈 트래커) 설정이 참조하는 자격 이름.
//! 프로필별 추적 헤더는 `needed_secrets`가, 호스트 단위 로그·이슈 자격은 `host_obs_secrets`가 모은다.
use crate::host::HostFile;
use bot_admin::obs::HostHeaderValue;
use bot_core::{id::ProfileId, profile::Snapshot};
use std::collections::{BTreeMap, BTreeSet};

/// 이 프로필의 수집(OTLP) 헤더가 내보낼 때마다 보관소에서 받는 자격 이름(호스트 `admin.trace`).
pub fn trace_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    use bot_admin::trace::HeaderValue;
    host.admin
        .trace
        .get(&snapshot.id)
        .and_then(|trace| trace.backend.as_ref())
        .into_iter()
        .flat_map(|backend| &backend.headers)
        .filter_map(|(_, value)| match value {
            HeaderValue::Secret(name) => Some(name.as_str().to_owned()),
            HeaderValue::Plain(_) => None,
        })
        .collect()
}

/// 호스트 단위 관측 설정(`admin.logs` 헤더·`admin.issues` 키)이 참조하는 자격 이름(이름 공간별).
pub fn host_obs_secrets(host: &HostFile) -> BTreeMap<ProfileId, BTreeSet<String>> {
    let mut by_profile: BTreeMap<ProfileId, BTreeSet<String>> = BTreeMap::new();
    if let Some(logs) = &host.admin.logs {
        for (_, value) in &logs.headers {
            if let HostHeaderValue::Secret(secret) = value {
                by_profile
                    .entry(secret.profile.clone())
                    .or_default()
                    .insert(secret.name.as_str().to_owned());
            }
        }
    }
    if let Some(issues) = &host.admin.issues
        && let Some(secret) = &issues.key
    {
        by_profile
            .entry(secret.profile.clone())
            .or_default()
            .insert(secret.name.as_str().to_owned());
    }
    by_profile
}

/// 활성 프로필들과 호스트 관측 설정(`admin.logs`·`admin.issues`)이 요구하는 자격 이름을
/// 보관소 이름 공간(`ProfileId`)별로 중복 없이 합친다.
pub fn all_needed_secrets<'a>(
    snapshots: impl IntoIterator<Item = &'a Snapshot>,
    host: &HostFile,
) -> BTreeMap<ProfileId, BTreeSet<String>> {
    let mut merged = host_obs_secrets(host);
    for snapshot in snapshots {
        merged
            .entry(snapshot.id.clone())
            .or_default()
            .extend(super::needed_secrets(snapshot, host));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host;
    use bot_gateway::testing::profile;

    #[test]
    fn all_needed_secrets_deduplicates_host_obs_and_profile_secrets() {
        let host = host::parse(
            r#"{
                "version": 1,
                "admin": {
                    "trace": {"personal": {"endpoint": "https://otel.example/v1/traces", "headers": {"Authorization": {"secret": "shared-key"}}}},
                    "logs": {"endpoint": "https://otel.example/v1/logs", "headers": {"Authorization": {"profile": "personal", "secret": "shared-key"}}},
                    "issues": {"kind": "sentry", "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/", "key": {"secret": "sentry-key"}}
                }
            }"#,
        )
        .unwrap();
        let snap = profile("personal").build();
        let merged = all_needed_secrets([snap.as_ref()], &host);
        assert_eq!(
            merged.get(&ProfileId("personal".into())),
            Some(&BTreeSet::from(["shared-key".to_owned()]))
        );
        assert_eq!(
            merged.get(&ProfileId("host".into())),
            Some(&BTreeSet::from(["sentry-key".to_owned()]))
        );
    }
}

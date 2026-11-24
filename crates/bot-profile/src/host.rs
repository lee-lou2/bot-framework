//! 운영자가 호스트에 등록한 것들. 프로필은 이 별칭만 가리킬 수 있다(미등록 별칭은 활성화 때 거절).
//! 값은 bot-cli가 운영자 설정에서 채운다.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct SlackWorkspace {
    pub channels: BTreeSet<String>,
    /// 이 워크스페이스에서 DM 입구를 받는가
    pub dm: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HostCatalog {
    /// 설치한 MCP 서버(실행 파일·이미지) 별칭
    pub servers: BTreeSet<String>,
    /// 프로필 → 그 프로필에 허용한 모델 별칭. 항목이 없으면 그 프로필은 모델을 쓸 수 없다.
    pub models: BTreeMap<String, BTreeSet<String>>,
    /// 호스트가 OS 사용자에 연결한 CLI 계정 별칭
    pub cli_accounts: BTreeSet<String>,
    /// 워크스페이스 별칭 → 채널·DM
    pub slack: BTreeMap<String, SlackWorkspace>,
    /// 인증된 HTTP 입구 별칭
    pub http_endpoints: BTreeSet<String>,
    /// 프로필 → egress 상한(`host:port`). 항목이 없으면 호스트 상한 없이 도구 선언만 따른다.
    pub egress: BTreeMap<String, BTreeSet<String>>,
}

impl HostCatalog {
    /// README 5분 시작의 호스트: 합성 일정 서버, 로컬 데모 모델, CLI 계정 `owner`.
    pub fn demo() -> Self {
        let set = |items: &[&str]| {
            items
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<BTreeSet<_>>()
        };
        Self {
            servers: set(&["demo-agenda"]),
            models: BTreeMap::from([("personal".to_owned(), set(&["assistant"]))]),
            cli_accounts: set(&["owner"]),
            ..Self::default()
        }
    }

    pub fn allows_model(&self, profile: &str, alias: &str) -> bool {
        self.models.get(profile).is_some_and(|m| m.contains(alias))
    }

    /// 이 프로필에 허용한 모델 별칭(없으면 빈 목록).
    pub fn models_of(&self, profile: &str) -> Vec<&str> {
        self.models
            .get(profile)
            .map(|m| m.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// 도구가 선언한 egress와 운영자 상한의 교집합(상한이 없으면 선언 그대로).
    pub fn effective_egress(&self, profile: &str, declared: &BTreeSet<String>) -> BTreeSet<String> {
        match self.egress.get(profile) {
            Some(cap) => declared.intersection(cap).cloned().collect(),
            None => declared.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_is_intersection_when_capped() {
        let mut host = HostCatalog::demo();
        let declared: BTreeSet<String> = ["a.test:443", "b.test:443"].map(String::from).into();
        assert_eq!(host.effective_egress("personal", &declared), declared);
        host.egress
            .insert("personal".into(), ["b.test:443".to_owned()].into());
        assert_eq!(host.effective_egress("personal", &declared).len(), 1);
        assert!(
            host.allows_model("personal", "assistant") && !host.allows_model("other", "assistant")
        );
    }
}

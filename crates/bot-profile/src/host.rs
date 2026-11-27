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
    /// 인증된 웹훅 HTTP 입구 별칭(`kind: webhook`, 종류를 생략한 옛 설정 포함)
    pub http_endpoints: BTreeSet<String>,
    /// OpenAI 호환 입구(`kind: openai`): 별칭 → 키 슬롯이 가리키는 주체(`http:<별칭>/<주체>`의 주체 부분) 집합. 키 값은 담지 않는다.
    pub openai_endpoints: BTreeMap<String, BTreeSet<String>>,
    /// 프로필 → egress 상한(`host:port`). 항목이 없으면 호스트 상한 없이 도구 선언만 따른다.
    pub egress: BTreeMap<String, BTreeSet<String>>,
    /// 원격(Streamable HTTP) 서버 별칭 → 접속 대상 `host:port`(`servers`에도 있다). 그 서버의 도구는 선언한 egress에 이 대상이 있어야 하고(운영자 상한 안에서),
    /// 단계 원장(`effects`)·`secrets`·`env`를 쓸 수 없다(원격 서버에는 전달할 길이 없다).
    pub remote_servers: BTreeMap<String, String>,
    /// 도구 호출 시간 한도의 천장(초, 호스트 `limits.tool_call_max_secs`). 없으면 기본 60초다. 도구의 실효 한도(선언 또는 기본 60초)가 이 값을 넘으면 활성화를 거절한다.
    pub tool_call_max_secs: Option<u32>,
    /// 프로필 → 호스트가 허용하는 슈퍼 관리자 상한(신원 집합). 항목이 없으면 프로필 선언(`super_admins`)을 그대로 따른다.
    pub super_admins: BTreeMap<String, BTreeSet<String>>,
}

impl HostCatalog {
    /// README 5분 시작의 호스트: 합성 일정 서버, 로컬 데모 모델. 예시 프로필 `examples/profiles/`의 둘을 맞춰 둔다 —
    /// 개인 비서 `personal`(CLI 계정 `owner`)과 팀 템플릿 `team`(CLI 계정 `team`). 입구 별칭은 활성 프로필 전체에서 하나뿐이라 계정이 따로다.
    pub fn demo() -> Self {
        let set = |items: &[&str]| {
            items
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<BTreeSet<_>>()
        };
        Self {
            servers: set(&["demo-agenda"]),
            models: BTreeMap::from([
                ("personal".to_owned(), set(&["assistant"])),
                ("team".to_owned(), set(&["assistant"])),
            ]),
            cli_accounts: set(&["owner", "team"]),
            ..Self::default()
        }
    }

    /// 도구 호출 시간 한도의 천장(초).
    pub fn tool_call_cap(&self) -> u32 {
        self.tool_call_max_secs
            .unwrap_or(bot_core::profile::DEFAULT_TOOL_CALL_SECS)
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

    /// 호스트가 이 프로필에 `super_admins` 상한을 둔 경우, 주어진 주체가 그 상한에 포함되는가.
    /// 상한이 없는 프로필은 프로필 선언을 그대로 허용한다.
    pub fn allows_super_admin(&self, profile: &str, principal: &str) -> bool {
        self.super_admins
            .get(profile)
            .is_none_or(|allowed| allowed.contains(principal))
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

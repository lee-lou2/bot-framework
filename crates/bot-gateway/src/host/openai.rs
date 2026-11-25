//! 호스트 `gateway.http.endpoints.<별칭>`의 OpenAI 호환 입구(`kind: openai`): 정적 API 키 슬롯·총 대기 시간·진행 중 작업 한도.
//! 웹훅 입구(JWT, `kind` 생략 또는 `webhook`)와 같은 JSON 객체에 쓰고 읽을 때 갈라 둔다 — 두 방식의 필드를 섞으면 기동 전에 필드 이름을 밝혀 거절한다.
//! 키 슬롯 이름은 운영자의 이름표이고, `subject`가 기존 규칙 `http:<별칭>/<subject>`의 멤버를 정한다. 여러 슬롯이 같은 `subject`를 가리키는 것은 키 회전용이다.
//! 경로는 루트 `/v1`뿐이라 별칭은 URL에 나오지 않는다: 보낸 키가 입구 하나를 정한다. 그래서 별칭 `v1`은 예약이고, OpenAI 입구가 있으면 웹훅 별칭 `v1`도 둘 수 없다.
use super::{HostError, HttpEndpoint, HttpHost, bad, secret};
use crate::identity::{self, is_alias};
use serde::{Deserialize, Deserializer, de::Error as _};
use std::{collections::BTreeMap, time::Duration};

/// 총 응답 대기(`wait_ms`) 기본값·상한(ms)
pub const DEFAULT_WAIT_MS: u64 = 120_000;
pub const MAX_WAIT_MS: u64 = 300_000;
/// 입구별 아직 끝나지 않은 작업 수 한도 기본값·상한
pub const DEFAULT_MAX_ACTIVE_RUNS: u32 = 32;
pub const MAX_ACTIVE_RUNS: u32 = 256;
/// 입구 하나의 키 슬롯 수 상한(회전 중 둘, 여유를 둔다)
pub const MAX_KEY_SLOTS: usize = 16;
/// 호스트가 `gateway.limits.body_bytes`를 정하지 않았을 때 OpenAI 요청 본문의 기본 한도(웹훅은 64 KiB)
pub const DEFAULT_BODY_BYTES: usize = 256 * 1024;

/// OpenAI 호환 입구 하나.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiEndpoint {
    /// 키 슬롯 이름 → 슬롯. 값은 이 파일에 쓰지 않는다(자격 **이름**만).
    pub keys: BTreeMap<String, KeySlot>,
    /// 한 요청이 붙잡히는 총 시간(ms). 생략하면 120초, 1~300000.
    pub wait_ms: Option<u64>,
    /// 이 입구에서 아직 끝나지 않은 작업의 최대 수(승인·자식·보류로 기다리는 작업 포함). 생략하면 32, 1~256.
    pub max_active_runs: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySlot {
    /// 키 값이 든 자격 이름(바인딩을 가진 프로필의 `bot/<profile>/<name>`에서 읽는다)
    pub secret: String,
    /// `http:<별칭>/<subject>` 멤버를 정하는 주체 이름
    pub subject: String,
}

impl OpenAiEndpoint {
    pub fn wait(&self) -> Duration {
        Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS))
    }

    pub fn max_active(&self) -> u32 {
        self.max_active_runs.unwrap_or(DEFAULT_MAX_ACTIVE_RUNS)
    }
}

/// 웹훅 전용 필드·OpenAI 전용 필드(섞어 쓰면 거절한다)
const WEBHOOK_ONLY: [&str; 5] = [
    "issuer",
    "audience",
    "key",
    "sync_wait_ms",
    "max_token_age_secs",
];
const OPENAI_ONLY: [&str; 3] = ["keys", "wait_ms", "max_active_runs"];

enum Parsed {
    Webhook(HttpEndpoint),
    OpenAi(OpenAiEndpoint),
}

fn endpoint_of(alias: &str, value: serde_json::Value) -> Result<Parsed, String> {
    let path = format!("gateway.http.endpoints.{alias}");
    let serde_json::Value::Object(mut map) = value else {
        return Err(format!("{path}: 객체여야 해요"));
    };
    let openai = match map.remove("kind") {
        None => false,
        Some(serde_json::Value::String(kind)) if kind == "webhook" => false,
        Some(serde_json::Value::String(kind)) if kind == "openai" => true,
        Some(_) => return Err(format!("{path}.kind: webhook 또는 openai여야 해요")),
    };
    let (kind, foreign) = if openai {
        ("openai", WEBHOOK_ONLY.as_slice())
    } else {
        ("webhook", OPENAI_ONLY.as_slice())
    };
    if let Some(field) = foreign.iter().find(|field| map.contains_key(**field)) {
        return Err(format!(
            "{path}.{field}: kind={kind} 입구에는 쓸 수 없는 필드예요(webhook과 openai 필드를 섞지 않아요)"
        ));
    }
    let value = serde_json::Value::Object(map);
    if openai {
        serde_json::from_value(value)
            .map(Parsed::OpenAi)
            .map_err(|e| format!("{path}: {e}"))
    } else {
        serde_json::from_value(value)
            .map(Parsed::Webhook)
            .map_err(|e| format!("{path}: {e}"))
    }
}

impl<'de> Deserialize<'de> for HttpHost {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            listen: Option<String>,
            #[serde(default)]
            remote: bool,
            #[serde(default)]
            endpoints: BTreeMap<String, serde_json::Value>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut host = HttpHost {
            listen: raw.listen,
            remote: raw.remote,
            ..HttpHost::default()
        };
        for (alias, value) in raw.endpoints {
            match endpoint_of(&alias, value).map_err(D::Error::custom)? {
                Parsed::Webhook(endpoint) => {
                    host.endpoints.insert(alias, endpoint);
                }
                Parsed::OpenAi(endpoint) => {
                    host.openai.insert(alias, endpoint);
                }
            }
        }
        Ok(host)
    }
}

/// OpenAI 입구들의 검사(웹훅 입구와의 충돌 포함). 오류에는 필드 경로만 싣고 값은 싣지 않는다.
pub(super) fn validate(http: &HttpHost) -> Result<(), HostError> {
    if http.openai.is_empty() {
        return Ok(());
    }
    if http.endpoints.contains_key("v1") {
        return Err(bad(
            "http.endpoints.v1",
            "OpenAI 입구가 루트 /v1 경로를 쓰므로 webhook 별칭 v1은 함께 둘 수 없어요 — 웹훅 별칭의 이름을 바꾸세요",
        ));
    }
    for (alias, endpoint) in &http.openai {
        let path = format!("http.endpoints.{alias}");
        if !is_alias(alias) {
            return Err(bad(&path, "별칭은 63자 이내 소문자 kebab-case여야 해요"));
        }
        if alias == "v1" || alias.starts_with('_') {
            return Err(bad(&path, "예약된 이름이에요(루트 /v1 경로)"));
        }
        if endpoint.keys.is_empty() || endpoint.keys.len() > MAX_KEY_SLOTS {
            return Err(bad(
                &format!("{path}.keys"),
                &format!("키 슬롯은 1~{MAX_KEY_SLOTS}개여야 해요"),
            ));
        }
        let mut names = BTreeMap::new();
        for (slot, key) in &endpoint.keys {
            let slot_path = format!("{path}.keys.{slot}");
            if !is_alias(slot) {
                return Err(bad(
                    &slot_path,
                    "슬롯 이름은 63자 이내 소문자 kebab-case여야 해요",
                ));
            }
            secret(&format!("{slot_path}.secret"), &key.secret)?;
            if identity::http(alias, &key.subject).is_none() {
                return Err(bad(
                    &format!("{slot_path}.subject"),
                    "주체 이름은 영숫자와 . _ - @ + 만, 128자 이내여야 해요",
                ));
            }
            // 두 슬롯이 같은 자격을 가리키면 한 키 값이 두 슬롯에 맞아 인증이 모호해진다
            if let Some(first) = names.insert(&key.secret, slot) {
                return Err(bad(
                    &format!("{slot_path}.secret"),
                    &format!(
                        "슬롯 {first}도 같은 자격 이름을 써요(키 한 개가 두 슬롯에 맞게 돼요)"
                    ),
                ));
            }
        }
        if endpoint
            .wait_ms
            .is_some_and(|ms| ms == 0 || ms > MAX_WAIT_MS)
        {
            return Err(bad(&path, &format!("wait_ms는 1~{MAX_WAIT_MS}이어야 해요")));
        }
        if endpoint
            .max_active_runs
            .is_some_and(|n| n == 0 || n > MAX_ACTIVE_RUNS)
        {
            return Err(bad(
                &path,
                &format!("max_active_runs는 1~{MAX_ACTIVE_RUNS}이어야 해요"),
            ));
        }
    }
    Ok(())
}

/// API 키 값이 쓸 수 있는 모양: ASCII 그래픽 문자(공백 없음) 32~256바이트. 인증(요청의 키와 보관소의 값)과 시작 전 점검이 같은 규칙을 쓴다.
pub fn key_shaped(bytes: &[u8]) -> bool {
    (MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&bytes.len()) && bytes.iter().all(u8::is_ascii_graphic)
}

const MIN_KEY_BYTES: usize = 32;
const MAX_KEY_BYTES: usize = 256;

#[cfg(test)]
mod tests;

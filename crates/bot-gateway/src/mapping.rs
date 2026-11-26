//! 게이트웨이의 실제 신원·입구 매핑과 인증 설정의 지문(비밀 값 제외).
//!
//! 프로필 판본은 그대로여도 호스트가 정한 매핑(CLI 계정이 가리키는 OS 사용자, Slack 워크스페이스·채널 ID, HTTP 입구의
//! 발급자·대상·키 이름)이 바뀌면 누가 무엇에 접근하는지가 달라진다. 이 지문을 활성 판본에 묶어 매핑 변경도 활성화
//! 사건이 되게 한다 — 옛 매핑으로 뜬 프로세스는 구판으로 닫히고, 매핑이 다른 프로세스는 명시 활성화 없이는 합류하지 못한다.
//!
//! 지문에 넣는 것: CLI 계정 별칭 → **실제 uid**, Slack 워크스페이스 별칭 → (팀·앱 ID·채널 별칭→ID·DM 여부·수신 방식·자격 **이름**),
//! HTTP 입구 별칭 → (발급자·대상·키 **이름**·토큰 최대 수명), OpenAI 호환 입구 별칭 → 키 슬롯별 (슬롯 이름·자격 **이름**·주체).
//! OpenAI 입구가 없는 호스트의 지문 바이트는 그 기능이 생기기 전과 같다(구간 자체를 넣지 않는다 — 이미 활성화된 호스트가 재활성화를 요구받지 않는다).
//! 넣지 않는 것: 자격 **값**(회전은 재활성화 없이 반영된다), 속도·본문·실행 수 한도, 대기 시간, 소켓·수신 주소, 관리 화면 주소(신원과 무관한 운영 값).
use crate::host::{CliAccount, HostConfig, HostError, resolve_uid};
use sha2::{Digest, Sha256};

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn optional(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(text) => {
            hash.update([1]);
            field(hash, text.as_bytes());
        }
        None => hash.update([0]),
    }
}

fn count(hash: &mut Sha256, n: usize) {
    hash.update((n as u64).to_be_bytes());
}

/// 지금 호스트에서 OS 사용자 이름을 풀어 지문을 만든다.
pub fn fingerprint(config: &HostConfig) -> Result<[u8; 32], HostError> {
    fingerprint_with(config, &resolve_uid)
}

/// OS 사용자 해석을 바꿔 지문을 만든다(시험용·재사용).
pub fn fingerprint_with(
    config: &HostConfig,
    uid_of: &dyn Fn(&CliAccount) -> Result<u32, HostError>,
) -> Result<[u8; 32], HostError> {
    let mut hash = Sha256::new();
    hash.update(b"bot-gateway-mapping-v1\0");
    hash.update(b"cli");
    count(&mut hash, config.cli.accounts.len());
    for (alias, account) in &config.cli.accounts {
        field(&mut hash, alias.as_bytes());
        hash.update(uid_of(account)?.to_be_bytes());
    }
    hash.update(b"slack");
    count(&mut hash, config.slack.workspaces.len());
    for (alias, ws) in &config.slack.workspaces {
        field(&mut hash, alias.as_bytes());
        field(&mut hash, ws.team_id.as_bytes());
        optional(&mut hash, ws.app_id.as_deref());
        hash.update([u8::from(ws.dm)]);
        hash.update([match ws.receive {
            crate::host::Receive::Socket => 0,
            crate::host::Receive::Http => 1,
        }]);
        field(&mut hash, ws.bot_token.as_bytes());
        optional(&mut hash, ws.app_token.as_deref());
        optional(&mut hash, ws.signing_secret.as_deref());
        count(&mut hash, ws.channels.len());
        for (channel, id) in &ws.channels {
            field(&mut hash, channel.as_bytes());
            field(&mut hash, id.as_bytes());
        }
    }
    hash.update(b"http");
    count(&mut hash, config.http.endpoints.len());
    for (alias, endpoint) in &config.http.endpoints {
        field(&mut hash, alias.as_bytes());
        field(&mut hash, endpoint.issuer.as_bytes());
        field(&mut hash, endpoint.audience.as_bytes());
        field(&mut hash, endpoint.key.as_bytes());
        match endpoint.max_token_age_secs {
            Some(secs) => {
                hash.update([1]);
                hash.update(secs.to_be_bytes());
            }
            None => hash.update([0]),
        }
    }
    if !config.http.openai.is_empty() {
        hash.update(b"openai");
        count(&mut hash, config.http.openai.len());
        for (alias, endpoint) in &config.http.openai {
            field(&mut hash, alias.as_bytes());
            count(&mut hash, endpoint.keys.len());
            for (slot, key) in &endpoint.keys {
                field(&mut hash, slot.as_bytes());
                field(&mut hash, key.secret.as_bytes());
                field(&mut hash, key.subject.as_bytes());
            }
        }
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn base() -> Value {
        json!({
            "cli": {"accounts": {"owner": {"uid": 501}, "guest": {"os_user": "guest"}}, "socket": "/tmp/a.sock"},
            "slack": {"workspaces": {"home": {"team_id": "T1", "app_id": "A1", "bot_token": "bot-token",
                "app_token": "app-token", "dm": true, "channels": {"planning": "C1", "ops": "C2"}}}},
            "http": {"listen": "127.0.0.1:9000", "endpoints": {"inbox": {"issuer": "i", "audience": "a",
                "key": "inbox-key", "sync_wait_ms": 500, "max_token_age_secs": 600}}},
            "limits": {"body_bytes": 1000},
            "admin_url": "http://127.0.0.1:8787/ui"
        })
    }

    /// `guest`는 uid 502로 푼다(실제 OS 조회 없이)
    fn print(value: Value) -> [u8; 32] {
        let config = HostConfig::from_value(value).unwrap();
        fingerprint_with(&config, &|account| Ok(account.uid.unwrap_or(502))).unwrap()
    }

    /// 설정 검증을 거치지 않은 값의 지문(검증이 거절하는 모양도 지문 계산은 정의돼 있다)
    fn unvalidated_print(value: Value) -> [u8; 32] {
        let config: HostConfig = serde_json::from_value(value).unwrap();
        fingerprint_with(&config, &|account| Ok(account.uid.unwrap_or(502))).unwrap()
    }

    fn changed(edit: impl FnOnce(&mut Value)) -> [u8; 32] {
        let mut value = base();
        edit(&mut value);
        print(value)
    }

    fn hex(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 웹훅 입구만 있는 호스트의 지문 바이트는 OpenAI 입구가 생기기 전과 같다(이미 활성화된 호스트가 재활성화를 요구받지 않는다).
    #[test]
    fn a_host_without_openai_entrances_keeps_its_fingerprint_bytes() {
        assert_eq!(
            hex(print(base())),
            "59bd29edd4e4f8b1a48c39bd3ddf43768c7076b5c8dbdb6fd319540881cd987d"
        );
    }

    /// Slack 워크스페이스의 모든 필드(소켓 수신을 명시, 서명 비밀 이름, 채널)와 워크스페이스 둘을 갖춘 호스트의 지문 바이트가 바뀌지 않는다.
    /// 수신 방식 표지·서명 비밀을 다루는 코드를 고쳐도 이미 활성화된 호스트가 재활성화를 요구받지 않는다는 고정점이다.
    #[test]
    fn a_host_with_slack_workspaces_keeps_its_fingerprint_bytes() {
        let mut value = base();
        let workspaces = &mut value["slack"]["workspaces"];
        workspaces["home"]["receive"] = json!("socket");
        workspaces["home"]["signing_secret"] = json!("signing-secret");
        workspaces["team"] =
            json!({"team_id": "T2", "bot_token": "team-bot", "app_token": "team-app"});
        assert_eq!(
            hex(print(value)),
            "59b4cb137574eadc4047467f56bf2deadeb1a9e983d0ab13f3ec61aa2f7b9cf7"
        );
    }

    fn with_openai() -> Value {
        let mut value = base();
        value["http"]["endpoints"]["openai"] = json!({"kind": "openai", "keys": {
            "automation": {"secret": "automation-api-key", "subject": "build"},
            "rotated": {"secret": "automation-api-key-2", "subject": "build"}},
            "wait_ms": 120000, "max_active_runs": 32});
        value
    }

    #[test]
    fn openai_entrances_join_the_fingerprint_by_identity_not_by_value_or_limits() {
        let original = print(with_openai());
        assert_ne!(
            original,
            print(base()),
            "OpenAI 입구가 생기면 활성화 사건이다"
        );
        let changed_openai = |edit: &dyn Fn(&mut Value)| {
            let mut value = with_openai();
            edit(&mut value["http"]["endpoints"]["openai"]);
            print(value)
        };
        for (name, print) in [
            (
                "주체",
                changed_openai(&|e| e["keys"]["automation"]["subject"] = json!("ops")),
            ),
            (
                "자격 이름",
                changed_openai(&|e| e["keys"]["automation"]["secret"] = json!("other-key")),
            ),
            (
                "슬롯 이름",
                changed_openai(&|e| {
                    let keys = e["keys"].as_object_mut().unwrap();
                    let slot = keys.remove("rotated").unwrap();
                    keys.insert("next".into(), slot);
                }),
            ),
            (
                "슬롯 추가",
                changed_openai(&|e| {
                    e["keys"]["extra"] = json!({"secret": "extra-key", "subject": "build"});
                }),
            ),
            ("입구 별칭", {
                let mut value = with_openai();
                let endpoint = value["http"]["endpoints"]
                    .as_object_mut()
                    .unwrap()
                    .remove("openai")
                    .unwrap();
                value["http"]["endpoints"]["gpt"] = endpoint;
                print(value)
            }),
            (
                "슬롯 제거",
                changed_openai(&|e| {
                    e["keys"].as_object_mut().unwrap().remove("rotated");
                }),
            ),
        ] {
            assert_ne!(print, original, "{name}");
        }
        // 대기 시간·실행 수 한도·키 순서는 신원과 무관한 운영 값이다
        for (name, print) in [
            ("대기 시간", changed_openai(&|e| e["wait_ms"] = json!(5000))),
            (
                "실행 수 한도",
                changed_openai(&|e| e["max_active_runs"] = json!(8)),
            ),
        ] {
            assert_eq!(print, original, "{name}");
        }
    }

    #[test]
    fn openai_and_webhook_entrances_do_not_collide_in_the_fingerprint() {
        // 같은 별칭·같은 이름을 웹훅 입구로 쓰는 것과 OpenAI 입구로 쓰는 것은 다르다(구간 표지가 다르다)
        let mut webhook = base();
        webhook["http"]["endpoints"]["x"] = json!({"issuer": "i", "audience": "a", "key": "k"});
        let mut openai = base();
        openai["http"]["endpoints"]["x"] =
            json!({"kind": "openai", "keys": {"k": {"secret": "k", "subject": "i"}}});
        assert_ne!(print(webhook), print(openai));
    }

    #[test]
    fn identity_and_auth_mapping_changes_change_the_fingerprint() {
        let original = print(base());
        assert_eq!(original, print(base()), "결정적");
        for (name, print) in [
            (
                "실제 uid",
                changed(|v| v["cli"]["accounts"]["owner"]["uid"] = json!(502)),
            ),
            (
                "계정 추가",
                changed(|v| v["cli"]["accounts"]["extra"] = json!({"uid": 7})),
            ),
            (
                "팀",
                changed(|v| v["slack"]["workspaces"]["home"]["team_id"] = json!("T2")),
            ),
            (
                "앱",
                changed(|v| v["slack"]["workspaces"]["home"]["app_id"] = json!("A2")),
            ),
            (
                "채널 ID",
                changed(|v| v["slack"]["workspaces"]["home"]["channels"]["ops"] = json!("C9")),
            ),
            (
                "채널 별칭",
                changed(|v| {
                    let channels = v["slack"]["workspaces"]["home"]["channels"]
                        .as_object_mut()
                        .unwrap();
                    let id = channels.remove("ops").unwrap();
                    channels.insert("ops2".into(), id);
                }),
            ),
            (
                "DM",
                changed(|v| v["slack"]["workspaces"]["home"]["dm"] = json!(false)),
            ),
            ("수신 방식", {
                let mut value = base();
                let home = &mut value["slack"]["workspaces"]["home"];
                home["receive"] = json!("http");
                home["signing_secret"] = json!("signing");
                unvalidated_print(value)
            }),
            (
                "봇 토큰 이름",
                changed(|v| v["slack"]["workspaces"]["home"]["bot_token"] = json!("other")),
            ),
            (
                "발급자",
                changed(|v| v["http"]["endpoints"]["inbox"]["issuer"] = json!("j")),
            ),
            (
                "대상",
                changed(|v| v["http"]["endpoints"]["inbox"]["audience"] = json!("b")),
            ),
            (
                "키 이름",
                changed(|v| v["http"]["endpoints"]["inbox"]["key"] = json!("other-key")),
            ),
            (
                "토큰 수명",
                changed(|v| v["http"]["endpoints"]["inbox"]["max_token_age_secs"] = json!(60)),
            ),
            (
                "입구 추가",
                changed(|v| {
                    v["http"]["endpoints"]["extra"] =
                        json!({"issuer": "i", "audience": "a", "key": "k"});
                }),
            ),
        ] {
            assert_ne!(print, original, "{name}");
        }
    }

    #[test]
    fn operational_values_and_key_order_do_not_change_the_fingerprint() {
        let original = print(base());
        for (name, print) in [
            (
                "소켓",
                changed(|v| v["cli"]["socket"] = json!("/tmp/b.sock")),
            ),
            (
                "수신 주소",
                changed(|v| v["http"]["listen"] = json!("127.0.0.1:9100")),
            ),
            (
                "대기 시간",
                changed(|v| v["http"]["endpoints"]["inbox"]["sync_wait_ms"] = json!(900)),
            ),
            ("한도", changed(|v| v["limits"]["body_bytes"] = json!(2000))),
            (
                "관리 주소",
                changed(|v| v["admin_url"] = json!("http://127.0.0.1:9/ui")),
            ),
        ] {
            assert_eq!(print, original, "{name}");
        }
    }

    #[test]
    fn field_boundaries_cannot_be_shifted_to_collide() {
        // 별칭·팀 경계를 옮겨도 같은 지문이 되지 않는다(길이가 앞에 붙는다)
        let ws = |alias: &str, team: &str, extra: Value| {
            let mut entry = json!({"team_id": team, "bot_token": "t", "app_token": "x"});
            entry
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"slack": {"workspaces": {alias: entry}}})
        };
        let a = ws("ab", "C", json!({}));
        let b = ws("a", "BC", json!({}));
        assert_ne!(print(a), print(b));
        // 값이 없는 것과 있는 것은 다르다
        assert_ne!(
            print(ws("w", "T", json!({}))),
            print(ws("w", "T", json!({"app_id": "A1"})))
        );
    }

    #[test]
    fn an_unresolvable_os_user_is_an_error_not_a_silent_default() {
        let config =
            HostConfig::from_value(json!({"cli": {"accounts": {"owner": {"os_user": "x"}}}}))
                .unwrap();
        let result = fingerprint_with(&config, &|_| Err(HostError("해석할 수 없어요".into())));
        assert!(result.is_err());
    }
}

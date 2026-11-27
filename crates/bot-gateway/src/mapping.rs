//! Deterministic fingerprint of gateway identity, entrance, and auth-name mappings (excluding secret values).
//!
//! Bound into activation fingerprints so mapping changes require explicit activation.
//! Stability invariant: hosts without OpenAI endpoints omit the `openai` section so existing deployments keep
//! identical bytes; operational settings (limits, timeouts, listen/socket/admin addresses) are excluded.
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

/// Retained byte marker for Socket Mode reception to preserve activation fingerprint compatibility.
const RECEIVE_SOCKET: u8 = 0;

pub fn fingerprint(config: &HostConfig) -> Result<[u8; 32], HostError> {
    fingerprint_with(config, &resolve_uid)
}

/// Validates before hashing so unvalidated configs (e.g. `receive: "http"`) cannot collide with `RECEIVE_SOCKET`.
pub fn fingerprint_with(
    config: &HostConfig,
    uid_of: &dyn Fn(&CliAccount) -> Result<u32, HostError>,
) -> Result<[u8; 32], HostError> {
    config.validate()?;
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
        hash.update([RECEIVE_SOCKET]);
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

    fn print(value: Value) -> [u8; 32] {
        let config = HostConfig::from_value(value).unwrap();
        fingerprint_with(&config, &|account| Ok(account.uid.unwrap_or(502))).unwrap()
    }

    fn changed(edit: impl FnOnce(&mut Value)) -> [u8; 32] {
        let mut value = base();
        edit(&mut value);
        print(value)
    }

    fn hex(bytes: [u8; 32]) -> String {
        bot_core::random::hex(&bytes)
    }

    #[test]
    fn a_host_without_openai_entrances_keeps_its_fingerprint_bytes() {
        assert_eq!(
            hex(print(base())),
            "59bd29edd4e4f8b1a48c39bd3ddf43768c7076b5c8dbdb6fd319540881cd987d"
        );
    }

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
            "adding an OpenAI entrance must change the fingerprint"
        );
        let changed_openai = |edit: &dyn Fn(&mut Value)| {
            let mut value = with_openai();
            edit(&mut value["http"]["endpoints"]["openai"]);
            print(value)
        };
        for (name, print) in [
            (
                "subject",
                changed_openai(&|e| e["keys"]["automation"]["subject"] = json!("ops")),
            ),
            (
                "secret name",
                changed_openai(&|e| e["keys"]["automation"]["secret"] = json!("other-key")),
            ),
            (
                "slot name",
                changed_openai(&|e| {
                    let keys = e["keys"].as_object_mut().unwrap();
                    let slot = keys.remove("rotated").unwrap();
                    keys.insert("next".into(), slot);
                }),
            ),
            (
                "add slot",
                changed_openai(&|e| {
                    e["keys"]["extra"] = json!({"secret": "extra-key", "subject": "build"});
                }),
            ),
            ("endpoint alias", {
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
                "remove slot",
                changed_openai(&|e| {
                    e["keys"].as_object_mut().unwrap().remove("rotated");
                }),
            ),
        ] {
            assert_ne!(print, original, "{name}");
        }
        for (name, print) in [
            ("wait_ms", changed_openai(&|e| e["wait_ms"] = json!(5000))),
            (
                "max_active_runs",
                changed_openai(&|e| e["max_active_runs"] = json!(8)),
            ),
        ] {
            assert_eq!(print, original, "{name}");
        }
    }

    #[test]
    fn openai_and_webhook_entrances_do_not_collide_in_the_fingerprint() {
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
        assert_eq!(original, print(base()), "deterministic");
        for (name, print) in [
            (
                "uid",
                changed(|v| v["cli"]["accounts"]["owner"]["uid"] = json!(502)),
            ),
            (
                "add account",
                changed(|v| v["cli"]["accounts"]["extra"] = json!({"uid": 7})),
            ),
            (
                "team_id",
                changed(|v| v["slack"]["workspaces"]["home"]["team_id"] = json!("T2")),
            ),
            (
                "app_id",
                changed(|v| v["slack"]["workspaces"]["home"]["app_id"] = json!("A2")),
            ),
            (
                "channel id",
                changed(|v| v["slack"]["workspaces"]["home"]["channels"]["ops"] = json!("C9")),
            ),
            (
                "channel alias",
                changed(|v| {
                    let channels = v["slack"]["workspaces"]["home"]["channels"]
                        .as_object_mut()
                        .unwrap();
                    let id = channels.remove("ops").unwrap();
                    channels.insert("ops2".into(), id);
                }),
            ),
            (
                "dm",
                changed(|v| v["slack"]["workspaces"]["home"]["dm"] = json!(false)),
            ),
            (
                "signing_secret",
                changed(|v| v["slack"]["workspaces"]["home"]["signing_secret"] = json!("signing")),
            ),
            (
                "bot_token",
                changed(|v| v["slack"]["workspaces"]["home"]["bot_token"] = json!("other")),
            ),
            (
                "issuer",
                changed(|v| v["http"]["endpoints"]["inbox"]["issuer"] = json!("j")),
            ),
            (
                "audience",
                changed(|v| v["http"]["endpoints"]["inbox"]["audience"] = json!("b")),
            ),
            (
                "key name",
                changed(|v| v["http"]["endpoints"]["inbox"]["key"] = json!("other-key")),
            ),
            (
                "max_token_age_secs",
                changed(|v| v["http"]["endpoints"]["inbox"]["max_token_age_secs"] = json!(60)),
            ),
            (
                "add endpoint",
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
                "socket",
                changed(|v| v["cli"]["socket"] = json!("/tmp/b.sock")),
            ),
            (
                "listen",
                changed(|v| v["http"]["listen"] = json!("127.0.0.1:9100")),
            ),
            (
                "sync_wait_ms",
                changed(|v| v["http"]["endpoints"]["inbox"]["sync_wait_ms"] = json!(900)),
            ),
            (
                "body_bytes",
                changed(|v| v["limits"]["body_bytes"] = json!(2000)),
            ),
            (
                "admin_url",
                changed(|v| v["admin_url"] = json!("http://127.0.0.1:9/ui")),
            ),
        ] {
            assert_eq!(print, original, "{name}");
        }
    }

    #[test]
    fn field_boundaries_cannot_be_shifted_to_collide() {
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
        assert_ne!(
            print(ws("w", "T", json!({}))),
            print(ws("w", "T", json!({"app_id": "A1"})))
        );
    }

    #[test]
    fn a_config_that_validation_refuses_has_no_fingerprint() {
        let uid_of = |account: &CliAccount| Ok(account.uid.unwrap_or(502));
        let mut declared = base();
        declared["slack"]["workspaces"]["home"]["receive"] = json!("http");
        let unvalidated: HostConfig = serde_json::from_value(declared).unwrap();
        let error = fingerprint_with(&unvalidated, &uid_of).unwrap_err();
        assert!(
            error
                .0
                .starts_with("gateway.slack.workspaces.home.receive: "),
            "{error}"
        );
        let socket: HostConfig = serde_json::from_value(base()).unwrap();
        assert_eq!(fingerprint_with(&socket, &uid_of).unwrap(), print(base()));
    }

    #[test]
    fn an_unresolvable_os_user_is_an_error_not_a_silent_default() {
        let config =
            HostConfig::from_value(json!({"cli": {"accounts": {"owner": {"os_user": "x"}}}}))
                .unwrap();
        let result = fingerprint_with(&config, &|_| Err(HostError("cannot resolve".into())));
        assert!(result.is_err());
    }
}

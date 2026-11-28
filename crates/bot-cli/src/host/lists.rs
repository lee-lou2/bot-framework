//! Parsing helpers and authority-fingerprint binders for list-shaped host catalog sections.

use super::{HostFileError, PrivateEgressMap, bad};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn name_list(value: &Value, path: &str) -> Result<BTreeSet<String>, HostFileError> {
    let Value::Array(items) = value else {
        return Err(bad(format!("{path}: must be an array of strings")));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad(format!("{path}: must be an array of strings")))
        })
        .collect()
}

pub(super) fn profile_map(
    value: &Value,
    path: &str,
    check: fn(&str) -> bool,
) -> Result<BTreeMap<String, BTreeSet<String>>, HostFileError> {
    let Value::Object(map) = value else {
        return Err(bad(format!(
            "{path}: must be an object keyed by profile name"
        )));
    };
    map.iter()
        .map(|(profile, list)| {
            if !bot_core::id::is_name(profile) {
                return Err(bad(format!("{path}: invalid profile name")));
            }
            let items = name_list(list, &format!("{path}.{profile}"))?;
            if !items.iter().all(|item| check(item)) {
                return Err(bad(format!("{path}.{profile}: contains an invalid entry")));
            }
            Ok((profile.clone(), items))
        })
        .collect()
}

pub(super) fn private_egress_map(value: &Value) -> Result<PrivateEgressMap, HostFileError> {
    let Value::Object(profiles) = value else {
        return Err(bad(
            "private_egress: must be an object keyed by profile name",
        ));
    };
    profiles
        .iter()
        .map(|(profile, routes)| {
            if !bot_core::id::is_name(profile) {
                return Err(bad("private_egress: invalid profile name"));
            }
            let Value::Object(entries) = routes else {
                return Err(bad(format!(
                    "private_egress.{profile}: must be a `host:port -> ip:port` object"
                )));
            };
            let mapped = entries
                .iter()
                .map(|(from, to)| {
                    let hp = bot_broker::resolve::HostPort::parse(from).ok_or_else(|| {
                        bad(format!("private_egress.{profile}: invalid host: {from}"))
                    })?;
                    let addr_str = to.as_str().ok_or_else(|| {
                        bad(format!(
                            "private_egress.{profile}.{from}: must be an `ip:port` string"
                        ))
                    })?;
                    let addr: std::net::SocketAddr = addr_str.parse().map_err(|_| {
                        bad(format!(
                            "private_egress.{profile}.{from}: invalid `ip:port`: {addr_str}"
                        ))
                    })?;
                    Ok(((hp.host, hp.port), addr))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            Ok((profile.clone(), mapped))
        })
        .collect()
}

pub(super) fn is_egress(text: &str) -> bool {
    let Some((host, port)) = text.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        && (1..=5).contains(&port.len())
        && port.bytes().all(|b| b.is_ascii_digit())
}

pub(super) fn is_super_admin(text: &str) -> bool {
    bot_core::identity::identity_kind(text)
        .is_some_and(|kind| kind != bot_core::identity::IdentityKind::Service)
}

/// Only opt-in Claude and Codex CLI subscription relays are permitted; Google consumer OAuth is never allowed.
pub(super) fn is_subscription_vendor(text: &str) -> bool {
    matches!(text, "claude" | "codex")
}

/// Binds `private_egress` into the activation fingerprint only when non-empty to preserve existing bytes.
pub(super) fn bind_private_egress(base: [u8; 32], map: &PrivateEgressMap) -> [u8; 32] {
    if map.is_empty() {
        return base;
    }
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-private-egress-v1\0");
    hash.update(base);
    for (profile, routes) in map {
        hash.update(profile.as_bytes());
        hash.update(b"\0");
        for ((host, port), addr) in routes {
            hash.update(format!("{host}:{port}={addr}\0").as_bytes());
        }
    }
    hash.finalize().into()
}

/// Binds `super_admins` into the activation fingerprint only when non-empty to preserve existing bytes.
pub(super) fn bind_super_admins(
    base: [u8; 32],
    admins: &BTreeMap<String, BTreeSet<String>>,
) -> [u8; 32] {
    if admins.is_empty() {
        return base;
    }
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-super-admins-v1\0");
    hash.update(base);
    for (profile, list) in admins {
        hash.update(profile.as_bytes());
        hash.update(b"\0");
        for admin in list {
            hash.update(admin.as_bytes());
            hash.update(b"\0");
        }
    }
    hash.finalize().into()
}

/// Binds `allow_subscription_auth` into the activation fingerprint only when non-empty to preserve existing bytes.
pub(super) fn bind_subscription_auth(
    base: [u8; 32],
    allow: &BTreeMap<String, BTreeSet<String>>,
) -> [u8; 32] {
    if allow.values().all(BTreeSet::is_empty) {
        return base;
    }
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-subscription-auth-v1\0");
    hash.update(base);
    for (profile, vendors) in allow {
        if vendors.is_empty() {
            continue;
        }
        hash.update(profile.as_bytes());
        hash.update(b"\0");
        for vendor in vendors {
            hash.update(vendor.as_bytes());
            hash.update(b"\0");
        }
    }
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use std::collections::{BTreeMap, BTreeSet};

    fn hex(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn super_admins_parse_valid_non_service_principals_and_reject_service() {
        let ok = parse(
            r#"{
                "version": 1,
                "super_admins": {
                    "personal": ["local:owner", "slack:home/U1", "http:ops/alice"]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            ok.catalog.super_admins,
            BTreeMap::from([(
                "personal".to_owned(),
                ["local:owner", "slack:home/U1", "http:ops/alice"]
                    .into_iter()
                    .map(String::from)
                    .collect::<BTreeSet<_>>()
            )])
        );
        assert!(ok.catalog.allows_super_admin("personal", "local:owner"));
        assert!(!ok.catalog.allows_super_admin("personal", "local:guest"));

        for bad in [
            r#"{"version": 1, "super_admins": {"personal": ["service:cron"]}}"#,
            r#"{"version": 1, "super_admins": {"personal": ["not-a-principal"]}}"#,
            r#"{"version": 1, "super_admins": {"BadProfile": ["local:owner"]}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn authority_fingerprint_is_unchanged_without_super_admins_and_changes_when_set() {
        let base = parse(r#"{"version": 1}"#).unwrap();
        let empty = parse(r#"{"version": 1, "super_admins": {}}"#).unwrap();
        let with_admin =
            parse(r#"{"version": 1, "super_admins": {"personal": ["local:owner"]}}"#).unwrap();
        let with_other =
            parse(r#"{"version": 1, "super_admins": {"personal": ["local:alice"]}}"#).unwrap();

        let rev = BTreeMap::new();
        let fp_base = base.authority_fingerprint(&rev).unwrap();
        let fp_empty = empty.authority_fingerprint(&rev).unwrap();
        let fp_admin = with_admin.authority_fingerprint(&rev).unwrap();
        let fp_other = with_other.authority_fingerprint(&rev).unwrap();

        assert_eq!(
            fp_base, fp_empty,
            "empty super_admins must preserve existing fingerprint bytes"
        );
        assert_ne!(
            fp_base, fp_admin,
            "super_admins must bind into the activation fingerprint"
        );
        assert_ne!(fp_admin, fp_other);
        assert_eq!(
            hex(fp_base),
            hex(parse(r#"{"version": 1}"#)
                .unwrap()
                .authority_fingerprint(&rev)
                .unwrap()),
            "same input must produce a deterministic fingerprint"
        );
        // Fixed-input golden pins for private_egress and super_admins binders.
        let fixed = [5u8; 32];
        let priv_map = BTreeMap::from([(
            "personal".to_owned(),
            BTreeMap::from([(
                ("internal.lan".to_owned(), 443),
                "10.0.0.5:443".parse().unwrap(),
            )]),
        )]);
        assert_eq!(super::bind_private_egress(fixed, &BTreeMap::new()), fixed);
        assert_eq!(
            hex(super::bind_private_egress(fixed, &priv_map)),
            "79f9d5fba1080bd5761310777ed6956afcf9efdab9366be44d90be19ac2040e1"
        );
        let admins_map = BTreeMap::from([(
            "personal".to_owned(),
            BTreeSet::from(["local:owner".to_owned()]),
        )]);
        assert_eq!(super::bind_super_admins(fixed, &BTreeMap::new()), fixed);
        assert_eq!(
            hex(super::bind_super_admins(fixed, &admins_map)),
            "eadfe4b1b57504ef97b9e06a881990976c66da1fbacfe9d8cd572aea417963f1"
        );
        // allow_subscription_auth preserves bytes when empty, binds when set, and rejects Google/unknown vendors.
        let sub_base = parse(r#"{"version": 1}"#).unwrap();
        let sub_empty =
            parse(r#"{"version": 1, "allow_subscription_auth": {"personal": []}}"#).unwrap();
        let sub_on = parse(
            r#"{"version": 1, "allow_subscription_auth": {"personal": ["claude", "codex"]}}"#,
        )
        .unwrap();
        assert_eq!(
            sub_base.authority_fingerprint(&rev).unwrap(),
            sub_empty.authority_fingerprint(&rev).unwrap()
        );
        assert_ne!(
            sub_base.authority_fingerprint(&rev).unwrap(),
            sub_on.authority_fingerprint(&rev).unwrap()
        );
        for bad in [
            r#"{"version": 1, "allow_subscription_auth": {"personal": ["google"]}}"#,
            r#"{"version": 1, "allow_subscription_auth": {"personal": ["agy"]}}"#,
            r#"{"version": 1, "allow_subscription_auth": {"personal": ["gemini"]}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}

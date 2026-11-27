//! 호스트 등록부의 목록형 절(`models`·`egress`) 해석 도우미.
use super::{HostFileError, PrivateEgressMap, bad};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn name_list(value: &Value, path: &str) -> Result<BTreeSet<String>, HostFileError> {
    let Value::Array(items) = value else {
        return Err(bad(format!("{path}: 문자열 배열이어야 해요")));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad(format!("{path}: 문자열 배열이어야 해요")))
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
            "{path}: 프로필 이름을 키로 하는 객체여야 해요"
        )));
    };
    map.iter()
        .map(|(profile, list)| {
            if !bot_core::id::is_name(profile) {
                return Err(bad(format!("{path}: 프로필 이름이 올바르지 않아요")));
            }
            let items = name_list(list, &format!("{path}.{profile}"))?;
            if !items.iter().all(|item| check(item)) {
                return Err(bad(format!(
                    "{path}.{profile}: 올바르지 않은 항목이 있어요"
                )));
            }
            Ok((profile.clone(), items))
        })
        .collect()
}

/// `private_egress`: 프로필 → `{ "host:port": "ip:port" }` (사설/로컬 고정 목적지 매핑).
pub(super) fn private_egress_map(value: &Value) -> Result<PrivateEgressMap, HostFileError> {
    let Value::Object(profiles) = value else {
        return Err(bad("private_egress: 프로필 이름을 키로 하는 객체여야 해요"));
    };
    profiles
        .iter()
        .map(|(profile, routes)| {
            if !bot_core::id::is_name(profile) {
                return Err(bad("private_egress: 프로필 이름이 올바르지 않아요"));
            }
            let Value::Object(entries) = routes else {
                return Err(bad(format!(
                    "private_egress.{profile}: `host:port -> ip:port` 객체여야 해요"
                )));
            };
            let mapped = entries
                .iter()
                .map(|(from, to)| {
                    let hp = bot_broker::resolve::HostPort::parse(from).ok_or_else(|| {
                        bad(format!(
                            "private_egress.{profile}: 올바르지 않은 호스트예요: {from}"
                        ))
                    })?;
                    let addr_str = to.as_str().ok_or_else(|| {
                        bad(format!(
                            "private_egress.{profile}.{from}: `ip:port` 문자열이어야 해요"
                        ))
                    })?;
                    let addr: std::net::SocketAddr = addr_str.parse().map_err(|_| {
                        bad(format!(
                            "private_egress.{profile}.{from}: 올바르지 않은 `ip:port`예요: {addr_str}"
                        ))
                    })?;
                    Ok(((hp.host, hp.port), addr))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            Ok((profile.clone(), mapped))
        })
        .collect()
}

/// `host:port` (소문자·숫자·`.`·`-`, 포트 1~5자리)
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

/// 호스트 `super_admins` 항목: 유효한 비-서비스 주체(`local:*` · `slack:*/*` · `http:*/*`).
pub(super) fn is_super_admin(text: &str) -> bool {
    bot_core::identity::identity_kind(text)
        .is_some_and(|kind| kind != bot_core::identity::IdentityKind::Service)
}

/// 사설 고정 egress 매핑이 비어 있지 않을 때만 활성화 지문에 묶는다(없으면 기존 지문 바이트 그대로).
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

/// 호스트 `super_admins` 상한이 비어 있지 않을 때만 활성화 지문에 묶는다(없으면 기존 지문 바이트 그대로).
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
            "빈 super_admins는 기존 지문 바이트와 같아야 해요"
        );
        assert_ne!(
            fp_base, fp_admin,
            "super_admins 설정은 활성화 지문에 묶여야 해요"
        );
        assert_ne!(fp_admin, fp_other);
        assert_eq!(
            hex(fp_base),
            hex(parse(r#"{"version": 1}"#)
                .unwrap()
                .authority_fingerprint(&rev)
                .unwrap()),
            "같은 입력은 결정적 지문"
        );
    }
}

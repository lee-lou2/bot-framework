//! 호스트 등록부의 목록형 절(`models`·`egress`) 해석 도우미.
use super::{HostFileError, PrivateEgressMap, bad};
use serde_json::Value;
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

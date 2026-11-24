//! 호스트 설정(JSON)의 `admin` 절. 조립하는 쪽(bot-cli)이 `serde_json::Value`를 넘기면 엄격하게 읽어 타입으로 돌려준다.
//! 모르는 키·잘못된 타입·잘못된 값은 시작 전에 거절한다. 비밀 값은 담지 않는다(헤더 비밀은 SecretStore 이름만).

use crate::{
    AdminConfig,
    auth::hosts_for,
    check_bind,
    trace::{Backend, HeaderValue, Ids, ProfileTrace},
};
use bot_core::id::{ProfileId, SecretName, is_name};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, net::SocketAddr};

/// 주소를 정하지 않았을 때의 로컬 관리 화면 주소.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

#[derive(Clone, Debug, PartialEq)]
pub struct AdminSettings {
    pub bind: SocketAddr,
    /// 루프백이 아닌 주소를 열려면 운영자가 명시해야 한다.
    pub remote: bool,
    /// 프록시 뒤에서 쓰는 추가 `Host`(포트 포함).
    pub hosts: Vec<String>,
    /// 접속 출처별 실패 한도(기본 5회/60초).
    pub max_failures: usize,
    /// 프로필별 추적 설정. 없는 프로필은 내보내지 않는다.
    pub trace: BTreeMap<ProfileId, ProfileTrace>,
}

fn object<'a>(v: &'a Value, at: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object().ok_or_else(|| format!("{at}: 객체여야 한다"))
}

fn only(o: &Map<String, Value>, keys: &[&str], at: &str) -> Result<(), String> {
    match o.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(format!("{at}: 알 수 없는 키 `{k}`")),
        None => Ok(()),
    }
}

fn flag(o: &Map<String, Value>, key: &str, default: bool, at: &str) -> Result<bool, String> {
    match o.get(key) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{at}.{key}: true 또는 false여야 한다")),
    }
}

fn text<'a>(o: &'a Map<String, Value>, key: &str, at: &str) -> Result<Option<&'a str>, String> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(_) => Err(format!("{at}.{key}: 비어 있지 않은 글이어야 한다")),
    }
}

/// `이름:포트`(IPv6는 `[주소]:포트`). 글자는 `[A-Za-z0-9.-]`만이다.
fn host_ok(h: &str) -> bool {
    let (name, port) = match h.strip_prefix('[') {
        Some(v6) => match v6.split_once("]:") {
            Some((a, p)) => (a, p),
            None => return false,
        },
        None => match h.rsplit_once(':') {
            Some(x) => x,
            None => return false,
        },
    };
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b':'));
    name_ok && port.parse::<u16>().is_ok_and(|p| p != 0)
}

fn header(name: &str, v: &Value, at: &str) -> Result<HeaderValue, String> {
    match v {
        Value::String(s) => Ok(HeaderValue::Plain(s.clone())),
        Value::Object(o) => {
            only(o, &["secret"], &format!("{at}.headers.{name}"))?;
            let secret = text(o, "secret", &format!("{at}.headers.{name}"))?
                .ok_or_else(|| format!("{at}.headers.{name}: secret 이름이 필요하다"))?;
            SecretName::parse(secret)
                .map(HeaderValue::Secret)
                .ok_or_else(|| format!("{at}.headers.{name}: 잘못된 비밀 이름"))
        }
        _ => Err(format!(
            "{at}.headers.{name}: 글 또는 {{\"secret\": 이름}}이어야 한다"
        )),
    }
}

fn one_trace(profile: &ProfileId, v: &Value) -> Result<ProfileTrace, String> {
    let at = format!("admin.trace.{}", profile.0);
    let o = object(v, &at)?;
    only(
        o,
        &[
            "endpoint", "name", "headers", "link", "remote", "content", "ids", "sample",
        ],
        &at,
    )?;
    let endpoint = text(o, "endpoint", &at)?.ok_or_else(|| format!("{at}.endpoint: 필요하다"))?;
    let mut headers = Vec::new();
    if let Some(h) = o.get("headers") {
        for (name, value) in object(h, &format!("{at}.headers"))? {
            headers.push((name.clone(), header(name, value, &at)?));
        }
    }
    let ids = match text(o, "ids", &at)? {
        None | Some("hash") => Ids::Hash,
        Some("plain") => Ids::Plain,
        Some(_) => return Err(format!("{at}.ids: hash 또는 plain이어야 한다")),
    };
    let sample = match o.get("sample") {
        None => 1.0,
        Some(n) => n
            .as_f64()
            .ok_or_else(|| format!("{at}.sample: 0.0~1.0 숫자여야 한다"))?,
    };
    let trace = ProfileTrace {
        backend: Some(Backend {
            name: text(o, "name", &at)?.unwrap_or("default").to_owned(),
            endpoint: endpoint.to_owned(),
            headers,
            link: text(o, "link", &at)?.map(str::to_owned),
            remote: flag(o, "remote", false, &at)?,
        }),
        ids,
        content: flag(o, "content", false, &at)?,
        sample,
    };
    trace.check(profile)?;
    Ok(trace)
}

impl AdminSettings {
    /// `admin` 객체를 읽는다. 키를 모두 생략해도(`{}`) 로컬 기본값으로 동작한다.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let o = object(v, "admin")?;
        only(
            o,
            &["bind", "remote", "hosts", "max_failures", "trace"],
            "admin",
        )?;
        let bind: SocketAddr = text(o, "bind", "admin")?
            .unwrap_or(DEFAULT_BIND)
            .parse()
            .map_err(|_| "admin.bind: `주소:포트` 형식이어야 한다".to_owned())?;
        let remote = flag(o, "remote", false, "admin")?;
        check_bind(bind, remote).map_err(|e| format!("admin.bind: {e}"))?;
        let mut hosts = Vec::new();
        if let Some(list) = o.get("hosts") {
            let items = list.as_array().ok_or("admin.hosts: 글 목록이어야 한다")?;
            for item in items {
                let h = item.as_str().map(str::to_ascii_lowercase);
                hosts.push(
                    h.filter(|h| host_ok(h))
                        .ok_or("admin.hosts: `이름:포트` 형식이어야 한다")?,
                );
            }
        }
        let max_failures = match o.get("max_failures") {
            None => 5,
            Some(n) => n
                .as_u64()
                .filter(|n| (1..=100).contains(n))
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("admin.max_failures: 1~100 정수여야 한다")?,
        };
        let mut trace = BTreeMap::new();
        if let Some(t) = o.get("trace").filter(|t| !t.is_null()) {
            for (name, cfg) in object(t, "admin.trace")? {
                if !is_name(name) {
                    return Err(format!(
                        "admin.trace: 프로필 이름 `{name}`이(가) 올바르지 않다"
                    ));
                }
                let profile = ProfileId(name.clone());
                if !cfg.is_null() {
                    let parsed = one_trace(&profile, cfg)?;
                    trace.insert(profile, parsed);
                }
            }
        }
        Ok(Self {
            bind,
            remote,
            hosts,
            max_failures,
            trace,
        })
    }

    /// 실제로 연 주소(포트 0 지원)로 `AdminConfig`를 만든다. 허용 `Host`는 그 주소와 루프백 이름, 설정의 추가 값이다.
    pub fn config(&self, actual: SocketAddr) -> AdminConfig {
        let mut cfg = AdminConfig::new(actual);
        cfg.hosts = hosts_for(actual, &self.hosts);
        cfg.max_failures = self.max_failures;
        cfg
    }
}

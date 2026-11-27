//! Strict parser for the `admin` section of host config JSON, rejecting unknown keys and invalid values before startup.

use crate::{
    AdminConfig,
    auth::hosts_for,
    check_bind,
    issues::{IssueConfig, IssueKind},
    logs::{LogConfig, LogLevel},
    obs::{HostHeaderValue, SecretRef},
    trace::{Backend, HeaderValue, Ids, ProfileTrace},
};
use bot_core::id::{ProfileId, SecretName, is_name};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, net::SocketAddr};

pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

#[derive(Clone, Debug, PartialEq)]
pub struct AdminSettings {
    pub bind: SocketAddr,
    pub remote: bool,
    pub hosts: Vec<String>,
    pub max_failures: usize,
    pub trace: BTreeMap<ProfileId, ProfileTrace>,
    pub logs: Option<LogConfig>,
    pub issues: Option<IssueConfig>,
}

fn object<'a>(v: &'a Value, at: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| format!("{at}: must be an object"))
}

fn only(o: &Map<String, Value>, keys: &[&str], at: &str) -> Result<(), String> {
    match o.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(format!("{at}: unknown key `{k}`")),
        None => Ok(()),
    }
}

fn flag(o: &Map<String, Value>, key: &str, default: bool, at: &str) -> Result<bool, String> {
    match o.get(key) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{at}.{key}: must be true or false")),
    }
}

fn text<'a>(o: &'a Map<String, Value>, key: &str, at: &str) -> Result<Option<&'a str>, String> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(_) => Err(format!("{at}.{key}: must be a non-empty string")),
    }
}

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
                .ok_or_else(|| format!("{at}.headers.{name}: secret name is required"))?;
            SecretName::parse(secret)
                .map(HeaderValue::Secret)
                .ok_or_else(|| format!("{at}.headers.{name}: invalid secret name"))
        }
        _ => Err(format!(
            "{at}.headers.{name}: must be a string or {{\"secret\": name}}"
        )),
    }
}

fn secret_ref(v: &Value, at: &str) -> Result<SecretRef, String> {
    let o = object(v, at)?;
    only(o, &["secret", "profile"], at)?;
    let secret = text(o, "secret", at)?.ok_or_else(|| format!("{at}: secret name is required"))?;
    let name = SecretName::parse(secret).ok_or_else(|| format!("{at}: invalid secret name"))?;
    match text(o, "profile", at)? {
        None => Ok(SecretRef::host(name)),
        Some(p) if is_name(p) => Ok(SecretRef::in_profile(ProfileId(p.to_owned()), name)),
        Some(p) => Err(format!("{at}.profile: invalid profile name `{p}`")),
    }
}

fn host_header(name: &str, v: &Value, at: &str) -> Result<HostHeaderValue, String> {
    let where_at = format!("{at}.headers.{name}");
    match v {
        Value::String(s) => Ok(HostHeaderValue::Plain(s.clone())),
        Value::Object(_) => secret_ref(v, &where_at).map(HostHeaderValue::Secret),
        _ => Err(format!(
            "{where_at}: must be a string or {{\"secret\": name}}"
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
    let endpoint = text(o, "endpoint", &at)?.ok_or_else(|| format!("{at}.endpoint: required"))?;
    let mut headers = Vec::new();
    if let Some(h) = o.get("headers") {
        for (name, value) in object(h, &format!("{at}.headers"))? {
            headers.push((name.clone(), header(name, value, &at)?));
        }
    }
    let ids = match text(o, "ids", &at)? {
        None | Some("hash") => Ids::Hash,
        Some("plain") => Ids::Plain,
        Some(_) => return Err(format!("{at}.ids: must be hash or plain")),
    };
    let sample = match o.get("sample") {
        None => 1.0,
        Some(n) => n
            .as_f64()
            .ok_or_else(|| format!("{at}.sample: must be a number in 0.0..=1.0"))?,
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

fn parse_logs(v: &Value) -> Result<LogConfig, String> {
    let at = "admin.logs";
    let o = object(v, at)?;
    only(o, &["name", "endpoint", "headers", "level", "remote"], at)?;
    let endpoint = text(o, "endpoint", at)?.ok_or_else(|| format!("{at}.endpoint: required"))?;
    let mut headers = Vec::new();
    if let Some(h) = o.get("headers") {
        for (name, value) in object(h, &format!("{at}.headers"))? {
            headers.push((name.clone(), host_header(name, value, at)?));
        }
    }
    let level = match text(o, "level", at)? {
        None => LogLevel::Warn,
        Some(raw) => LogLevel::parse(raw)
            .ok_or_else(|| format!("{at}.level: must be one of debug, info, warn, error"))?,
    };
    let cfg = LogConfig {
        name: text(o, "name", at)?.unwrap_or("otlp-logs").to_owned(),
        endpoint: endpoint.to_owned(),
        headers,
        level,
        remote: flag(o, "remote", false, at)?,
    };
    cfg.check()?;
    Ok(cfg)
}

fn parse_issues(v: &Value) -> Result<IssueConfig, String> {
    let at = "admin.issues";
    let o = object(v, at)?;
    only(
        o,
        &[
            "kind",
            "name",
            "endpoint",
            "key",
            "environment",
            "release",
            "sample",
            "remote",
        ],
        at,
    )?;
    let kind = match text(o, "kind", at)? {
        None | Some("sentry") => IssueKind::Sentry,
        Some(_) => return Err(format!("{at}.kind: only sentry is supported")),
    };
    let endpoint = text(o, "endpoint", at)?.ok_or_else(|| format!("{at}.endpoint: required"))?;
    let key = match o.get("key").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(secret_ref(v, &format!("{at}.key"))?),
    };
    let sample = match o.get("sample") {
        None => 1.0,
        Some(n) => n
            .as_f64()
            .ok_or_else(|| format!("{at}.sample: must be a number in 0.0..=1.0"))?,
    };
    let cfg = IssueConfig {
        kind,
        name: text(o, "name", at)?.unwrap_or(kind.as_str()).to_owned(),
        endpoint: endpoint.to_owned(),
        key,
        environment: text(o, "environment", at)?.map(str::to_owned),
        release: text(o, "release", at)?.map(str::to_owned),
        sample,
        remote: flag(o, "remote", false, at)?,
    };
    cfg.check()?;
    Ok(cfg)
}

impl AdminSettings {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let o = object(v, "admin")?;
        only(
            o,
            &[
                "bind",
                "remote",
                "hosts",
                "max_failures",
                "trace",
                "logs",
                "issues",
            ],
            "admin",
        )?;
        let bind: SocketAddr = text(o, "bind", "admin")?
            .unwrap_or(DEFAULT_BIND)
            .parse()
            .map_err(|_| "admin.bind: must be in `host:port` format".to_owned())?;
        let remote = flag(o, "remote", false, "admin")?;
        check_bind(bind, remote).map_err(|e| format!("admin.bind: {e}"))?;
        let mut hosts = Vec::new();
        if let Some(list) = o.get("hosts") {
            let items = list
                .as_array()
                .ok_or("admin.hosts: must be an array of strings")?;
            for item in items {
                let h = item.as_str().map(str::to_ascii_lowercase);
                hosts.push(
                    h.filter(|h| host_ok(h))
                        .ok_or("admin.hosts: must be in `host:port` format")?,
                );
            }
        }
        let max_failures = match o.get("max_failures") {
            None => 5,
            Some(n) => n
                .as_u64()
                .filter(|n| (1..=100).contains(n))
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("admin.max_failures: must be an integer in 1..=100")?,
        };
        let mut trace = BTreeMap::new();
        if let Some(t) = o.get("trace").filter(|t| !t.is_null()) {
            for (name, cfg) in object(t, "admin.trace")? {
                if !is_name(name) {
                    return Err(format!("admin.trace: invalid profile name `{name}`"));
                }
                let profile = ProfileId(name.clone());
                if !cfg.is_null() {
                    let parsed = one_trace(&profile, cfg)?;
                    trace.insert(profile, parsed);
                }
            }
        }
        let logs = match o.get("logs").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => Some(parse_logs(v)?),
        };
        let issues = match o.get("issues").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => Some(parse_issues(v)?),
        };
        Ok(Self {
            bind,
            remote,
            hosts,
            max_failures,
            trace,
            logs,
            issues,
        })
    }

    pub fn config(&self, actual: SocketAddr) -> AdminConfig {
        let mut cfg = AdminConfig::new(actual);
        cfg.hosts = hosts_for(actual, &self.hosts);
        cfg.max_failures = self.max_failures;
        cfg
    }
}

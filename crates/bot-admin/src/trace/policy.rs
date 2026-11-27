//! Trace export boundary policy enforcing attribute allowlists, identity pseudonymization, private-span suppression, and redaction.

use super::{Key, SpanRecord, Val, key, otlp::Out};
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ids {
    Plain,
    Hash,
}

pub const MASK: &str = "[redacted]";
const MAX_ATTRS: usize = 64;
const MAX_EVENTS: usize = 16;
const MAX_LINKS: usize = 8;
const TEXT_LIMIT: usize = 256;
const CONTENT_LIMIT: usize = 8 * 1024;

/// Shared live secret redactor. Clones share one longest-first list so rotated secrets added at runtime
/// take effect immediately across exporters and response leak checks and are never unlearned.
#[derive(Clone, Default)]
pub struct Redactor {
    values: Arc<RwLock<Arc<Vec<String>>>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("values", &self.snapshot().len())
            .finish()
    }
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '=')
}

/// Detects known secret prefixes and JWTs; `strict` also flags 40+ char mixed tokens in free text while allowing hex metadata hashes.
fn shaped(w: &str, strict: bool) -> bool {
    if w.len() >= 16 && bot_core::redact::has_secret_prefix(w) {
        return true;
    }
    if bot_core::redact::looks_like_jwt(w) {
        return true;
    }
    strict
        && w.len() >= 40
        && w.bytes().any(|c| c.is_ascii_digit())
        && w.bytes().any(|c| c.is_ascii_alphabetic())
        && w.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=' | b'-' | b'_'))
}

fn ordered(mut values: Vec<String>) -> Vec<String> {
    values.retain(|v| !v.is_empty());
    values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    values.dedup();
    values
}

impl Redactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let redactor = Self::default();
        redactor.add_all(values);
        redactor
    }

    fn snapshot(&self) -> Arc<Vec<String>> {
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn add(&self, value: &str) {
        if value.is_empty() || self.snapshot().iter().any(|known| known == value) {
            return;
        }
        self.add_all([value.to_owned()]);
    }

    pub fn add_all(&self, values: impl IntoIterator<Item = String>) {
        let mut guard = self.values.write().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<String> = guard.as_ref().clone();
        all.extend(values);
        *guard = Arc::new(ordered(all));
    }

    fn scan(&self, text: &str, strict: bool) -> bool {
        let probe = text.replace(MASK, "\u{0}");
        self.snapshot().iter().any(|v| probe.contains(v.as_str()))
            || probe
                .split(|c: char| !word_char(c))
                .any(|w| shaped(w, strict))
    }

    pub fn leaks(&self, text: &str) -> bool {
        self.scan(text, true)
    }

    pub fn leaks_meta(&self, text: &str) -> bool {
        self.scan(text, false)
    }

    pub fn has_value(&self, text: &str) -> bool {
        self.snapshot().iter().any(|v| text.contains(v.as_str()))
    }

    pub fn mask(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for v in self.snapshot().iter() {
            out = out.replace(v.as_str(), MASK);
        }
        let (mut done, mut word) = (String::with_capacity(out.len()), String::new());
        let flush = |done: &mut String, word: &mut String| {
            done.push_str(if shaped(word, true) {
                MASK
            } else {
                word.as_str()
            });
            word.clear();
        };
        for c in out.chars() {
            if word_char(c) {
                word.push(c);
            } else {
                flush(&mut done, &mut word);
                done.push(c);
            }
        }
        flush(&mut done, &mut word);
        done
    }
}

/// Per-span export policy; `private` suppresses content even when profile `content` export is enabled.
#[derive(Clone, Copy, Debug)]
pub(super) struct Policy {
    pub ids: Ids,
    pub content: bool,
    pub private: bool,
}

use bot_core::trace::key::BOT_PRIVATE as PRIVATE;

fn pseudonym(profile: &str, value: &str) -> String {
    let d = Sha256::digest(format!("bot-id\0{profile}\0{value}"));
    d[..6].iter().fold(String::new(), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

fn cap(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_owned()
    } else {
        s.chars().take(limit).collect()
    }
}

fn attr(
    p: &str,
    policy: &Policy,
    red: &Redactor,
    k: Key,
    v: Val,
) -> Result<(Key, Val), &'static str> {
    if key::CONTENT.contains(&k) {
        return match (policy.content && !policy.private, v) {
            (false, _) => Err(if policy.private { "private" } else { "off" }),
            (true, Val::Str(s)) => {
                let masked = red.mask(&s);
                if red.leaks(&masked) {
                    Err("secret")
                } else {
                    Ok((k, Val::Str(cap(&masked, CONTENT_LIMIT))))
                }
            }
            (true, _) => Err("off"),
        };
    }
    if !key::ALLOWED.contains(&k) {
        return Err("key");
    }
    match v {
        Val::Str(s) if key::IDENTITY.contains(&k) && policy.ids == Ids::Hash => {
            Ok((k, Val::Str(pseudonym(p, &s))))
        }
        Val::Str(s) if red.leaks_meta(&s) => Err("secret"),
        Val::Str(s) => Ok((k, Val::Str(cap(&s, TEXT_LIMIT)))),
        other => Ok((k, other)),
    }
}

fn attrs(
    p: &str,
    policy: &Policy,
    red: &Redactor,
    list: Vec<(Key, Val)>,
    dropped: &mut u32,
    why: &mut Option<&'static str>,
) -> Vec<(Key, Val)> {
    let mut out = Vec::new();
    for (k, v) in list {
        if k == key::BOT_PROFILE {
            continue;
        }
        if out.len() >= MAX_ATTRS {
            *dropped += 1;
            continue;
        }
        match attr(p, policy, red, k, v) {
            Ok(kv) => out.push(kv),
            Err(reason) => {
                *dropped += 1;
                if reason != "key" {
                    why.get_or_insert(reason);
                }
            }
        }
    }
    out
}

fn name(red: &Redactor, s: &str) -> String {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '/' | ':'));
    if ok && !red.leaks_meta(s) {
        s.to_owned()
    } else {
        MASK.to_owned()
    }
}

pub(super) fn apply(rec: SpanRecord, policy: &Policy, red: &Redactor) -> Out {
    let p = rec.profile.0.clone();
    let (mut dropped, mut why) = (0u32, None);
    let private = rec
        .attrs
        .iter()
        .any(|(k, v)| *k == PRIVATE && *v == Val::Bool(true));
    let policy = &Policy { private, ..*policy };
    let marked: Vec<(Key, Val)> = rec
        .attrs
        .into_iter()
        .filter(|(k, _)| *k != PRIVATE)
        .collect();
    let mut list = attrs(&p, policy, red, marked, &mut dropped, &mut why);
    list.push((key::BOT_PROFILE, Val::Str(p.clone())));
    let mut events = Vec::new();
    for e in rec.events {
        if events.len() >= MAX_EVENTS {
            dropped += 1;
            continue;
        }
        let at = attrs(&p, policy, red, e.attrs, &mut dropped, &mut why);
        events.push((name(red, e.name), e.at_nano, at));
    }
    dropped += u32::try_from(rec.links.len().saturating_sub(MAX_LINKS)).unwrap_or(u32::MAX);
    let links = rec
        .links
        .into_iter()
        .take(MAX_LINKS)
        .map(|l| (l.trace, l.span))
        .collect();
    if dropped > 0 {
        list.push((key::BOT_DROPPED, Val::Int(i64::from(dropped))));
    }
    if let Some(w) = why {
        list.push((key::BOT_CONTENT_DROPPED, Val::Str(w.to_owned())));
    }
    let error = rec
        .error
        .filter(|e| {
            !e.is_empty()
                && e.len() <= 64
                && e.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-')
                })
        })
        .map(str::to_owned);
    Out {
        ids: rec.ids,
        name: name(red, &rec.name),
        kind: rec.kind,
        start: rec.start_nano,
        end: rec.end_nano,
        attrs: list,
        events,
        links,
        error,
        dropped,
    }
}

#[cfg(test)]
mod tests;

//! 내보내기 경계의 정책: 허용 어휘 밖의 키·내용·비밀을 걸러 내고 식별자를 가명으로 바꾼다.
//! 호출하는 쪽이 무엇을 담았든 이 경계에서 한 번 더 건다(내용 기본 꺼짐, 승인 nonce·인자 키는 어휘에 없음).

use super::{Key, SpanRecord, Val, key, otlp::Out};
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};

/// 식별자 표기: `Plain`은 그대로, `Hash`는 프로필별 가명(같은 값은 같은 가명).
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

/// 비밀 값을 가리는 장치. 알려진 값은 길이와 무관하게 가리고, 토큰 모양도 가린다.
///
/// 복제본은 **같은 목록을 공유한다**: 한 곳에서 `add`한 값은 내보내기·관리 응답 검사가 곧바로 본다. 호스트는 비밀 보관소에서 읽어 쓰는 값을 쓰기 전에 여기에 더해
/// (키를 교체해도 새 값이 바로 가려진다) 시작 때 읽은 목록이 낡아도 새는 일이 없게 한다. 더한 값은 이 장치가 사는 동안 남는다(교체 전 값이 어딘가 남아 있어도 가린다).
#[derive(Clone, Default)]
pub struct Redactor {
    /// 비어 있지 않은 값, 긴 것부터(겹치는 값에서 긴 쪽을 먼저 가린다). 바꿀 때는 새 목록을 통째로 갈아 끼운다(읽는 쪽은 한 목록만 본다).
    values: Arc<RwLock<Arc<Vec<String>>>>,
}

/// 진단 출력에는 등록한 값을 싣지 않고 개수만 보인다.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("values", &self.snapshot().len())
            .finish()
    }
}

const PREFIXES: [&str; 17] = [
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xapp-",
    "sk-",
    "sk_live_",
    "pk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
];

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '=')
}

/// 토큰처럼 생긴 낱말인가: 알려진 접두사, JWT 모양. `strict`면 길고 무작위처럼 보이는 문자열도 포함한다
/// (자유 글인 내용에만 쓴다: 메타데이터의 판본·해시 같은 긴 16진 값을 비밀로 오인하지 않으려는 구분이다).
fn shaped(w: &str, strict: bool) -> bool {
    if w.len() >= 16 && PREFIXES.iter().any(|p| w.starts_with(p)) {
        return true;
    }
    if w.starts_with("eyJ") && w.len() >= 30 && w.matches('.').count() >= 2 {
        return true;
    }
    strict
        && w.len() >= 40
        && w.bytes().any(|c| c.is_ascii_digit())
        && w.bytes().any(|c| c.is_ascii_alphabetic())
        && w.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=' | b'-' | b'_'))
}

/// 비어 있지 않은 값을 긴 것부터 정렬하고 겹치는 것을 지운다.
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

    /// 지금 쓰는 목록(읽는 동안 바뀌지 않는 한 장).
    fn snapshot(&self) -> Arc<Vec<String>> {
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 값 하나를 더한다(복제본 모두에 곧바로 보인다). 비었거나 이미 있으면 아무것도 하지 않는다.
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

    /// 자유 글(내용): 비밀 값이나 토큰 모양(긴 무작위 문자열 포함)이 마스크 밖에 남아 있는가.
    pub fn leaks(&self, text: &str) -> bool {
        self.scan(text, true)
    }

    /// 메타데이터 값: 비밀 값이나 접두사·JWT 모양만 본다(긴 16진 판본·해시는 통과).
    pub fn leaks_meta(&self, text: &str) -> bool {
        self.scan(text, false)
    }

    /// 알려진 비밀 값이 들어 있는가(모양은 보지 않는다). 응답 검사처럼 해시가 많은 곳에 쓴다.
    pub fn has_value(&self, text: &str) -> bool {
        self.snapshot().iter().any(|v| text.contains(v.as_str()))
    }

    /// 알려진 값과 토큰 모양을 가린 글. 일반 문구는 그대로 남는다.
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

/// 프로필 정책: 식별자 표기와 내용 기록 여부. `private`는 스팬 단위 표지(민감 DM 경로)로, 켜면 프로필이 내용을 켰어도 내용은 나가지 않는다.
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

/// 속성 하나를 내보낼 값으로 바꾼다. 빼야 하면 이유(`off`·`secret`·`key`)를 돌려준다.
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

/// 이름류(스팬·이벤트 이름)는 고정 어휘만: 비밀이 섞였거나 모양이 어긋나면 가린다.
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

/// 끝난 스팬 하나를 내보낼 모양으로 바꾼다. 프로필은 레코드의 프로필로 고정한다.
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

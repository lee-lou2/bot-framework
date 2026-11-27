//! System One memory decision client (`d1.2-ultra`, `d1.2`, `jev-latest`) with activation fencing, secret filtering, and leak scanning.
use crate::model::api::{ApiClient, ApiEndpoint, scan::Needles};
use bot_core::{
    active::ActiveCheck,
    id::{MemoryId, ProfileId, SecretName},
    memory::{
        MAX_MEMORY_TEXT_BYTES, MemoryCandidate, MemoryDecision, MemoryDecisionItem, MemoryJudge,
        MemoryKind,
    },
    port::{Error, Reply, SecretStore},
    redact::secret_spans,
};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};

const MIN_KEY_BYTES: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CANDIDATES: usize = 8;
const MAX_SPANS: usize = 4;
const MAX_FALLBACK_BYTES: usize = 512;
const SAVE_MIN_PROB: f64 = 0.70;
const SUPERSEDE_MIN_CONF: f64 = 0.75;
const FORGET_MIN_PROB: f64 = 0.80;
const SECRET_BLOCK_PROB: f64 = 0.50;
const SPAN_MIN_FOUND: f64 = 0.65;

const EXTRA_CREDENTIAL_PREFIXES: &[&str] = &["liquid_", "apikey_", "AQ.", "-----BEGIN "];

#[derive(Clone, Debug)]
pub struct MemoryJudgeSettings {
    pub endpoint: ApiEndpoint,
    pub model: String,
    /// Profile secret name; `None` is permitted only for loopback IP endpoints.
    pub secret: Option<SecretName>,
}

pub struct MemoryJudgeClient {
    settings: MemoryJudgeSettings,
    client: ApiClient,
    secrets: Arc<dyn SecretStore>,
    gate: Arc<dyn ActiveCheck>,
}

impl MemoryJudgeClient {
    pub fn new(
        settings: MemoryJudgeSettings,
        secrets: Arc<dyn SecretStore>,
        gate: Arc<dyn ActiveCheck>,
    ) -> Result<Self, String> {
        if settings.secret.is_none() && !settings.endpoint.is_loopback() {
            return Err(
                "memory_judge.secret is required unless endpoint is a loopback IP address"
                    .to_owned(),
            );
        }
        if settings.model.is_empty() || settings.model.len() > 128 {
            return Err("memory_judge.model must be 1..=128 bytes".to_owned());
        }
        let client = ApiClient::new(settings.endpoint.is_loopback())?;
        Ok(Self {
            settings,
            client,
            secrets,
            gate,
        })
    }

    fn endpoint_url(&self) -> String {
        if self.settings.endpoint.path().ends_with("/systemone") {
            self.settings.endpoint.url("")
        } else {
            self.settings.endpoint.url("/v1/systemone")
        }
    }

    async fn resolve_key(&self, profile: &ProfileId) -> Result<Option<String>, Error> {
        let Some(secret_name) = &self.settings.secret else {
            return Ok(None);
        };
        let handle = self.secrets.for_profile(profile.clone()).await?;
        let raw = handle.resolve(secret_name.clone()).await?;
        let key = String::from_utf8(raw.0).map_err(|_| Error::Unavailable)?;
        let key = key.trim().to_owned();
        if key.len() < MIN_KEY_BYTES || key.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(Error::Unavailable);
        }
        Ok(Some(key))
    }

    async fn post_decision(
        &self,
        profile: &ProfileId,
        payload: &Value,
    ) -> Result<(u16, String), Error> {
        // Double active-revision guard: before reading the secret and immediately before sending.
        self.gate.verify().await?;
        let key = self.resolve_key(profile).await?;
        self.gate.verify().await?;

        let mut headers = vec![("content-type", "application/json".to_owned(), false)];
        let needles = key.as_deref().map(Needles::new);
        if let Some(token) = &key {
            headers.push(("authorization", format!("Bearer {token}"), true));
        }
        let url = self.endpoint_url();
        let answer = self
            .client
            .post(
                &url,
                headers,
                payload.to_string().into_bytes(),
                REQUEST_TIMEOUT,
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        let raw = String::from_utf8(answer.body).map_err(|_| Error::Unavailable)?;
        if needles.as_ref().is_some_and(|scan| scan.in_body(&raw)) {
            tracing::warn!(
                code = "secret_in_response",
                "discarded memory_judge response containing key"
            );
            return Err(Error::Unavailable);
        }
        Ok((answer.status, raw))
    }

    async fn judge_inner(
        &self,
        profile: &ProfileId,
        user_message: &str,
        existing: &[MemoryCandidate],
    ) -> Result<MemoryDecision, Error> {
        let trimmed = user_message.trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_MEMORY_TEXT_BYTES
            || contains_credential_like(trimmed)
        {
            return Ok(MemoryDecision::None);
        }
        let candidates: Vec<&MemoryCandidate> = existing.iter().take(MAX_CANDIDATES).collect();
        let req_with_spans = build_request(&self.settings.model, trimmed, &candidates, true);
        let (status, raw) = self.post_decision(profile, &req_with_spans).await?;
        let raw = if status == 422 {
            // Fall back to core System One primitives (noul + choice) for endpoints without `spans` (e.g. TypeSafe Jev).
            let fallback = build_request(&self.settings.model, trimmed, &candidates, false);
            let (fb_status, fb_raw) = self.post_decision(profile, &fallback).await?;
            if !(200..300).contains(&fb_status) {
                tracing::warn!(status = fb_status, "memory_judge request rejected");
                return Err(Error::Unavailable);
            }
            fb_raw
        } else if (200..300).contains(&status) {
            raw
        } else {
            tracing::warn!(status, "memory_judge request rejected");
            return Err(Error::Unavailable);
        };
        parse_decision(&raw, trimmed, &candidates)
    }
}

impl MemoryJudge for MemoryJudgeClient {
    fn model(&self) -> &str {
        &self.settings.model
    }

    fn judge<'a>(
        &'a self,
        profile: &'a ProfileId,
        user_message: &'a str,
        existing: &'a [MemoryCandidate],
    ) -> Reply<'a, MemoryDecision> {
        Box::pin(self.judge_inner(profile, user_message, existing))
    }
}

pub(crate) fn contains_credential_like(text: &str) -> bool {
    if !secret_spans(text).is_empty() {
        return true;
    }
    EXTRA_CREDENTIAL_PREFIXES.iter().any(|prefix| {
        let mut from = 0;
        while let Some(offset) = text[from..].find(prefix) {
            let begin = from + offset;
            let boundary = begin == 0 || !text.as_bytes()[begin - 1].is_ascii_alphanumeric();
            let rest = &text[begin + prefix.len()..];
            if boundary
                && (*prefix == "-----BEGIN "
                    || rest
                        .bytes()
                        .take_while(|b| {
                            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
                        })
                        .count()
                        >= 12)
            {
                return true;
            }
            from = begin + prefix.len();
        }
        false
    })
}

fn build_request(
    model: &str,
    user_message: &str,
    candidates: &[&MemoryCandidate],
    include_spans: bool,
) -> Value {
    let has_candidates = !candidates.is_empty();
    let state = if has_candidates {
        let list: Vec<Value> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| json!({ "slot": format!("m{i}"), "text": c.text }))
            .collect();
        Value::String(
            json!({ "user_message": user_message, "existing_memories": list }).to_string(),
        )
    } else {
        Value::String(user_message.to_owned())
    };

    let mut action_criteria = Map::new();
    action_criteria.insert(
        "save".into(),
        Value::String(
            "States a new or updated durable personal preference, standing rule, identity detail, schedule/plan, or important fact worth remembering across future conversations"
                .into(),
        ),
    );
    if has_candidates {
        action_criteria.insert(
            "forget".into(),
            Value::String(
                "Explicitly asks to forget, delete, or stop remembering one of the existing_memories"
                    .into(),
            ),
        );
    }
    action_criteria.insert(
        "none".into(),
        Value::String(
            "Greeting, one-off question, generic help request, transient small talk, already-stored duplicate, or secret/credential"
                .into(),
        ),
    );

    let mut questions = Map::new();
    questions.insert(
        "action".into(),
        json!({
            "type": "choice",
            "instructions": if has_candidates {
                "Based on `user_message` alone, should the assistant save a new/updated long-term memory, forget an existing memory, or do nothing?"
            } else {
                "Does the user's message state a durable personal preference, standing rule, schedule/plan, or important fact worth saving to long-term memory?"
            },
            "criteria": Value::Object(action_criteria),
        }),
    );
    questions.insert(
        "contains_secret".into(),
        json!({
            "type": "noul",
            "instructions": "Does the user's message contain an API key, password, access token, private key, or other sensitive credential?"
        }),
    );
    questions.insert(
        "kind".into(),
        json!({
            "type": "choice",
            "instructions": "If saved to long-term memory, what kind of memory is this?",
            "criteria": {
                "core": "Standing personal preference, identity, communication/style rule, or long-term habit",
                "fact": "Specific event, schedule, location, project detail, or concrete fact"
            }
        }),
    );
    if has_candidates {
        let mut target_criteria = Map::new();
        target_criteria.insert(
            "none".into(),
            Value::String(
                "Does not contradict, replace, or ask to forget any entry in existing_memories"
                    .into(),
            ),
        );
        for (i, c) in candidates.iter().enumerate() {
            target_criteria.insert(
                format!("m{i}"),
                Value::String(format!("Existing memory: {}", c.text)),
            );
        }
        questions.insert(
            "target_memory".into(),
            json!({
                "type": "choice",
                "instructions": "Which entry in `existing_memories` is directly superseded/updated or requested to be forgotten by `user_message`? Choose `none` for unrelated topics.",
                "criteria": Value::Object(target_criteria),
            }),
        );
    }
    if include_spans {
        questions.insert(
            "memorable_spans".into(),
            json!({
                "type": "spans",
                "max": MAX_SPANS,
                "instructions": if has_candidates {
                    "Extract each concise self-contained fact, preference, rule, or schedule ONLY from `user_message` (never from `existing_memories`), omitting greetings, filler, and unrelated questions."
                } else {
                    "Extract each concise self-contained fact, preference, rule, or schedule from the user's message, omitting greetings, filler, and unrelated questions."
                }
            }),
        );
    }
    json!({
        "model": model,
        "state": state,
        "questions": Value::Object(questions),
    })
}

fn choice_prob(entry: &Value, choice: &str) -> f64 {
    entry
        .get("probabilities")
        .and_then(|p| p.get(choice))
        .and_then(Value::as_f64)
        .or_else(|| entry.get("confidence").and_then(Value::as_f64))
        .unwrap_or(0.0)
}

fn resolve_slot(slot: &str, candidates: &[&MemoryCandidate]) -> Option<MemoryId> {
    let idx: usize = slot.strip_prefix('m')?.parse().ok()?;
    candidates.get(idx).map(|c| c.id.clone())
}

/// Splits text into complete sentences preserving trailing punctuation so Korean/English predicates stay intact.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (idx, ch) in text.char_indices() {
        if matches!(ch, '\n' | '.' | '!' | '?') {
            let end = idx + ch.len_utf8();
            let piece = text[start..end].trim();
            if !piece.is_empty() {
                out.push(piece);
            }
            start = end;
        }
    }
    if start < text.len() {
        let tail = text[start..].trim();
        if !tail.is_empty() {
            out.push(tail);
        }
    }
    out
}

/// Expands extracted spans to their containing sentences from `user_message`, stripping unrelated noise sentences.
fn extract_sentences(user_message: &str, spans_answer: Option<&Value>) -> Vec<String> {
    let sentences = split_sentences(user_message);
    let mut selected = Vec::new();
    if let Some(spans) = spans_answer
        .and_then(|v| v.get("spans"))
        .and_then(Value::as_array)
    {
        for span in spans {
            let found = span.get("found").and_then(Value::as_f64).unwrap_or(0.0);
            let Some(text) = span.get("text").and_then(Value::as_str).map(str::trim) else {
                continue;
            };
            if found < SPAN_MIN_FOUND || text.is_empty() {
                continue;
            }
            let matched_sentence = sentences.iter().find(|s| s.contains(text)).copied();
            let piece = matched_sentence.unwrap_or(text);
            if !selected.iter().any(|existing: &String| existing == piece) {
                selected.push(piece.to_owned());
            }
            if selected.len() >= MAX_SPANS {
                break;
            }
        }
    }
    if selected.is_empty() && user_message.len() <= MAX_FALLBACK_BYTES {
        selected.push(user_message.to_owned());
    }
    selected
        .into_iter()
        .filter(|s| {
            !s.is_empty() && s.len() <= MAX_MEMORY_TEXT_BYTES && !contains_credential_like(s)
        })
        .collect()
}

fn parse_decision(
    raw: &str,
    user_message: &str,
    candidates: &[&MemoryCandidate],
) -> Result<MemoryDecision, Error> {
    let parsed: Value = serde_json::from_str(raw).map_err(|_| Error::Unavailable)?;
    let answers = parsed
        .get("answers")
        .and_then(Value::as_object)
        .ok_or(Error::Unavailable)?;

    let secret_prob = answers
        .get("contains_secret")
        .and_then(|v| v.get("noul"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if secret_prob >= SECRET_BLOCK_PROB {
        return Ok(MemoryDecision::None);
    }

    let action_entry = answers.get("action").ok_or(Error::Unavailable)?;
    let action = action_entry
        .get("choice")
        .and_then(Value::as_str)
        .ok_or(Error::Unavailable)?;
    let action_prob = choice_prob(action_entry, action);

    let target = answers.get("target_memory").and_then(|entry| {
        let slot = entry.get("choice").and_then(Value::as_str)?;
        if slot == "none" {
            return None;
        }
        let prob = choice_prob(entry, slot);
        resolve_slot(slot, candidates).map(|id| (id, prob))
    });

    match action {
        "save" if action_prob >= SAVE_MIN_PROB => {
            let kind = answers
                .get("kind")
                .and_then(|v| v.get("choice"))
                .and_then(Value::as_str)
                .and_then(MemoryKind::parse)
                .unwrap_or(MemoryKind::Fact);
            let texts = extract_sentences(user_message, answers.get("memorable_spans"));
            if texts.is_empty() {
                return Ok(MemoryDecision::None);
            }
            let supersedes = target
                .filter(|(_, prob)| *prob >= SUPERSEDE_MIN_CONF)
                .map(|(id, _)| id);
            let items = texts
                .into_iter()
                .map(|text| MemoryDecisionItem { text, kind })
                .collect();
            Ok(MemoryDecision::Save { items, supersedes })
        }
        "forget" if action_prob >= FORGET_MIN_PROB => match target {
            Some((id, prob)) if prob >= FORGET_MIN_PROB => Ok(MemoryDecision::Forget(id)),
            _ => Ok(MemoryDecision::None),
        },
        _ => Ok(MemoryDecision::None),
    }
}

#[cfg(test)]
mod tests;

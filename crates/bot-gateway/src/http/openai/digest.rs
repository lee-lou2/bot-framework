//! Semantic SHA-256 request digest used for OpenAI idempotency-key equivalence.
use bot_core::api::ChatBody;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Bump whenever canonical digest inputs change so new digests never collide with old ones.
const VERSION: u32 = 1;

/// Excludes transport/ignored fields (`stream`, `temperature`, etc.) so streaming retries match the same run.
pub fn semantic(model: &str, body: &ChatBody) -> [u8; 32] {
    let messages: Vec<Value> = body
        .turns
        .iter()
        .map(|turn| {
            let mut message = json!({"role": turn.role.label(), "content": turn.content});
            if let Some(name) = &turn.name {
                message["name"] = Value::String(name.clone());
            }
            message
        })
        .collect();
    let canonical = json!({
        "version": VERSION,
        "model": model,
        "messages": messages,
        "options": {},
    });
    Sha256::digest(canonical.to_string().as_bytes()).into()
}

#[cfg(test)]
mod tests;

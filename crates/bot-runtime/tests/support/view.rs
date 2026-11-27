//! Inspection helpers for CLI and API tests: upstream request bodies, temp workspaces, step keys, and refusal markers.
#![allow(dead_code)]

use super::*;
use bot_core::id::*;
use serde_json::Value;
use std::path::PathBuf;

pub fn workspace(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bot-cli-{}-{n}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn bodies(upstream: &bot_broker::testing::FakeUpstream) -> Vec<Value> {
    upstream
        .requests()
        .into_iter()
        .map(|seen| serde_json::from_str(&seen.3).expect("request body must be JSON"))
        .collect()
}

pub fn result_text(body: &Value, tool_use_id: &str) -> Option<String> {
    for message in body["messages"].as_array()? {
        let Some(blocks) = message["content"].as_array() else {
            continue;
        };
        for block in blocks {
            if block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id {
                return Some(match &block["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    other => other.to_string(),
                });
            }
        }
    }
    None
}

pub async fn step_keys(w: &World, run: &RunId) -> Vec<String> {
    sqlx::query_scalar::<_, String>(&format!(
        "select key from bot_steps where run = '{}' order by key",
        run.0
    ))
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

pub fn result_error(body: &Value, tool_use_id: &str) -> Option<bool> {
    body["messages"]
        .as_array()?
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == tool_use_id)
        .map(|b| b["is_error"].as_bool().unwrap_or(false))
}

pub async fn marks(w: &World, run: &RunId) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(&format!(
        "select key, body from bot_steps where run = '{}' and key like 'cli-refusal-%' order by key",
        run.0
    ))
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

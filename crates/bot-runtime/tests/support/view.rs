//! 실제 CLI 시험의 공통 도우미: 상류가 받은 요청 본문 읽기, 작업 폴더, 단계 키, 확정 거절 표식.
#![allow(dead_code)]

use super::*;
use bot_core::id::*;
use serde_json::Value;
use std::path::PathBuf;

/// 시험마다 자기 작업 폴더를 받는다. 같은 `tag`를 쓰는 시험이 같은 프로세스에서 동시에 돌아도 서로의 폴더를 지우지 않도록 번호를 붙인다.
pub fn workspace(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bot-cli-{}-{n}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 상류가 받은 요청 본문들.
pub fn bodies(upstream: &bot_broker::testing::FakeUpstream) -> Vec<Value> {
    upstream
        .requests()
        .into_iter()
        .map(|seen| serde_json::from_str(&seen.3).expect("요청 본문은 JSON"))
        .collect()
}

/// 요청 본문에서 `tool_use_id`의 `tool_result` 글(블록 글을 이어 붙인 것). 없으면 None.
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

/// 요청 본문에서 `tool_use_id`의 `tool_result`가 오류 표시(`is_error`)인가. 없으면 None.
pub fn result_error(body: &Value, tool_use_id: &str) -> Option<bool> {
    body["messages"]
        .as_array()?
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == tool_use_id)
        .map(|b| b["is_error"].as_bool().unwrap_or(false))
}

/// 이 run의 확정 거절 표식(`cli-refusal-<호출 ID>`)들: (키, 본문).
pub async fn marks(w: &World, run: &RunId) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(&format!(
        "select key, body from bot_steps where run = '{}' and key like 'cli-refusal-%' order by key",
        run.0
    ))
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

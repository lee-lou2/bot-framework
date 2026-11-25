//! MCP 클라이언트. 여기는 stdio(JSON-RPC 2.0, 한 줄에 메시지 하나)이고, 원격 서버는 Streamable HTTP(`http`)로 말한다.
//!
//! 한 호출에 한 세션이다: `initialize`(협상한 판본 확인) → `tools/list`(선언과 대조할 입력 스키마) → `tools/call`.
//! 서버가 보낸 요청은 지원하지 않는다고 답하고(우리는 어떤 클라이언트 기능도 내세우지 않는다), 알림은 무시한다.
//! 서버가 준 글은 믿지 않는다: 줄·결과 크기를 제한하고 형식이 맞는 결과만 받아들인다.

use crate::{
    canon::canonicalize,
    json::parse,
    lines::read_line,
    sandbox::{BoxRead, BoxWrite, Pipe},
    text::json_quote,
};
use serde_json::{Map, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    time::timeout,
};

pub mod header;
pub mod http;
pub mod remote;
pub mod sse;
pub mod wire;

/// 지원하는 MCP 판본. 서버가 이 밖의 판본으로 답하면 호출하지 않는다.
pub const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// 서버가 보내는 줄 하나의 상한.
pub const MAX_FRAME: usize = 1 << 20;
const MAX_SKIPPED: usize = 256;
const MAX_PAGES: usize = 16;

#[derive(Debug, Eq, PartialEq)]
pub enum McpError {
    Io,
    Timeout,
    Closed,
    Protocol,
    Version,
    /// 서버가 JSON-RPC 오류로 답했다(코드만 취한다).
    Server(i64),
    /// (HTTP) 연결을 맺기 전에 실패했다: 요청이 서버에 닿지 않았다.
    Unreached,
    /// (HTTP) 2xx가 아닌 응답. `code`는 본문이 같은 요청의 JSON-RPC 오류일 때의 코드다(분류에는 쓰지 않는다: 실행 여부를 알려 주지 않는다).
    Status {
        status: u16,
        code: Option<i64>,
    },
    /// (HTTP) 서버 응답 어딘가(원문·해석한 글·키·숫자 표기·커서)에 이쪽 자격이 섞여 있다. 응답을 쓰지 않는다.
    Exposed,
    /// (HTTP) 도구 목록을 모으는 동안 서버가 목록이 바뀌었다고 알렸다: 모은 목록은 한 시점의 것이 아니다.
    Changed,
    /// (HTTP) 요청을 보내기 직전 확인에서 이 프로세스의 판본이 더는 활성이 아니다: 아무것도 보내지 않았다.
    Stale,
}

pub struct Session {
    read: BufReader<BoxRead>,
    write: BoxWrite,
    next: u64,
}

impl Session {
    pub fn new(pipe: Pipe) -> Self {
        Self {
            read: BufReader::new(pipe.read),
            write: pipe.write,
            next: 1,
        }
    }

    async fn send_frame(&mut self, mut text: String) -> Result<(), McpError> {
        text.push('\n');
        self.write
            .write_all(text.as_bytes())
            .await
            .map_err(|_| McpError::Io)?;
        self.write.flush().await.map_err(|_| McpError::Io)
    }

    /// 요청을 보내고 요청 번호를 돌려준다. `params`는 JSON 객체 글이다.
    async fn send(&mut self, method: &str, params: &str) -> Result<u64, McpError> {
        let id = self.next;
        self.next += 1;
        let frame = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":{},\"params\":{params}}}",
            json_quote(method)
        );
        self.send_frame(frame).await?;
        Ok(id)
    }

    async fn frame(&mut self) -> Result<Value, McpError> {
        let line = match read_line(&mut self.read, MAX_FRAME).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(McpError::Closed),
            Err(_) => return Err(McpError::Io),
        };
        let text = std::str::from_utf8(&line).map_err(|_| McpError::Protocol)?;
        parse(text).ok_or(McpError::Protocol)
    }

    /// 서버가 보낸 요청에는 오류로 답한다.
    async fn reject(&mut self, id: &Value) -> Result<(), McpError> {
        let id = serde_json::to_string(id).map_err(|_| McpError::Protocol)?;
        let frame = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"method not supported\"}}}}"
        );
        self.send_frame(frame).await
    }

    async fn wait(&mut self, id: u64) -> Result<Value, McpError> {
        for _ in 0..MAX_SKIPPED {
            let frame = self.frame().await?;
            let obj = frame.as_object().ok_or(McpError::Protocol)?;
            if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(McpError::Protocol);
            }
            if obj.get("method").and_then(Value::as_str).is_some() {
                if let Some(request_id) = obj.get("id") {
                    self.reject(request_id).await?;
                }
                continue;
            }
            if obj.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = obj.get("error") {
                return Err(McpError::Server(
                    error.get("code").and_then(Value::as_i64).unwrap_or(0),
                ));
            }
            return match obj.get("result") {
                Some(result @ Value::Object(_)) => Ok(result.clone()),
                _ => Err(McpError::Protocol),
            };
        }
        Err(McpError::Protocol)
    }

    /// 보낸 요청의 응답을 기다린다.
    pub async fn await_result(&mut self, id: u64, limit: Duration) -> Result<Value, McpError> {
        timeout(limit, self.wait(id))
            .await
            .map_err(|_| McpError::Timeout)?
    }

    async fn request(
        &mut self,
        method: &str,
        params: &str,
        limit: Duration,
    ) -> Result<Value, McpError> {
        let id = self.send(method, params).await?;
        self.await_result(id, limit).await
    }

    /// 판본을 협상하고 `initialized`를 알린다.
    pub async fn initialize(&mut self, limit: Duration) -> Result<(), McpError> {
        let params = format!(
            "{{\"protocolVersion\":{},\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"bot-broker\",\"version\":\"0.1.0\"}}}}",
            json_quote(SUPPORTED[0])
        );
        let result = self.request("initialize", &params, limit).await?;
        match result.get("protocolVersion").and_then(Value::as_str) {
            Some(version) if SUPPORTED.contains(&version) => {}
            _ => return Err(McpError::Version),
        }
        self.send_frame("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".into())
            .await
    }

    /// 서버가 내세우는 도구 `name`의 입력 스키마. 없으면 `None`, 같은 이름이 둘이면 모호하므로 오류다.
    pub async fn tool_schema(
        &mut self,
        name: &str,
        limit: Duration,
    ) -> Result<Option<Value>, McpError> {
        let found = self.find_tool(name, limit).await?;
        match found {
            None => Ok(None),
            Some(tool) => Ok(Some(
                tool.get("inputSchema").cloned().ok_or(McpError::Protocol)?,
            )),
        }
    }

    /// 서버가 내세우는 도구 `name`의 정의 한 건(`name`·`description`·`inputSchema` …). 없으면 `None`, 같은 이름이 둘이면 모호하므로 오류다.
    pub async fn find_tool(
        &mut self,
        name: &str,
        limit: Duration,
    ) -> Result<Option<Value>, McpError> {
        let mut found: Option<Value> = None;
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => format!("{{\"cursor\":{}}}", json_quote(cursor)),
                None => "{}".to_owned(),
            };
            let result = self.request("tools/list", &params, limit).await?;
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or(McpError::Protocol)?;
            for tool in tools {
                if tool.get("name").and_then(Value::as_str) == Some(name) {
                    if found.is_some() {
                        return Err(McpError::Protocol);
                    }
                    found = Some(tool.clone());
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => return Ok(found),
            }
        }
        Err(McpError::Protocol)
    }

    /// `tools/call`을 보낸다. `arguments`는 정규화한 인자 글 그대로, `meta`는 `_meta` 객체 글이다.
    /// 보내다 실패하면 도착했는지 알 수 없다.
    pub async fn send_call(
        &mut self,
        name: &str,
        arguments: &str,
        meta: &str,
    ) -> Result<u64, McpError> {
        let params = format!(
            "{{\"name\":{},\"arguments\":{arguments},\"_meta\":{meta}}}",
            json_quote(name)
        );
        self.send("tools/call", &params).await
    }
}

/// 도구가 결과 `_meta`에 후속 확인을 요청하는 키.
pub const WAKE_KEY: &str = "bot/wake";
/// 직접 호출한 도구가 «이 이벤트를 처리하지 않았다»고 알리는 키(F3). 일반 Slack 직접 트리거의 성공 결과에서만 해석한다(`service::pass`).
pub const PASS_KEY: &str = "bot/pass";
/// 넘김 표식과 함께 `_meta`에 다른 항목이 왔다는 표지(F3). 다른 항목의 내용은 검사·보관하지 않고 버리되, 표식이 «다른 지시와 함께 온 것»임을 `payload`에 남겨
/// 브로커가 `invalid_pass`로 닫게 한다(표식 말고 남는 것이 있으면 유효하지 않다). 표식이 없는 결과에는 싣지 않는다.
pub const EXTRA_KEY: &str = "bot/extra";

/// 검증을 마친 도구 결과.
#[derive(Debug, Eq, PartialEq)]
pub struct Output {
    pub is_error: bool,
    pub texts: Vec<String>,
    pub structured: Option<Value>,
    /// 결과 최상위 `_meta["bot/wake"]`(도구가 요청하는 한 번짜리 후속 확인). 다른 `_meta` 항목은 버린다. 브로커가 선언·권한을
    /// 확인한 뒤 모델에게 가기 전에 제거하고 예약으로 옮긴다.
    pub wake: Option<Value>,
    /// 결과 최상위 `_meta["bot/pass"]`(값은 그대로: 불리언 `true`인지는 브로커가 호출 맥락과 함께 판정한다). 모델에게 가기 전에 제거한다.
    pub pass: Option<Value>,
    /// `_meta`에 후속 확인(`bot/wake`)·넘김(`bot/pass`) 말고 다른 항목이 있었다(내용은 버린다). 넘김 표식이 있을 때만 `payload`에 표지로 남는다.
    pub meta_extra: bool,
}

/// `{content:[{type:"text",text}], isError?, structuredContent?}`만 받는다.
/// 이미지·오디오·리소스 같은 다른 형식과 모양이 틀린 결과는 거절한다(`result_shape`).
pub fn parse_result(value: &Value) -> Result<Output, &'static str> {
    const SHAPE: &str = "result_shape";
    let obj = value.as_object().ok_or(SHAPE)?;
    let is_error = match obj.get("isError") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(SHAPE),
    };
    let mut texts = Vec::new();
    for item in obj.get("content").and_then(Value::as_array).ok_or(SHAPE)? {
        let item = item.as_object().ok_or(SHAPE)?;
        if item.get("type").and_then(Value::as_str) != Some("text") {
            return Err(SHAPE);
        }
        texts.push(
            item.get("text")
                .and_then(Value::as_str)
                .ok_or(SHAPE)?
                .to_owned(),
        );
    }
    Ok(Output {
        is_error,
        texts,
        structured: obj.get("structuredContent").cloned(),
        wake: obj
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get(WAKE_KEY))
            .cloned(),
        pass: obj
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get(PASS_KEY))
            .cloned(),
        meta_extra: obj
            .get("_meta")
            .and_then(Value::as_object)
            .is_some_and(|meta| meta.keys().any(|key| key != WAKE_KEY && key != PASS_KEY)),
    })
}

/// 모델에게 돌려줄 고정 형식 결과. 알 수 없는 필드는 버린다. 정규화할 수 없는 수가 있으면 `None`.
pub fn payload(output: &Output) -> Option<String> {
    let mut map = Map::new();
    map.insert(
        "content".into(),
        Value::Array(
            output
                .texts
                .iter()
                .map(|text| {
                    let mut item = Map::new();
                    item.insert("type".into(), "text".into());
                    item.insert("text".into(), text.as_str().into());
                    Value::Object(item)
                })
                .collect(),
        ),
    );
    map.insert("isError".into(), output.is_error.into());
    if let Some(structured) = &output.structured {
        map.insert("structuredContent".into(), structured.clone());
    }
    if output.wake.is_some() || output.pass.is_some() {
        let mut meta = Map::new();
        if let Some(wake) = &output.wake {
            meta.insert(WAKE_KEY.into(), wake.clone());
        }
        if let Some(pass) = &output.pass {
            meta.insert(PASS_KEY.into(), pass.clone());
            // 표식과 다른 `_meta` 항목이 함께 왔다: 내용은 버리지만 «표식만 온 것이 아니다»는 남겨 `invalid_pass`로 닫게 한다
            if output.meta_extra {
                meta.insert(EXTRA_KEY.into(), Value::Bool(true));
            }
        }
        map.insert("_meta".into(), Value::Object(meta));
    }
    canonicalize(&Value::Object(map))
}

/// 서버가 JSON-RPC 오류로 답했을 때 모델에게 돌려줄 고정 결과. 서버가 준 문장은 싣지 않는다.
pub fn error_payload() -> String {
    "{\"content\":[{\"text\":\"tool_error\",\"type\":\"text\"}],\"isError\":true}".to_owned()
}

#[cfg(test)]
mod tests;

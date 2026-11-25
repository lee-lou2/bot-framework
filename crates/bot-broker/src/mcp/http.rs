//! MCP Streamable HTTP 클라이언트. 한 호출에 한 세션이다(stdio `Session`과 같은 모양): 판본을 가린 뒤 `tools/list`로 도구 정의를 받고 `tools/call`을 한 번 보낸다.
//!
//! 판본은 현행 `2026-07-28`(요청마다 `_meta`로 판본·기능을 싣고 세션·핸드셰이크가 없다)과 구판 `2025-11-25`·`2025-06-18`·`2025-03-26`(`initialize` →
//! `notifications/initialized`, `Mcp-Session-Id`; 도구 부분은 같다)만 말한다. 먼저 **읽기 전용 `server/discover`**로 현행 서버인지 가린다. 공식 HTTP 호환 절차
//! (`400`·`404`·`405`이고 본문이 현행의 JSON-RPC 오류가 아님)에서 구판으로 확인될 때만 `initialize`로 간다(우리가 내세우는 판본은 구판 중 최신이고, 서버가 답한
//! 구판을 쓴다). `MCP-Protocol-Version` 머리는 `2025-06-18` 이후 명세의 요구라 `2025-03-26`에는 싣지 않는다. 쓰기일 수 있는 `tools/call`로 판본을 알아보거나
//! 다시 보내지 않는다(실패한 호출은 그대로 돌려준다).
//!
//! 이 클라이언트는 기능을 내세우지 않는다(`clientCapabilities: {}`): 서버의 요청(샘플링·인출·루트)은 지원하지 않는다. 현행 서버가 스트림에서 요청을 보내거나
//! `input_required` 결과를 주면 프로토콜 위반이고, 구판 서버의 `ping`에는 빈 결과로, 그 밖의 요청에는 `method not supported` 오류로 답한다. GET 스트림은 열지 않는다.
mod message;

use super::{
    MAX_FRAME, MAX_PAGES, MAX_SKIPPED, McpError,
    remote::Remote,
    sse::Events,
    wire::{RemoteWire, WireFail, WireMethod, WireRequest, WireResponse, header_value},
};
use crate::{json::parse, model::ChunkSource, text::json_quote};
use message::{
    Answer, Message, error_message, finish, holds, holds_bytes, listed, media_type, read_all,
    response_message, session_ok,
};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::timeout;

/// 현행 판본.
pub const MODERN: &str = "2026-07-28";
/// 구판 중 최신(`initialize` 핸드셰이크와 세션 ID를 쓴다). `initialize`에 내세우는 판본이다.
pub const LEGACY: &str = "2025-11-25";
/// 받아들이는 구판들(최신부터). 서버는 우리가 내세운 판본이나 자기가 지원하는 다른 판본으로 답한다.
pub const LEGACY_VERSIONS: [&str; 3] = [LEGACY, "2025-06-18", "2025-03-26"];
/// `MCP-Protocol-Version` 머리를 요구하지 않는 구판(그 머리는 `2025-06-18` 명세에서 생겼다).
const NO_VERSION_HEADER: &str = "2025-03-26";

const CLIENT_INFO: &str = r#"{"name":"bot-broker","version":"0.1.0"}"#;
/// 현행 판본의 요청마다 싣는 `_meta`(필수: 판본·기능. 우리는 기능을 내세우지 않는다)
const MODERN_META: &str = r#""io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"bot-broker","version":"0.1.0"},"io.modelcontextprotocol/protocolVersion":"2026-07-28""#;
/// SSE 응답 하나의 총 크기 한도.
const MAX_STREAM: usize = 8 << 20;
const AUX_LIMIT: Duration = Duration::from_secs(5);
/// 한 목록에 모으는 도구 수의 상한(쪽마다 1 MiB 한도가 있어도 합이 불어나지 않게).
const MAX_LISTED: usize = 4096;
/// 서버가 준 커서 하나의 길이 상한(커서는 다음 요청 본문에 그대로 실린다).
const MAX_CURSOR: usize = 4096;
/// 도구 목록이 바뀌었다고 알리는 알림.
const LIST_CHANGED: &str = "notifications/tools/list_changed";

#[derive(Debug)]
enum Era {
    /// 아직 가리지 않았다(`server/discover`를 보내는 중)
    Probe,
    Modern,
    Legacy {
        session: Option<String>,
        /// 서버와 정한 구판(`LEGACY_VERSIONS` 중 하나)
        version: &'static str,
    },
}

pub struct Http {
    wire: Arc<dyn RemoteWire>,
    remote: Remote,
    /// `Authorization: Bearer` 값(요청 머리에만 들어간다)
    bearer: Option<String>,
    allow_private: bool,
    era: Era,
    next: u64,
    /// 도구 목록을 모으는 동안 서버가 `list_changed`를 알렸는가
    changed: AtomicBool,
}

/// 서버가 말하는 판본들에 받아들이는 구판이 있는가.
fn offers_legacy(versions: &[&str]) -> bool {
    versions.iter().any(|v| LEGACY_VERSIONS.contains(v))
}

impl Http {
    pub fn new(
        wire: Arc<dyn RemoteWire>,
        remote: Remote,
        bearer: Option<String>,
        allow_private: bool,
    ) -> Self {
        Self {
            wire,
            remote,
            bearer,
            allow_private,
            era: Era::Probe,
            next: 1,
            changed: AtomicBool::new(false),
        }
    }

    /// 응답에서 읽은 원문에 이쪽 자격이 들어 있으면 쓰지 않는다. 자격은 요청의 인증 머리에만 있어야 하고, 서버가 되풀이한 것은 도구 정의·커서·결과 어디로도 가지 않는다.
    fn guard(&self, raw: &[u8]) -> Result<(), McpError> {
        if holds_bytes(raw, self.bearer.as_deref()) {
            Err(McpError::Exposed)
        } else {
            Ok(())
        }
    }

    /// 가린 판본(시험·진단용).
    pub fn version(&self) -> Option<&'static str> {
        match self.era {
            Era::Probe => None,
            Era::Modern => Some(MODERN),
            Era::Legacy { version, .. } => Some(version),
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    fn plain_headers(&self) -> Vec<(String, String, bool)> {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into(), false),
            (
                "Accept".into(),
                "application/json, text/event-stream".into(),
                false,
            ),
            ("User-Agent".into(), "bot-broker/0.1".into(), false),
        ];
        if let Some(token) = &self.bearer {
            headers.push(("Authorization".into(), format!("Bearer {token}"), true));
        }
        headers
    }

    /// 현행 판본의 요청 머리: 판본·메서드(·도구 이름)와 스키마가 지정한 인자 머리.
    fn modern_headers(
        &self,
        method: &str,
        name: Option<&str>,
        params: Vec<(String, String)>,
    ) -> Vec<(String, String, bool)> {
        let mut headers = self.plain_headers();
        headers.push(("MCP-Protocol-Version".into(), MODERN.into(), false));
        headers.push(("Mcp-Method".into(), method.into(), false));
        if let Some(name) = name {
            headers.push(("Mcp-Name".into(), super::header::encode_value(name), false));
        }
        headers.extend(params.into_iter().map(|(n, v)| (n, v, false)));
        headers
    }

    /// 구판 핸드셰이크를 마친 뒤의 요청 머리: 판본과 세션 ID.
    fn legacy_headers(&self) -> Vec<(String, String, bool)> {
        let mut headers = self.plain_headers();
        if let Era::Legacy { session, version } = &self.era {
            if *version != NO_VERSION_HEADER {
                headers.push(("MCP-Protocol-Version".into(), (*version).into(), false));
            }
            if let Some(session) = session {
                headers.push(("Mcp-Session-Id".into(), session.clone(), false));
            }
        }
        headers
    }

    /// 요청 본문. `fields`는 `params`의 멤버들(`"name":…`), `meta`는 우리가 더 싣는 `_meta` 멤버들이다(둘 다 중괄호 없는 JSON 조각).
    /// 현행 판본은 필수 `_meta`(판본·기능)를 함께 싣는다.
    fn body(&self, id: u64, method: &str, fields: &str, meta: &str) -> String {
        let mut entries: Vec<&str> = Vec::new();
        if !matches!(self.era, Era::Legacy { .. }) {
            entries.push(MODERN_META);
        }
        if !meta.is_empty() {
            entries.push(meta);
        }
        let mut members = fields.to_owned();
        if !entries.is_empty() {
            if !members.is_empty() {
                members.push(',');
            }
            members.push_str(&format!("\"_meta\":{{{}}}", entries.join(",")));
        }
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":{},\"params\":{{{members}}}}}",
            json_quote(method)
        )
    }

    async fn send(
        &self,
        method: WireMethod,
        headers: Vec<(String, String, bool)>,
        body: String,
    ) -> Result<WireResponse, McpError> {
        let request = WireRequest {
            method,
            tls: self.remote.tls,
            host: self.remote.host.clone(),
            port: self.remote.port,
            path: self.remote.path.clone(),
            headers,
            body: body.into_bytes(),
            allow_private: self.allow_private,
        };
        self.wire.send(request).await.map_err(|fail| match fail {
            WireFail::Unreached => McpError::Unreached,
            WireFail::Lost => McpError::Io,
            WireFail::Stale => McpError::Stale,
        })
    }

    /// 요청 하나를 보내고 응답 메시지를 읽는다. 상태 코드의 해석은 부르는 쪽이 한다.
    async fn exchange(
        &self,
        id: u64,
        headers: Vec<(String, String, bool)>,
        body: String,
    ) -> Result<Answer, McpError> {
        let WireResponse {
            status,
            headers: reply,
            body: mut stream,
        } = self.send(WireMethod::Post, headers, body).await?;
        let session = header_value(&reply, "mcp-session-id").map(str::to_owned);
        if let Some(session) = &session {
            self.guard(session.as_bytes())?;
        }
        if !(200..300).contains(&status) {
            let message = error_message(&mut *stream, id, self.bearer.as_deref()).await?;
            return Ok(Answer {
                status,
                session,
                message,
            });
        }
        if matches!(status, 202 | 204) {
            return Ok(Answer {
                status,
                session,
                message: Message::Empty,
            });
        }
        let kind = header_value(&reply, "content-type").map(media_type);
        let message = match kind.as_deref() {
            Some("application/json") => {
                let bytes = read_all(&mut *stream, MAX_FRAME).await?;
                self.guard(&bytes)?;
                let text = std::str::from_utf8(&bytes).map_err(|_| McpError::Protocol)?;
                let value = parse(text).ok_or(McpError::Protocol)?;
                if holds(&value, self.bearer.as_deref()) {
                    return Err(McpError::Exposed);
                }
                response_message(&value, id)?.ok_or(McpError::Protocol)?
            }
            Some("text/event-stream") => self.stream(&mut *stream, id).await?,
            _ => return Err(McpError::Protocol),
        };
        Ok(Answer {
            status,
            session,
            message,
        })
    }

    /// SSE 응답에서 우리 요청의 응답을 기다린다. 알림은 무시하고, 서버의 요청은 판본 규칙대로 다룬다.
    async fn stream(&self, body: &mut dyn ChunkSource, id: u64) -> Result<Message, McpError> {
        let mut events = Events::new(MAX_FRAME);
        let (mut total, mut seen) = (0usize, 0usize);
        loop {
            let chunk = match body.next().await {
                Ok(Some(chunk)) => chunk,
                // 응답 없이 스트림이 끝났다: 현행 판본은 끊긴 스트림의 요청을 잃은 것으로 본다(재개 없음)
                Ok(None) => return Err(McpError::Closed),
                Err(_) => return Err(McpError::Io),
            };
            total += chunk.len();
            if total > MAX_STREAM {
                return Err(McpError::Protocol);
            }
            for data in events.push(&chunk).map_err(|_| McpError::Protocol)? {
                seen += 1;
                if seen > MAX_SKIPPED {
                    return Err(McpError::Protocol);
                }
                // 이벤트마다 원문과 해석한 값을 모두 본다(서버의 요청 번호를 되풀이하는 답에도 자격이 실리지 않게 처리하기 전에 거른다)
                self.guard(data.as_bytes())?;
                let value = parse(&data).ok_or(McpError::Protocol)?;
                if holds(&value, self.bearer.as_deref()) {
                    return Err(McpError::Exposed);
                }
                let obj = value.as_object().ok_or(McpError::Protocol)?;
                if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                    return Err(McpError::Protocol);
                }
                if let Some(method) = obj.get("method").and_then(Value::as_str) {
                    match (obj.get("id"), &self.era) {
                        (None, _) => {
                            if method == LIST_CHANGED {
                                self.changed.store(true, Ordering::SeqCst);
                            }
                        }
                        (Some(request), Era::Legacy { .. }) => self.refuse(request, method).await,
                        (Some(_), _) => return Err(McpError::Protocol),
                    }
                    continue;
                }
                if let Some(message) = response_message(&value, id)? {
                    return Ok(message);
                }
            }
        }
    }

    /// (구판) 서버가 보낸 요청에 답한다: `ping`은 빈 결과, 그 밖은 지원하지 않는다는 오류. 답을 보내지 못해도 진행한다.
    async fn refuse(&self, request: &Value, method: &str) {
        let id = request.to_string();
        let body = if method == "ping" {
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}")
        } else {
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"method not supported\"}}}}"
            )
        };
        let _ = timeout(
            AUX_LIMIT,
            self.send(WireMethod::Post, self.legacy_headers(), body),
        )
        .await;
    }

    /// 판본을 가린다: 읽기 전용 `server/discover`로 현행 서버인지 보고, 공식 호환 절차에서 구판으로 확인될 때만 `initialize`로 간다.
    pub async fn connect(&mut self, limit: Duration) -> Result<(), McpError> {
        timeout(limit, self.negotiate())
            .await
            .map_err(|_| McpError::Timeout)?
    }

    async fn negotiate(&mut self) -> Result<(), McpError> {
        let id = self.take_id();
        let body = self.body(id, "server/discover", "", "");
        let headers = self.modern_headers("server/discover", None, Vec::new());
        let Answer {
            status, message, ..
        } = self.exchange(id, headers, body).await?;
        match (status, message) {
            (200..=299, Message::Result(result)) => {
                let versions = listed(result.get("supportedVersions"));
                if versions.contains(&MODERN) {
                    self.era = Era::Modern;
                    Ok(())
                } else if offers_legacy(&versions) {
                    self.initialize().await
                } else {
                    Err(McpError::Version)
                }
            }
            // 현행 서버가 우리 판본을 지원하지 않는다고 답했다: 목록에 구판이 있으면 그 판본으로 간다
            (400, Message::Error { code: -32022, data }) => {
                let versions = listed(data.as_ref().and_then(|d| d.get("supported")));
                if versions.contains(&MODERN) {
                    Err(McpError::Protocol)
                } else if offers_legacy(&versions) {
                    self.initialize().await
                } else {
                    Err(McpError::Version)
                }
            }
            // 현행 서버의 그 밖의 오류(머리 불일치·기능 부족): 구판으로 떨어지지 않는다
            (400, Message::Error { code, .. }) if (-32021..=-32020).contains(&code) => {
                Err(McpError::Status {
                    status,
                    code: Some(code),
                })
            }
            (200..=299, Message::Empty) => Err(McpError::Protocol),
            // 현행 오류가 아닌 4xx(또는 200 안의 JSON-RPC 오류: 구판 서버가 모르는 메서드에 하는 답): 구판 서버다
            (400 | 404 | 405, Message::Empty | Message::Error { .. })
            | (200..=299, Message::Error { .. }) => self.initialize().await,
            (status, Message::Error { code, .. }) => Err(McpError::Status {
                status,
                code: Some(code),
            }),
            (status, _) => Err(McpError::Status { status, code: None }),
        }
    }

    /// (구판) `initialize` → `notifications/initialized`. 세션 ID가 있으면 이후 요청에 싣는다.
    async fn initialize(&mut self) -> Result<(), McpError> {
        let id = self.take_id();
        let body = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"initialize\",\"params\":{{\"protocolVersion\":{},\"capabilities\":{{}},\"clientInfo\":{CLIENT_INFO}}}}}",
            json_quote(LEGACY)
        );
        let answer = self.exchange(id, self.plain_headers(), body).await?;
        let session = answer.session.clone();
        let result = finish(answer)?;
        // 서버는 우리가 내세운 판본이나 자기가 지원하는 다른 판본으로 답한다: 아는 구판이면 그 판본으로 간다
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .and_then(|given| {
                LEGACY_VERSIONS
                    .iter()
                    .copied()
                    .find(|known| *known == given)
            })
            .ok_or(McpError::Version)?;
        if session.as_deref().is_some_and(|text| !session_ok(text)) {
            return Err(McpError::Protocol);
        }
        self.era = Era::Legacy { session, version };
        let response = self
            .send(
                WireMethod::Post,
                self.legacy_headers(),
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".into(),
            )
            .await?;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(McpError::Status {
                status: response.status,
                code: None,
            })
        }
    }

    /// 요청 하나(판본에 맞는 머리·`_meta`)를 보내고 결과를 받는다.
    async fn request(
        &mut self,
        method: &str,
        fields: &str,
        meta: &str,
        name: Option<&str>,
        params: Vec<(String, String)>,
    ) -> Result<Value, McpError> {
        let id = self.take_id();
        let body = self.body(id, method, fields, meta);
        let headers = match self.era {
            Era::Legacy { .. } => self.legacy_headers(),
            _ => self.modern_headers(method, name, params),
        };
        finish(self.exchange(id, headers, body).await?)
    }

    /// 서버가 내세우는 도구 정의 전체(모든 쪽). 끝나지 않는 목록은 불완전하므로 오류다. `nextCursor`는 없거나(마지막 쪽) 비어 있지 않은 짧은 글이어야 하고(그 밖의 값은
    /// 마지막 쪽이 아니라 잘못된 응답이다), 같은 커서가 다시 오면 오류다. 모으는 동안 서버가 목록이 바뀌었다고 알리면 실패한다(한 시점의 목록이 아니다).
    pub async fn tools(&mut self, limit: Duration) -> Result<Vec<Value>, McpError> {
        self.changed.store(false, Ordering::SeqCst);
        let mut all = Vec::new();
        let (mut cursor, mut used): (Option<String>, Vec<String>) = (None, Vec::new());
        for _ in 0..MAX_PAGES {
            let fields = cursor
                .as_deref()
                .map_or_else(String::new, |c| format!("\"cursor\":{}", json_quote(c)));
            let result = timeout(
                limit,
                self.request("tools/list", &fields, "", None, Vec::new()),
            )
            .await
            .map_err(|_| McpError::Timeout)??;
            if self.changed.swap(false, Ordering::SeqCst) {
                return Err(McpError::Changed);
            }
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or(McpError::Protocol)?;
            if all.len() + page.len() > MAX_LISTED {
                return Err(McpError::Protocol);
            }
            all.extend(page.iter().cloned());
            match result.get("nextCursor") {
                None => return Ok(all),
                Some(Value::String(next))
                    if !next.is_empty() && next.len() <= MAX_CURSOR && !used.contains(next) =>
                {
                    used.push(next.clone());
                    cursor = Some(next.clone());
                }
                Some(_) => return Err(McpError::Protocol),
            }
        }
        Err(McpError::Protocol)
    }

    /// 이름이 `name`인 도구의 정의 한 건. 없으면 `None`, 같은 이름이 둘이면 모호하므로 오류다.
    pub async fn find_tool(
        &mut self,
        name: &str,
        limit: Duration,
    ) -> Result<Option<Value>, McpError> {
        let mut found = None;
        for tool in self.tools(limit).await? {
            if tool.get("name").and_then(Value::as_str) == Some(name) {
                if found.is_some() {
                    return Err(McpError::Protocol);
                }
                found = Some(tool);
            }
        }
        Ok(found)
    }

    /// `tools/call` 한 번. `arguments`는 정규화한 인자 글 그대로, `meta`는 `_meta`에 더 싣는 멤버들(중괄호 없는 JSON 조각), `params`는 `Mcp-Param-*` 머리다.
    /// 응답이 없거나 상태가 이상하면 오류를 돌려줄 뿐 다시 보내지 않는다.
    pub async fn call(
        &mut self,
        name: &str,
        arguments: &str,
        meta: &str,
        params: Vec<(String, String)>,
        limit: Duration,
    ) -> Result<Value, McpError> {
        let fields = format!("\"name\":{},\"arguments\":{arguments}", json_quote(name));
        timeout(
            limit,
            self.request("tools/call", &fields, meta, Some(name), params),
        )
        .await
        .map_err(|_| McpError::Timeout)?
    }

    /// (구판) 세션을 끝낸다. 서버가 허용하지 않아도(405) 상관없다. 결과와 무관한 정리라 실패는 무시한다.
    pub async fn close(&mut self) {
        if let Era::Legacy {
            session: Some(_), ..
        } = &self.era
        {
            let _ = timeout(
                AUX_LIMIT,
                self.send(WireMethod::Delete, self.legacy_headers(), String::new()),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests;

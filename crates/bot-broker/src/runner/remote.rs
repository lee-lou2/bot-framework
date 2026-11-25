//! 원격(Streamable HTTP) MCP 서버로 가는 호출. 샌드박스·프록시 없이 브로커가 직접 연결하되, 로컬 서버와 같은 경계를 지킨다.
//!
//! - **접속 대상은 설정한 주소의 `호스트:포트` 하나**다. 도구 선언 ∩ 프로필 상한의 허용 목록(`EgressPolicy`)에 정확히 있어야 하고(아니면 보내지 않는다),
//!   연결마다 이름 풀이 결과 전체를 검사하고 검사한 주소로만 연결하는 일은 전송(`RemoteWire`)이 맡는다. 리다이렉트를 따라가지 않는다.
//! - bearer 값은 프로필 이름공간에서 이름으로 읽어 요청 머리(`Authorization`)에만 싣고, 이 호출이 쓰는 자격 목록에 넣어 결과·보고에 섞이면 막는다.
//! - 승인 뒤 서버가 내세우는 도구 정의(이름·설명·입력 스키마)가 `tools.lock.yaml`의 해시와 다르면 **호출하기 전에** 막는다(`Refused`, 고정 코드·감사).
//! - 단계 원장(`effects: true`)은 로컬 FD·프록시 전용 주소와 원장 증거에 기대므로 원격 서버에는 같은 채널이 없다: 실행하기 전에 지원하지 않는다.
//!   도구가 선언한 `secrets`·`env`도 원격 서버에 전달할 길이 없어 같다.
//! - 연결 종료는 원격 실행의 종료 증명이 아니다(`reap_proves_end`). 시간 초과·끊김은 보냈다면 `Unknown`이고 기존 복구(보류)를 따른다. **숨은 재전송은 없다.**
use super::*;
use crate::mcp::{
    header,
    http::{Http, MODERN},
};

/// 호출을 보내기 전 단계(판본 확인·도구 목록)의 실패: 서버가 자격을 되풀이했으면 정의를 믿지 않고 고정 코드로 거절하고, 그 밖은 아직 아무것도 실행되지 않았다.
fn before_call(error: McpError) -> Ran {
    match error {
        McpError::Exposed => Ran::Refused("response_credential"),
        _ => Ran::NotSent,
    }
}

/// 서버가 실행하기 전에 확정적으로 거절한 JSON-RPC·MCP 오류 코드인가(잘못된 요청·없는 메서드·잘못된 인자, 머리 불일치·기능 부족·판본 미지원).
/// **2xx이고 이 요청과 같은 번호의 응답**에 실린 코드일 때만 쓴다(그 밖의 응답은 서버가 이 요청을 실행했는지 알려 주지 않는다).
fn rejected(code: i64) -> bool {
    (-32602..=-32600).contains(&code) || (-32022..=-32020).contains(&code)
}

/// 원격 서버에 싣는 `_meta` 멤버(중괄호 없는 JSON 조각). 외부 멱등키 `[profile-id, run-id, call-id]`만이고, 주체·진행 출처·수신 대화 같은 신원 정보는 보내지 않는다.
fn meta_members(job: &ToolJob) -> String {
    format!(
        "\"bot/call-id\":{},\"bot/profile-id\":{},\"bot/run-id\":{}",
        json_quote(&job.call.0),
        json_quote(&job.attempt.profile.0),
        json_quote(&job.attempt.run.0)
    )
}

impl McpRunner {
    pub(super) async fn execute_remote(
        &self,
        job: &ToolJob,
        remote: &Remote,
        budget: Budget,
    ) -> Ran {
        let tool = &job.tool.tool;
        if tool.effects || !tool.secrets.is_empty() || !tool.env.is_empty() {
            return Ran::NotSent;
        }
        let Some(wire) = self.wire.clone() else {
            return Ran::NotSent;
        };
        // 접속 대상은 설정한 주소의 호스트:포트 하나이고 도구 선언 ∩ 프로필 상한에 정확히 있어야 한다
        let cap = self
            .host
            .egress_caps
            .get(&job.attempt.profile)
            .map(Vec::as_slice);
        if !EgressPolicy::new(&job.tool.egress, cap).permits(&remote.host, remote.port) {
            return Ran::NotSent;
        }
        let (mut credentials, mut bearer) = (Vec::new(), None);
        if let Some(name) = &remote.bearer {
            if crate::private::reserved_secret(name) {
                return Ran::NotSent;
            }
            let Ok(store) = self
                .deps
                .secrets
                .for_profile(job.attempt.profile.clone())
                .await
            else {
                return Ran::NotSent;
            };
            let Ok(value) = store.resolve(name.clone()).await else {
                return Ran::NotSent;
            };
            let Ok(text) = String::from_utf8(value.0.clone()) else {
                return Ran::NotSent;
            };
            // 머리에 그대로 실리므로 비어 있거나 줄바꿈·NUL이 든 값은 쓰지 않는다
            if text.is_empty() || text.contains(['\0', '\r', '\n']) {
                return Ran::NotSent;
            }
            credentials.push(value.0);
            bearer = Some(text);
        }
        let mut http = Http::new(wire, remote.clone(), bearer, self.cfg.allow_private_egress);
        let sent = AtomicBool::new(false);
        let ran = match timeout(
            budget.wall,
            self.drive_remote(job, &mut http, &credentials, &sent, budget.call),
        )
        .await
        {
            Ok(ran) => ran,
            Err(_) if sent.load(Ordering::SeqCst) => Ran::Unknown("wall_timeout"),
            Err(_) => Ran::NotSent,
        };
        // 구판 세션 정리(결과와 무관한 최선의 정리)
        http.close().await;
        ran
    }

    async fn drive_remote(
        &self,
        job: &ToolJob,
        http: &mut Http,
        credentials: &[Vec<u8>],
        sent: &AtomicBool,
        call_limit: Duration,
    ) -> Ran {
        let limit = self.cfg.handshake_timeout;
        if let Err(error) = http.connect(limit).await {
            return before_call(error);
        }
        let Some(schema) = parse(&job.tool.tool.input.0) else {
            return Ran::NotSent;
        };
        let Some(declared) = canonicalize(&schema) else {
            return Ran::NotSent;
        };
        let entry = match http.find_tool(&job.tool.name, limit).await {
            Ok(entry) => entry,
            Err(error) => return before_call(error),
        };
        if let Err(ran) = check_definition(&job.tool.tool, entry.as_ref(), &declared) {
            return ran;
        }
        // 현행 판본은 스키마의 `x-mcp-header`가 가리키는 인자를 `Mcp-Param-*` 머리로 되풀이한다(고정한 선언의 스키마로 만든다: 서버가 준 것이 아니다)
        let params = if http.version() == Some(MODERN) {
            let Some(args) = parse(&job.args) else {
                return Ran::NotSent;
            };
            match header::param_headers(&schema, &args) {
                Ok(params) => params,
                Err(_) => return Ran::Refused("header_annotation"),
            }
        } else {
            Vec::new()
        };
        sent.store(true, Ordering::SeqCst);
        let result = http
            .call(
                &job.tool.name,
                &job.args,
                &meta_members(job),
                params,
                call_limit,
            )
            .await;
        match result {
            Ok(value) => self.vet(&value, credentials),
            // 보내기 전에 끝났다(연결을 맺기 전에 실패했거나 요청 직전 확인에서 구판이었다): 서버에 닿지 않았다
            Err(McpError::Unreached | McpError::Stale) => Ran::NotSent,
            // 서버가 자격을 되풀이했다: 호출은 갔으니 결과는 쓰지 않고 막는다(쓰기는 보류)
            Err(McpError::Exposed) => Ran::Blocked("result_credential"),
            // 2xx이고 같은 번호의 JSON-RPC 오류만 서버의 답이다
            Err(McpError::Server(code)) if rejected(code) => Ran::Rejected,
            Err(McpError::Server(_)) => Ran::Unknown("server_error"),
            // 2xx가 아닌 응답은 인증·권한 거절(401·403)이든 5xx든 본문이 무엇이든 실행됐는지 알려 주지 않는다: 결과 불명(쓰기는 보류)
            Err(McpError::Status { .. }) => Ran::Unknown("http_status"),
            Err(McpError::Timeout) => Ran::Unknown("timeout"),
            Err(McpError::Closed) => Ran::Unknown("closed"),
            Err(McpError::Io) => Ran::Unknown("io"),
            Err(_) => Ran::Unknown("protocol"),
        }
    }
}

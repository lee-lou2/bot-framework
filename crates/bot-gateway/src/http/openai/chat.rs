//! `POST /v1/chat/completions`: 읽기 → 검증 → 원자 접수(또는 같은 키의 재전달) → 기한 안에서 기다리기 → JSON 또는 SSE.
//!
//! 기한은 인증 직후 한 번 정하는 단조시계 하나다(`wait_ms`). 기다림은 그보다 `min(2초, wait_ms/4)` 먼저 끝내고 그 예산을 마지막 권한 확인과 응답 쓰기에 남긴다.
//! 접수 전의 실패만 SDK가 자동으로 다시 보낼 상태(408·429·503)로 낸다. 접수 뒤의 불명·실패는 422 + `x-should-retry: false`다.
use super::{
    Cx,
    auth::{AuthError, Authed},
    body, digest,
    error::Failure,
    json, render, request,
    stream::{self, Sink},
};
use crate::{
    http::{HttpGateway, wire::is_json},
    inbound::valid_request_id,
    ingress::{ApiInbound, ApiRelease, ApiVerdict},
    routes::Routes,
};
use axum::{
    body::Body,
    http::{HeaderMap, request::Parts},
    response::Response,
};
use bot_core::{
    api::{ApiKey, ApiRun, ApiRuns, ApiState},
    command::TokenUsage,
    id::{ProfileId, RequestId},
    port::Error,
};
use render::{Finish, Identity};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, error::Elapsed, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

/// 상태를 다시 읽는 간격: 처음엔 빠르게, 길어지면 이 값까지 늘린다(긴 요청은 초 단위라 이 이상 촘촘할 필요가 없다)
const POLL_START: Duration = Duration::from_millis(40);
const POLL_MAX: Duration = Duration::from_millis(500);
/// 본문을 읽는 데 주는 시간(총 기한이 더 짧으면 그쪽). 느리게 보내는 상대가 긴 대기 시간만큼 연결을 붙들지 못하게 한다.
const BODY_READ: Duration = Duration::from_secs(10);
/// 서버가 만든 멱등 키 접두사. 128비트 OS 난수 16진수가 뒤따른다.
const GENERATED_PREFIX: &str = "gw-";

/// 사용자에게 내보낼 응답의 내용(성공 또는 고정 안내)
pub(super) enum Answer {
    Text {
        text: String,
        finish: Finish,
        usage: Option<TokenUsage>,
    },
    /// 호스트가 정한 안내(승인 대기·처리 중·승인 불가). 사용량을 싣지 않고 `stop`으로 끝난다.
    Notice(String),
}

impl Answer {
    pub(super) fn parts(self) -> (String, Finish, Option<TokenUsage>) {
        match self {
            Answer::Text {
                text,
                finish,
                usage,
            } => (text, finish, usage),
            Answer::Notice(text) => (text, Finish::Stop, None),
        }
    }
}

pub(super) enum Resolved {
    Answer(Answer),
    Failure(Failure),
    /// 스트림의 상대가 끊었다(작업은 그대로 둔다)
    Gone,
}

/// 접수한 작업 하나를 끝까지(또는 기한까지) 따라가는 데 필요한 것
pub(super) struct Ctl {
    pub gateway: Arc<HttpGateway>,
    pub api: Arc<dyn ApiRuns>,
    pub authed: Authed,
    pub profile: ProfileId,
    pub key: String,
    /// 기다림이 끝나는 때(총 기한보다 `min(2초, wait_ms/4)` 이르다)
    pub wait_until: Instant,
    /// 응답 쓰기까지의 총 기한
    pub hard: Instant,
    pub stop: CancellationToken,
    /// 참이면 결과를 내보내기 직전 확인을 이 구조 밖에서(스트림 본문이 조각을 넘기는 순간) 한다. 거짓이면 `resolve`가 돌려주기 직전에 한다.
    pub at_handoff: bool,
}

/// 조회 호출의 결과: 끝났거나(`Ok`) 기한 안에 끝나지 못했다(`Err`)
type Lookup = Result<Result<Option<ApiRun>, Error>, Elapsed>;

/// 속도 제한에 걸린 요청의 키로 이미 접수한 작업이 있는가
enum Known {
    /// 같은 요청의 작업이다
    Run(Box<ApiRun>),
    /// 같은 키로 다른 요청을 이미 접수했다
    Different,
    /// **확실히** 없다(원장이 «없음»이라고 답했거나, 서버가 만든 키라 있을 수 없다): 접수 전의 제한이므로 429가 맞다
    Nothing,
    /// 확인하지 못했다(조회 한도·원장 오류·시간 초과): 이 키로 이미 접수했을 수 있으므로 다시 보내도 되는 429가 아니라 «확인 불가»(422, 재전송 금지)다
    Unconfirmed,
}

/// 같은 키의 작업 조회 한 번의 결과
enum Look {
    /// 조회가 끝났거나(`Ok`) 기한 안에 끝나지 못했다(`Err`)
    Got(Box<Lookup>),
    /// 종료 신호가 와서 멈췄다
    Stopped,
    /// 스트림의 상대가 끊었다
    Gone,
}

enum Step {
    Answer(Answer),
    Failed(Failure),
    Wait,
}

/// 저장된 상태가 지금 말할 수 있는 것
fn step(state: &ApiState) -> Step {
    match state {
        // 일반 보류·자식 대기는 «승인 대기»가 아니다: 계속 기다린다
        ApiState::Active | ApiState::Held => Step::Wait,
        ApiState::NeedsHuman => Step::Answer(Answer::Notice(render::approval_pending())),
        ApiState::ApprovalBlocked(why) => Step::Answer(Answer::Notice(render::blocked(*why))),
        ApiState::Final {
            text,
            truncated,
            usage,
        } => {
            // 빈 성공은 전달하지 않는다
            if text.trim().is_empty() {
                Step::Failed(Failure::run_failed())
            } else {
                Step::Answer(Answer::Text {
                    text: text.clone(),
                    finish: if *truncated {
                        Finish::Length
                    } else {
                        Finish::Stop
                    },
                    usage: *usage,
                })
            }
        }
        ApiState::Failed { .. } => Step::Failed(Failure::run_failed()),
        ApiState::Cancelled => Step::Failed(Failure::run_cancelled()),
    }
}

/// 본문 읽기의 기한: 총 기한(기다림이 끝나는 때)과 `BODY_READ` 중 이른 쪽
fn read_deadline(started: Instant, wait_until: Instant) -> Instant {
    wait_until.min(started + BODY_READ)
}

/// `Idempotency-Key`: 없거나 하나여야 하고, 있으면 요청 ID 문자 규칙(1~128바이트)이어야 한다.
fn supplied_key(headers: &HeaderMap) -> Result<Option<String>, Failure> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Failure::invalid(
            "Idempotency-Key 헤더는 하나만 보낼 수 있어요",
        ));
    }
    match first.to_str() {
        Ok(key) if valid_request_id(key) => Ok(Some(key.to_owned())),
        _ => Err(Failure::invalid("Idempotency-Key 형식이 올바르지 않아요")),
    }
}

/// 공식 SDK의 재시도 표지(`x-stainless-retry-count > 0`). 실행·권한의 근거로 쓰지 않고, 키 없는 재전송이 새 실행이 되는 것만 막는다.
fn retried(headers: &HeaderMap) -> bool {
    headers
        .get("x-stainless-retry-count")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .is_some_and(|count| count > 0)
}

impl HttpGateway {
    pub(super) async fn chat(
        self: Arc<Self>,
        cx: Cx,
        authed: Authed,
        routes: Arc<Routes>,
        parts: Parts,
        body: Body,
    ) -> Response {
        let request_id = cx.request_id.clone();
        let mut applied = None;
        let response = match self
            .clone()
            .chat_inner(cx, authed, routes, parts, body, &mut applied)
            .await
        {
            Ok(response) => response,
            Err(failure) => failure.response(),
        };
        super::stamp(response, &request_id, applied.as_deref())
    }

    async fn chat_inner(
        self: Arc<Self>,
        cx: Cx,
        authed: Authed,
        routes: Arc<Routes>,
        parts: Parts,
        body: Body,
        applied: &mut Option<String>,
    ) -> Result<Response, Failure> {
        let config = self
            .ingress
            .host()
            .http
            .openai
            .get(&authed.endpoint)
            .cloned()
            .ok_or_else(Failure::unavailable)?;
        let api = self.api.clone().ok_or_else(Failure::unavailable)?;
        // 인증 직후 한 번 정하는 총 기한
        let started = Instant::now();
        let wait = config.wait();
        let reserve = (wait / 4).min(Duration::from_secs(2));
        let hard = started + wait;
        let wait_until = hard - reserve;
        let headers = &parts.headers;

        let supplied = supplied_key(headers)?;
        let supplied_by_caller = supplied.is_some();
        let key = match supplied {
            Some(key) => key,
            None if retried(headers) => return Err(Failure::idempotency_required()),
            None => {
                let random = super::random_hex(16).map_err(|_| Failure::unavailable())?;
                format!("{GENERATED_PREFIX}{random}")
            }
        };
        *applied = Some(key.clone());
        if !is_json(headers) {
            return Err(Failure::unsupported_media());
        }
        let limit = self.ingress.host().openai_body_bytes();
        let bytes = body::read(body, limit, headers, read_deadline(started, wait_until)).await?;
        let chat = request::parse(&bytes)?;
        let digest = digest::semantic(&chat.model, &chat.body);
        let profile = routes
            .openai_profile(&authed.endpoint)
            .cloned()
            .ok_or_else(Failure::model_not_found)?;
        let inbound = ApiInbound {
            endpoint: authed.endpoint.clone(),
            model: chat.model.clone(),
            principal: authed.principal.clone(),
            request: RequestId(key.clone()),
            body: chat.body.clone(),
            digest,
            conversation: super::random_hex(16).map_err(|_| Failure::unavailable())?,
        };
        // 접수 호출이 기한을 넘기면 커밋했는지 모른다: 같은 키로 확인한다
        let verdict =
            match timeout_at(wait_until, self.ingress.admit_api(&*api, &routes, inbound)).await {
                Ok(verdict) => verdict,
                Err(_) => ApiVerdict::Unknown,
            };
        let ctl = Ctl {
            gateway: self.clone(),
            api,
            authed,
            profile,
            key,
            wait_until,
            hard,
            stop: cx.stop.clone(),
            at_handoff: false,
        };
        let run = match verdict {
            ApiVerdict::Admitted(run) => *run,
            ApiVerdict::Unknown => ctl.confirm(digest).await?,
            ApiVerdict::Conflict => return Err(Failure::idempotency_conflict()),
            ApiVerdict::NotMember => return Err(Failure::access_denied()),
            ApiVerdict::Hidden => return Err(Failure::model_not_found()),
            // 속도 제한에 걸렸어도 같은 키로 이미 접수한 작업이면 새 실행 없이 그 작업에 붙는다(재조회는 제한이 아니라 같은 일의 확인이다)
            ApiVerdict::RateLimited(wait) => match ctl.known(digest, supplied_by_caller).await {
                Known::Run(run) => *run,
                Known::Different => return Err(Failure::idempotency_conflict()),
                Known::Nothing => return Err(Failure::rate_limited(wait)),
                Known::Unconfirmed => return Err(Failure::outcome_unknown()),
            },
            ApiVerdict::Full => return Err(Failure::rate_limited(Duration::from_secs(1))),
            ApiVerdict::Unavailable => return Err(Failure::unavailable()),
        };
        let identity = Identity::new(&run.run.0, run.created, &run.model);
        if chat.stream {
            // 헤더를 보내기 전에 키·권한·접수했던 바인딩을 한 번 확인한다(그 뒤의 오류는 200 스트림 안에서만 알릴 수 있다)
            ctl.release(&run).await?;
            return Ok(stream::response(
                ctl,
                run,
                identity,
                chat.include_usage,
                cx.permit,
            ));
        }
        match ctl.resolve(run, None).await {
            Resolved::Answer(answer) => {
                let (text, finish, usage) = answer.parts();
                Ok(json(&render::completion(&identity, &text, finish, usage)))
            }
            Resolved::Failure(failure) => Err(failure),
            Resolved::Gone => Err(Failure::unavailable()),
        }
    }
}

impl Ctl {
    /// 속도 제한에 걸린 요청이 키를 **직접 보냈으면**, 현재 인가 범위(프로필·입구·주체·키)의 원장에서 그 작업을 찾는다. 있으면 새 실행 없이 그 작업에 붙고(다른 요청의
    /// 키였으면 충돌), 없거나 확인하지 못하면 제한이 그대로다. 서버가 만든 키는 이 요청이 처음이라 찾지 않는다. 이 조회도 상태 조회의 속도 제한을 받는다.
    async fn known(&self, digest: [u8; 32], supplied: bool) -> Known {
        if !supplied {
            return Known::Nothing;
        }
        let who = format!("{}/{}", self.profile.0, self.authed.principal.0);
        if self.gateway.status_limiter.check(&who).is_err() {
            return Known::Unconfirmed;
        }
        match timeout_at(self.wait_until, self.api.lookup(self.scope())).await {
            Ok(Ok(Some(run))) if run.digest == digest => Known::Run(Box::new(run)),
            Ok(Ok(Some(_))) => Known::Different,
            Ok(Ok(None)) => Known::Nothing,
            Ok(Err(_)) | Err(_) => Known::Unconfirmed,
        }
    }

    /// 접수 호출이 끝났는지 알 수 없을 때: 같은 키로 원장을 조회해 확인한다. 그래도 모르면 422(새 실행·가짜 성공은 없다).
    async fn confirm(&self, digest: [u8; 32]) -> Result<ApiRun, Failure> {
        let lookup = self.api.lookup(self.scope());
        match timeout_at(self.hard, lookup).await {
            Ok(Ok(Some(run))) if run.digest == digest => Ok(run),
            // 같은 키를 다른 요청이 먼저 커밋했다
            Ok(Ok(Some(_))) => Err(Failure::idempotency_conflict()),
            _ => Err(Failure::outcome_unknown()),
        }
    }

    /// 같은 키의 작업을 조회한다. 조회가 오래 걸려도 스트림에는 keep-alive를 쓰고, 상대가 끊거나 종료 신호가 오면 곧바로 멈춘다.
    async fn lookup_alive(&self, sink: &mut Option<&mut Sink>) -> Look {
        enum Tick {
            Done(Box<Lookup>),
            Beat,
            Gone,
            Stop,
        }
        let lookup = timeout_at(self.wait_until, self.api.lookup(self.scope()));
        tokio::pin!(lookup);
        loop {
            let beat = sink
                .as_deref()
                .map_or(Duration::from_secs(3600), Sink::until_keep_alive);
            let tick = {
                let gone = async {
                    match sink.as_deref() {
                        Some(sink) => sink.gone().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    done = &mut lookup => Tick::Done(Box::new(done)),
                    () = sleep(beat) => Tick::Beat,
                    () = gone => Tick::Gone,
                    () = self.stop.cancelled() => Tick::Stop,
                }
            };
            match tick {
                Tick::Done(done) => return Look::Got(done),
                Tick::Gone => return Look::Gone,
                Tick::Stop => return Look::Stopped,
                Tick::Beat => {
                    if let Some(sink) = sink.as_deref_mut()
                        && !sink.keep_alive().await
                    {
                        return Look::Gone;
                    }
                }
            }
        }
    }

    fn scope(&self) -> ApiKey {
        ApiKey {
            profile: self.profile.clone(),
            endpoint: self.authed.endpoint.clone(),
            principal: self.authed.principal.clone(),
            key: RequestId(self.key.clone()),
        }
    }

    /// 상태·결과·안내를 내보내기 직전의 재확인: 키 값이 지금도 같은 입구·주체를 가리키는지, 그 주체가 지금도 멤버이고 접수했던 바인딩·에이전트를 쓸 수 있는지,
    /// 이 프로세스의 판본이 활성 판본인지. 총 기한 안에 확인하지 못하면 확인하지 못한 것이다(가린다).
    pub(super) async fn release(&self, run: &ApiRun) -> Result<(), Failure> {
        timeout_at(self.hard, self.recheck(run))
            .await
            .unwrap_or_else(|_| Err(Failure::access_denied()))
    }

    async fn recheck(&self, run: &ApiRun) -> Result<(), Failure> {
        let gateway = &self.gateway;
        // 오래 기다릴 수 있는 것(키 보관소 읽기)을 먼저 끝내고, 활성 집합·멤버·바인딩은 **마지막에** 본다: 이 확인이 끝나면 기다림 없이 결과를 넘기므로
        // 그 사이에 다른 프로세스가 새 집합을 게시했어도(옛 답을 돌려주지 않도록) 그 게시는 마지막 확인에 보인다.
        match gateway.openai_reauthenticate(&self.authed).await {
            Ok(()) => {}
            Err(AuthError::Invalid) => return Err(Failure::invalid_api_key()),
            Err(AuthError::Unavailable) => return Err(Failure::access_denied()),
        }
        match gateway
            .ingress
            .recheck_api(&self.authed.endpoint, &self.authed.principal, run)
            .await
        {
            ApiRelease::Allowed => Ok(()),
            ApiRelease::NotMember | ApiRelease::Unconfirmed => Err(Failure::access_denied()),
            ApiRelease::Hidden => Err(Failure::model_not_found()),
        }
    }

    /// 기다림이 끝났는가: 기한(`wait_until`)이 됐거나 종료 신호가 왔다
    fn waiting_over(&self) -> bool {
        Instant::now() >= self.wait_until || self.stop.is_cancelled()
    }

    /// 끝났거나 사람이 해야 할 일이 정해질 때까지(또는 기한까지) 같은 작업을 지켜본다. 새 실행을 만들지 않는다.
    pub(super) async fn resolve(&self, mut run: ApiRun, mut sink: Option<&mut Sink>) -> Resolved {
        let mut delay = POLL_START;
        // 마지막 조회가 성공했는가(실패한 채 기한이 오면 작업 상태를 확인하지 못한 것이다)
        let mut confirmed = true;
        loop {
            match step(&run.state) {
                Step::Wait => {}
                Step::Answer(answer) => return self.deliver(&run, answer).await,
                Step::Failed(failure) => return self.refuse(&run, failure).await,
            }
            if self.waiting_over() {
                break;
            }
            let mut nap = delay.min(self.wait_until.saturating_duration_since(Instant::now()));
            if let Some(sink) = sink.as_deref() {
                nap = nap.min(sink.until_keep_alive());
            }
            let gone = async {
                match sink.as_deref() {
                    Some(sink) => sink.gone().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = sleep(nap) => {}
                () = self.stop.cancelled() => {}
                () = gone => return Resolved::Gone,
            }
            if let Some(sink) = sink.as_deref_mut()
                && !sink.keep_alive().await
            {
                return Resolved::Gone;
            }
            // 잠든 사이 기다림이 끝났거나 종료 신호가 왔다: 새 조회를 시작하지 않는다. 기한을 이미 넘긴 조회는 시작하자마자 만료돼 «확인 못 함»으로 오인된다.
            if self.waiting_over() {
                break;
            }
            match self.lookup_alive(&mut sink).await {
                Look::Got(done) => match *done {
                    Ok(Ok(Some(next))) if next.run == run.run => {
                        run = next;
                        confirmed = true;
                    }
                    // 접수한 작업이 사라졌거나 다른 작업이 됐다: 확인할 수 없다
                    Ok(Ok(_)) => return Resolved::Failure(Failure::outcome_unknown()),
                    // 원장이 조회에 실패했다: 지금 상태를 확인하지 못했다
                    Ok(Err(_)) => confirmed = false,
                    // 기다림의 기한이 이 조회 도중에 끝났다: 새 정보 없이 마지막으로 확인한 상태로 마친다(조회가 실패한 것이 아니다)
                    Err(_) => {}
                },
                // 종료 신호: 마지막으로 확인한 상태 그대로 진행 중 안내로 끝낸다(다음 순서에 종료 신호를 보고 나간다)
                Look::Stopped => {}
                Look::Gone => return Resolved::Gone,
            }
            delay = (delay * 2).min(POLL_MAX);
        }
        if !confirmed {
            return Resolved::Failure(Failure::outcome_unknown());
        }
        self.deliver(&run, Answer::Notice(render::still_running()))
            .await
    }

    async fn deliver(&self, run: &ApiRun, answer: Answer) -> Resolved {
        if self.at_handoff {
            return Resolved::Answer(answer);
        }
        match self.release(run).await {
            Ok(()) => Resolved::Answer(answer),
            Err(failure) => Resolved::Failure(failure),
        }
    }

    /// 실패·취소도 권한이 있는 호출자에게만 알린다
    async fn refuse(&self, run: &ApiRun, failure: Failure) -> Resolved {
        if self.at_handoff {
            return Resolved::Failure(failure);
        }
        match self.release(run).await {
            Ok(()) => Resolved::Failure(failure),
            Err(denied) => Resolved::Failure(denied),
        }
    }
}

#[cfg(test)]
mod tests;

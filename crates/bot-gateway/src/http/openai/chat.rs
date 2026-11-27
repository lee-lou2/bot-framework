//! `POST /v1/chat/completions` handler: read, validate, atomically admit, wait within deadline, and respond as JSON or SSE.
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

const POLL_START: Duration = Duration::from_millis(40);
const POLL_MAX: Duration = Duration::from_millis(500);
/// Caps body read time so slow clients cannot hold connections for the full `wait_ms` budget.
const BODY_READ: Duration = Duration::from_secs(10);
const GENERATED_PREFIX: &str = "gw-";

pub(super) enum Answer {
    Text {
        text: String,
        finish: Finish,
        usage: Option<TokenUsage>,
    },
    /// Fixed host notice (pending approval, still running, or blocked); omits usage and finishes with `stop`.
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
    /// Streaming client disconnected; leave the run running.
    Gone,
}

pub(super) struct Ctl {
    pub gateway: Arc<HttpGateway>,
    pub api: Arc<dyn ApiRuns>,
    pub authed: Authed,
    pub profile: ProfileId,
    pub key: String,
    /// Polling cutoff (`hard - min(2s, wait_ms / 4)`), reserving budget for final recheck and response write.
    pub wait_until: Instant,
    /// Total deadline for the entire request including final checks and output.
    pub hard: Instant,
    pub stop: CancellationToken,
    /// When true, pre-emission checks run in the stream handoff gate instead of inside `resolve`.
    pub at_handoff: bool,
}

type Lookup = Result<Result<Option<ApiRun>, Error>, Elapsed>;

/// Ledger check for a rate-limited request to decide whether it replays an already-admitted run.
enum Known {
    Run(Box<ApiRun>),
    Different,
    /// Confirmed absent (or server-generated key): safe to return retryable 429.
    Nothing,
    /// Could not verify absence; must fail closed as non-retryable 422 (`outcome_unknown`), never 429.
    Unconfirmed,
}

enum Look {
    Got(Box<Lookup>),
    Stopped,
    Gone,
}

enum Step {
    Answer(Answer),
    Failed(Failure),
    Wait,
}

fn step(state: &ApiState) -> Step {
    match state {
        // Holds and delegated child waits are not human-approval waits; keep polling.
        ApiState::Active | ApiState::Held => Step::Wait,
        ApiState::NeedsHuman => Step::Answer(Answer::Notice(render::approval_pending())),
        ApiState::ApprovalBlocked(why) => Step::Answer(Answer::Notice(render::blocked(*why))),
        ApiState::Final {
            text,
            truncated,
            usage,
        } => {
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

fn read_deadline(started: Instant, wait_until: Instant) -> Instant {
    wait_until.min(started + BODY_READ)
}

fn supplied_key(headers: &HeaderMap) -> Result<Option<String>, Failure> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Failure::invalid(
            "Only one Idempotency-Key header is allowed",
        ));
    }
    match first.to_str() {
        Ok(key) if valid_request_id(key) => Ok(Some(key.to_owned())),
        _ => Err(Failure::invalid("Invalid Idempotency-Key format")),
    }
}

/// Detects official SDK retries (`x-stainless-retry-count > 0`) solely to block keyless duplicate runs.
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
        // Single monotonic deadline anchored right after authentication.
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
                let random = bot_core::random::random_hex(16);
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
            conversation: bot_core::random::random_hex(16),
        };
        // Admission timeout leaves commit status ambiguous; confirm via ledger lookup.
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
            // Replaying an already-admitted key bypasses new-admission rate limits.
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
            // Re-verify before committing HTTP 200 SSE headers; later failures can only be sent inline.
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

    async fn confirm(&self, digest: [u8; 32]) -> Result<ApiRun, Failure> {
        let lookup = self.api.lookup(self.scope());
        match timeout_at(self.hard, lookup).await {
            Ok(Ok(Some(run))) if run.digest == digest => Ok(run),
            Ok(Ok(Some(_))) => Err(Failure::idempotency_conflict()),
            _ => Err(Failure::outcome_unknown()),
        }
    }

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

    /// Re-verifies key, membership, binding, and active set within `self.hard`; fails closed on timeout.
    pub(super) async fn release(&self, run: &ApiRun) -> Result<(), Failure> {
        timeout_at(self.hard, self.recheck(run))
            .await
            .unwrap_or_else(|_| Err(Failure::access_denied()))
    }

    async fn recheck(&self, run: &ApiRun) -> Result<(), Failure> {
        let gateway = &self.gateway;
        // Finish the potentially slow secret-store check first, then check active-set/membership/binding last
        // so any concurrent active-set publish is observed immediately before releasing output.
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

    fn waiting_over(&self) -> bool {
        Instant::now() >= self.wait_until || self.stop.is_cancelled()
    }

    pub(super) async fn resolve(&self, mut run: ApiRun, mut sink: Option<&mut Sink>) -> Resolved {
        let mut delay = POLL_START;
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
            // Avoid starting a lookup past `wait_until`, which would time out immediately and look unconfirmed.
            if self.waiting_over() {
                break;
            }
            match self.lookup_alive(&mut sink).await {
                Look::Got(done) => match *done {
                    Ok(Ok(Some(next))) if next.run == run.run => {
                        run = next;
                        confirmed = true;
                    }
                    Ok(Ok(_)) => return Resolved::Failure(Failure::outcome_unknown()),
                    Ok(Err(_)) => confirmed = false,
                    // Deadline hit mid-lookup: keep last confirmed state instead of treating as an error.
                    Err(_) => {}
                },
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

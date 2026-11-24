//! 계측. 끝난 스팬을 `core::trace::Tracer`에 넘긴다(동기·무대기·무실패). 요청 접수와 회신 발송은 worker 세대가 없으므로
//! 가짜 `Attempt`를 만들지 않고, `(프로필, run)`에서 정한 추적 ID와 `bot.request.id`·`bot.run.id` 속성으로 worker 쪽
//! 추적과 잇는다. 본문·인자·영수증·승인 nonce는 싣지 않는다(허용 어휘 밖).
use crate::{
    inbound::{Event, kind_name},
    ingress::{Admission, Rejection},
};
use bot_core::{
    id::{ProfileId, RunId},
    trace::{Key, SpanIds, SpanKind, SpanRecord, Tracer, key, now_nano},
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 등록된 트리거 정의와 맞지 않은 HTTP 요청에 쓰는 고정 값(OpenTelemetry의 알 수 없는 method 표기와 같다).
pub const UNMATCHED_METHOD: &str = "_OTHER";
pub const UNMATCHED_ROUTE: &str = "unmatched";

fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

/// 같은 `(프로필, run)`의 게이트웨이 스팬(접수·회신)은 한 추적에 모인다. 스팬 ID는 매번 다르다.
pub fn ids(profile: &ProfileId, run: &RunId) -> SpanIds {
    let base = digest(&[b"bot-gateway-trace", profile.0.as_bytes(), run.0.as_bytes()]);
    ids_from(base)
}

/// run이 아직 없는 접수(거절·충돌 등)의 추적. 요청 ID로 묶는다.
pub fn ids_for_request(profile: &ProfileId, binding: &str, request: &str) -> SpanIds {
    let base = digest(&[
        b"bot-gateway-request",
        profile.0.as_bytes(),
        binding.as_bytes(),
        request.as_bytes(),
    ]);
    ids_from(base)
}

fn ids_from(base: [u8; 32]) -> SpanIds {
    let mut trace = [0u8; 16];
    trace.copy_from_slice(&base[..16]);
    let unique = digest(&[
        b"bot-gateway-span",
        &base,
        &now_nano().to_be_bytes(),
        &SEQ.fetch_add(1, Ordering::Relaxed).to_be_bytes(),
    ]);
    let mut span = [0u8; 8];
    span.copy_from_slice(&unique[..8]);
    if span == [0; 8] {
        span[7] = 1;
    }
    SpanIds {
        trace,
        span,
        parent: None,
    }
}

/// 접수 결과의 고정 어휘 이름(`bot.outcome`)
pub fn outcome(admission: &Admission) -> &'static str {
    match admission {
        Admission::Accepted { .. } => "accepted",
        Admission::Private => "private",
        Admission::Ignored => "ignored",
        Admission::Rejected(rejection) => match rejection {
            Rejection::Unregistered => "unregistered",
            Rejection::Forbidden => "forbidden",
            Rejection::RateLimited(_) => "rate_limited",
            Rejection::TooLarge => "too_large",
            Rejection::NoRoute => "no_route",
            Rejection::Invalid => "invalid",
            Rejection::Conflict => "conflict",
            Rejection::Unavailable => "unavailable",
        },
    }
}

/// 실패 분류(고정 어휘). 성공·정상 거절은 오류가 아니다.
pub fn failure(admission: &Admission) -> Option<&'static str> {
    match admission {
        Admission::Rejected(Rejection::Unavailable) => Some("unavailable"),
        Admission::Rejected(Rejection::Conflict) => Some("conflict"),
        _ => None,
    }
}

/// 접수 한 건의 스팬에 쓰는 비민감 정보
pub struct Intake<'a> {
    pub profile: &'a ProfileId,
    pub binding: &'a str,
    pub request: &'a str,
    pub principal: &'a str,
    pub conversation: &'static str,
    pub trigger: Option<&'a str>,
    pub private: bool,
    /// 요청이 맞은 등록된 트리거 정의의 (method, path). 요청이 보낸 값이 아니다.
    pub route: Option<(&'a str, &'a str)>,
    pub adapter: &'static str,
    pub event: &'a Event,
    pub started: u64,
}

pub fn record_intake(tracer: &dyn Tracer, intake: &Intake<'_>, admission: &Admission) {
    let ids = match admission {
        Admission::Accepted { run, .. } => ids(intake.profile, run),
        _ => ids_for_request(intake.profile, intake.binding, intake.request),
    };
    let mut span = SpanRecord::start(intake.profile, format!("ingress {}", intake.adapter), ids)
        .kind(SpanKind::Server)
        .started_at(intake.started)
        .attr(key::BOT_PROFILE, intake.profile.0.as_str())
        .attr(key::BOT_BINDING, intake.binding)
        .attr(key::BOT_REQUEST_ID, intake.request)
        .attr(key::BOT_PRINCIPAL, intake.principal)
        .attr(key::BOT_CONVERSATION_KIND, intake.conversation)
        .attr(key::BOT_OUTCOME, outcome(admission));
    if let Admission::Accepted { run, .. } = admission {
        span = span.attr(key::BOT_RUN_ID, run.0.as_str());
    }
    if let Some(trigger) = intake.trigger {
        span = span.attr(key::BOT_TRIGGER, trigger);
    }
    if let Event::Http { .. } = intake.event {
        // 요청이 보낸 method·경로는 기록하지 않는다(호출자가 식별자나 비밀을 경로에 실을 수 있고, 내용 꺼짐·식별자 가명에서도 그대로 나간다).
        // 등록된 트리거 정의와 맞은 요청만 그 정의의 값을 쓰고, 맞지 않으면 고정 값이다.
        let (method, route) = intake.route.unwrap_or((UNMATCHED_METHOD, UNMATCHED_ROUTE));
        span = span
            .attr(key::HTTP_REQUEST_METHOD, method)
            .attr(key::HTTP_ROUTE, route);
    }
    if intake.private {
        // 민감 DM: 내용 기록을 차단하는 표지(표지 자체는 내보내지 않는다). 원문은 어디에도 싣지 않는다.
        span = span.attr(key::BOT_PRIVATE, true);
    }
    if let Some(class) = failure(admission) {
        span = span.fail(class);
    }
    span.end(tracer);
}

/// 회신 발송 한 건의 스팬에 쓰는 비민감 정보
pub struct Delivered<'a> {
    pub profile: &'a ProfileId,
    pub run: &'a RunId,
    pub binding: &'a str,
    pub conversation: &'static str,
    pub outcome: &'static str,
    pub error: Option<&'static str>,
    pub started: u64,
}

/// 회신 발송 한 건의 스팬(발송 세대는 worker 시도가 아니다)
pub fn record_delivery(tracer: &dyn Tracer, delivered: &Delivered<'_>) {
    let mut span = SpanRecord::start(
        delivered.profile,
        "deliver reply",
        ids(delivered.profile, delivered.run),
    )
    .kind(SpanKind::Producer)
    .started_at(delivered.started)
    .attr(key::BOT_PROFILE, delivered.profile.0.as_str())
    .attr(key::BOT_RUN_ID, delivered.run.0.as_str())
    .attr(key::BOT_BINDING, delivered.binding)
    .attr(key::BOT_CONVERSATION_KIND, delivered.conversation)
    .attr(key::BOT_OUTCOME, delivered.outcome);
    if let Some(class) = delivered.error {
        span = span.fail(class);
    }
    span.end(tracer);
}

/// 승인 버튼 클릭 한 건의 스팬. nonce는 싣지 않는다.
pub fn record_approval(
    tracer: &dyn Tracer,
    profile: &ProfileId,
    principal: &str,
    outcome: &'static str,
    started: u64,
) {
    let seed = format!("{principal}:{started}");
    let ids = ids_for_request(profile, "approval", &seed);
    let key_outcome: Key = key::BOT_APPROVAL_OUTCOME;
    let mut span = SpanRecord::start(profile, "approval decide", ids)
        .kind(SpanKind::Server)
        .started_at(started)
        .attr(key::BOT_PROFILE, profile.0.as_str())
        .attr(key::BOT_PRINCIPAL, principal)
        .attr(key_outcome, outcome);
    if matches!(outcome, "unavailable") {
        span = span.fail("unavailable");
    }
    span.end(tracer);
}

pub fn conversation_name(kind: &bot_core::state::ConversationKind) -> &'static str {
    kind_name(kind)
}

//! 계측 계약과 시도별 추적 ID.
//!
//! - 호출하는 쪽(워커·브로커·게이트웨이)은 끝난 스팬을 `Tracer`에 넘긴다. 모든 호출은 동기·무대기·무실패다.
//! - 추적 ID는 시도 `(프로필, run, 세대)`에서 결정적으로 만든다. 분리 배치의 다른 프로세스도 같은 값을 계산하므로
//!   IPC로 문맥을 실어 나르지 않아도 한 시도의 스팬이 한 추적에 모인다. 세대가 바뀌면 새 추적이다.
//! - 속성 키는 내보낼 때 허용 목록을 검사한다. 승인 nonce는 수집하지 않는다.
//! - 내보내기(`export`)는 허용 어휘 밖의 키, 내용 키(기본 꺼짐), 비밀 값을 경계에서 다시 걸러 낸다.

use crate::id::{Attempt, ProfileId};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Key = &'static str;

/// 속성 값. 사용자 글·비밀은 넣지 않는다(길이·개수·분류만).
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<u32> for Val {
    fn from(v: u32) -> Self {
        Self::Int(v.into())
    }
}
impl From<u64> for Val {
    fn from(v: u64) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

/// 속성·이벤트 키 어휘. 규약(OTel GenAI 등)이 바뀌면 이 모듈만 고친다.
pub mod key {
    use super::Key;
    pub const BOT_PROFILE: Key = "bot.profile";
    pub const BOT_AGENT: Key = "bot.agent";
    pub const BOT_REQUEST_ID: Key = "bot.request.id";
    pub const BOT_RUN_ID: Key = "bot.run.id";
    pub const BOT_RUN_ROOT: Key = "bot.run.root";
    pub const BOT_RUN_PARENT: Key = "bot.run.parent";
    pub const BOT_ATTEMPT_GENERATION: Key = "bot.attempt.generation";
    pub const BOT_BINDING: Key = "bot.binding";
    pub const BOT_TRIGGER: Key = "bot.trigger";
    pub const BOT_CONVERSATION_KIND: Key = "bot.conversation.kind";
    pub const BOT_PRINCIPAL: Key = "bot.principal";
    pub const BOT_OUTCOME: Key = "bot.outcome";
    pub const BOT_TOOL_RISK: Key = "bot.tool.risk";
    pub const BOT_TOOL_REVISION: Key = "bot.tool.revision";
    pub const BOT_CALL_STATE: Key = "bot.call.state";
    pub const BOT_APPROVAL_RISK: Key = "bot.approval.risk";
    pub const BOT_APPROVAL_OUTCOME: Key = "bot.approval.outcome";
    pub const BOT_EFFECT_STEP: Key = "bot.effect.step";
    pub const BOT_EFFECT_STATE: Key = "bot.effect.state";
    pub const BOT_EFFECT_SEND: Key = "bot.effect.send";
    pub const BOT_HOLD_REASON: Key = "bot.hold.reason";
    pub const BOT_DELEGATE_STEP: Key = "bot.delegate.step";
    pub const BOT_DELEGATE_CHILD: Key = "bot.delegate.child";
    pub const BOT_ADMIN_ACTION: Key = "bot.admin.action";
    pub const BOT_ADMIN_VERDICT: Key = "bot.admin.verdict";
    pub const ERROR_TYPE: Key = "error.type";
    pub const GEN_AI_OPERATION_NAME: Key = "gen_ai.operation.name";
    pub const GEN_AI_AGENT_NAME: Key = "gen_ai.agent.name";
    pub const GEN_AI_CONVERSATION_ID: Key = "gen_ai.conversation.id";
    pub const GEN_AI_PROVIDER_NAME: Key = "gen_ai.provider.name";
    pub const GEN_AI_REQUEST_MODEL: Key = "gen_ai.request.model";
    pub const GEN_AI_RESPONSE_MODEL: Key = "gen_ai.response.model";
    pub const GEN_AI_USAGE_INPUT_TOKENS: Key = "gen_ai.usage.input_tokens";
    pub const GEN_AI_USAGE_OUTPUT_TOKENS: Key = "gen_ai.usage.output_tokens";
    pub const GEN_AI_TOOL_NAME: Key = "gen_ai.tool.name";
    pub const GEN_AI_TOOL_CALL_ID: Key = "gen_ai.tool.call.id";
    pub const GEN_AI_TOOL_TYPE: Key = "gen_ai.tool.type";
    pub const HTTP_REQUEST_METHOD: Key = "http.request.method";
    pub const HTTP_ROUTE: Key = "http.route";
    pub const HTTP_RESPONSE_STATUS_CODE: Key = "http.response.status_code";
    // 내보내기가 남기는 표지.
    pub const BOT_DROPPED: Key = "bot.dropped";
    /// 내부 Bool 표지. true이면 이 스팬의 내용 기록을 차단하며 표지 자체는 내보내지 않는다.
    pub const BOT_PRIVATE: Key = "bot.private";
    pub const BOT_CONTENT_DROPPED: Key = "bot.content.dropped";
    // 내용 속성. 프로필이 켠 경우에만 가림 처리를 거쳐 나간다(기본 꺼짐).
    pub const GEN_AI_INPUT_MESSAGES: Key = "gen_ai.input.messages";
    pub const GEN_AI_OUTPUT_MESSAGES: Key = "gen_ai.output.messages";
    pub const GEN_AI_SYSTEM_INSTRUCTIONS: Key = "gen_ai.system_instructions";
    pub const GEN_AI_TOOL_CALL_ARGUMENTS: Key = "gen_ai.tool.call.arguments";
    pub const GEN_AI_TOOL_CALL_RESULT: Key = "gen_ai.tool.call.result";
    pub const BOT_ERROR_MESSAGE: Key = "bot.error.message";

    pub const CONTENT: [Key; 6] = [
        GEN_AI_INPUT_MESSAGES,
        GEN_AI_OUTPUT_MESSAGES,
        GEN_AI_SYSTEM_INSTRUCTIONS,
        GEN_AI_TOOL_CALL_ARGUMENTS,
        GEN_AI_TOOL_CALL_RESULT,
        BOT_ERROR_MESSAGE,
    ];

    /// 사람·대화를 가리켜 가명 처리 대상인 키.
    pub const IDENTITY: [Key; 3] = [BOT_PRINCIPAL, GEN_AI_CONVERSATION_ID, BOT_REQUEST_ID];

    /// 허용 어휘 전체. 내보내기는 이 밖의 키를 버린다.
    pub const ALLOWED: [Key; 40] = [
        BOT_PROFILE,
        BOT_AGENT,
        BOT_REQUEST_ID,
        BOT_RUN_ID,
        BOT_RUN_ROOT,
        BOT_RUN_PARENT,
        BOT_ATTEMPT_GENERATION,
        BOT_BINDING,
        BOT_TRIGGER,
        BOT_CONVERSATION_KIND,
        BOT_PRINCIPAL,
        BOT_OUTCOME,
        BOT_TOOL_RISK,
        BOT_TOOL_REVISION,
        BOT_CALL_STATE,
        BOT_APPROVAL_RISK,
        BOT_APPROVAL_OUTCOME,
        BOT_EFFECT_STEP,
        BOT_EFFECT_STATE,
        BOT_EFFECT_SEND,
        BOT_HOLD_REASON,
        BOT_DELEGATE_STEP,
        BOT_DELEGATE_CHILD,
        BOT_ADMIN_ACTION,
        BOT_ADMIN_VERDICT,
        ERROR_TYPE,
        GEN_AI_OPERATION_NAME,
        GEN_AI_AGENT_NAME,
        GEN_AI_CONVERSATION_ID,
        GEN_AI_PROVIDER_NAME,
        GEN_AI_REQUEST_MODEL,
        GEN_AI_RESPONSE_MODEL,
        GEN_AI_USAGE_INPUT_TOKENS,
        GEN_AI_USAGE_OUTPUT_TOKENS,
        GEN_AI_TOOL_NAME,
        GEN_AI_TOOL_CALL_ID,
        GEN_AI_TOOL_TYPE,
        HTTP_REQUEST_METHOD,
        HTTP_ROUTE,
        HTTP_RESPONSE_STATUS_CODE,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanKind {
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

/// 스팬이 속한 추적 안의 위치. 같은 시도의 스팬은 같은 `trace`를 쓴다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpanIds {
    pub trace: [u8; 16],
    pub span: [u8; 8],
    pub parent: Option<[u8; 8]>,
}

/// 길이를 앞에 붙인 조각들을 FNV-1a 두 줄기로 섞고 splitmix로 마무리한다. 암호 해시가 아니다(ID 생성용).
fn mix(parts: &[&[u8]]) -> [u8; 16] {
    let (mut a, mut b) = (0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64);
    for part in parts {
        for byte in (part.len() as u64).to_le_bytes().iter().chain(part.iter()) {
            a = (a ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
            b = (b.rotate_left(7) ^ u64::from(*byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    let fin = |mut z: u64| {
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&fin(a ^ b.rotate_left(32)).to_be_bytes());
    out[8..].copy_from_slice(&fin(b ^ a.rotate_left(17)).to_be_bytes());
    if out == [0; 16] {
        out[15] = 1;
    }
    out
}

fn nonzero8(mut id: [u8; 8]) -> [u8; 8] {
    if id == [0; 8] {
        id[7] = 1;
    }
    id
}

impl SpanIds {
    /// 시도의 루트 스팬. 같은 시도는 어느 프로세스에서도 같은 값이다.
    pub fn attempt(a: &Attempt) -> Self {
        let g = a.generation.0.to_be_bytes();
        let base = mix(&[b"bot-trace", a.profile.0.as_bytes(), a.run.0.as_bytes(), &g]);
        let mut span = [0; 8];
        span.copy_from_slice(&mix(&[b"bot-span", &base])[..8]);
        Self {
            trace: base,
            span: nonzero8(span),
            parent: None,
        }
    }

    /// 이 스팬의 자식. `tag`는 종류(`tool`·`effect` 등), `id`는 그 종류 안의 안정된 ID다.
    pub fn child(&self, tag: &str, id: &str) -> Self {
        let mut span = [0; 8];
        span.copy_from_slice(&mix(&[b"bot-child", &self.span, tag.as_bytes(), id.as_bytes()])[..8]);
        Self {
            trace: self.trace,
            span: nonzero8(span),
            parent: Some(self.span),
        }
    }
}

/// trace ID 값으로 정하는 샘플링. 어느 프로세스에서 계산해도 같은 결론이다.
pub fn sample_hit(trace: &[u8; 16], ratio: f64) -> bool {
    if ratio.is_nan() || ratio <= 0.0 {
        return false;
    }
    if ratio >= 1.0 {
        return true;
    }
    let low = u64::from_be_bytes(trace[8..].try_into().unwrap_or([0; 8]));
    (low as f64 / u64::MAX as f64) < ratio
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub name: Key,
    pub at_nano: u64,
    pub attrs: Vec<(Key, Val)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkRecord {
    pub trace: [u8; 16],
    pub span: [u8; 8],
}

/// 끝난 스팬 하나.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanRecord {
    pub profile: ProfileId,
    pub name: String,
    pub kind: SpanKind,
    pub ids: SpanIds,
    pub start_nano: u64,
    pub end_nano: u64,
    pub attrs: Vec<(Key, Val)>,
    pub events: Vec<EventRecord>,
    pub links: Vec<LinkRecord>,
    /// 실패 분류(고정 어휘). 있으면 상태는 오류다.
    pub error: Option<&'static str>,
}

pub fn now_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

impl SpanRecord {
    /// 지금 시작하는 스팬. `end`를 부르면 끝 시각을 찍고 `Tracer`에 넘긴다.
    pub fn start(profile: &ProfileId, name: impl Into<String>, ids: SpanIds) -> Self {
        let now = now_nano();
        Self {
            profile: profile.clone(),
            name: name.into(),
            kind: SpanKind::Internal,
            ids,
            start_nano: now,
            end_nano: now,
            attrs: Vec::new(),
            events: Vec::new(),
            links: Vec::new(),
            error: None,
        }
    }

    pub fn kind(mut self, kind: SpanKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn attr(mut self, key: Key, val: impl Into<Val>) -> Self {
        self.attrs.push((key, val.into()));
        self
    }

    pub fn event(mut self, name: Key, attrs: Vec<(Key, Val)>) -> Self {
        self.events.push(EventRecord {
            name,
            at_nano: now_nano(),
            attrs,
        });
        self
    }

    pub fn link(mut self, trace: [u8; 16], span: [u8; 8]) -> Self {
        self.links.push(LinkRecord { trace, span });
        self
    }

    pub fn fail(mut self, class: &'static str) -> Self {
        self.error = Some(class);
        self
    }

    /// 시작 시각을 바꾼다(이미 일어난 일을 나중에 기록할 때).
    pub fn started_at(mut self, nano: u64) -> Self {
        self.start_nano = nano;
        self
    }

    pub fn end(mut self, tracer: &dyn Tracer) {
        self.end_nano = now_nano().max(self.start_nano);
        tracer.record(self);
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find_map(|(k, v)| match v {
            Val::Str(s) if *k == key => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        self.attrs.iter().find_map(|(k, v)| match v {
            Val::Int(n) if *k == key => Some(*n),
            _ => None,
        })
    }
}

/// 끝난 스팬을 받는 곳. **모든 메서드는 동기·무대기·무실패**여야 한다. 가득 차면 스스로 버린다.
pub trait Tracer: Send + Sync {
    fn record(&self, span: SpanRecord);
}

/// 아무 일도 하지 않는 기본 구현.
pub struct NoopTracer;

impl Tracer for NoopTracer {
    fn record(&self, _: SpanRecord) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Generation, RunId};
    use std::collections::HashSet;

    fn attempt(run: &str, generation: u64) -> Attempt {
        Attempt {
            profile: ProfileId("alpha".into()),
            run: RunId(run.into()),
            claim: None,
            generation: Generation(generation),
        }
    }

    #[test]
    fn same_attempt_gets_the_same_ids_in_every_process() {
        let a = SpanIds::attempt(&attempt("r-1", 4));
        assert_eq!(a, SpanIds::attempt(&attempt("r-1", 4)));
        let tool = a.child("tool", "c-17");
        assert_eq!(
            tool,
            SpanIds::attempt(&attempt("r-1", 4)).child("tool", "c-17")
        );
        assert_eq!((tool.trace, tool.parent), (a.trace, Some(a.span)));
        assert_ne!(tool.span, a.child("tool", "c-18").span);
        assert_ne!(tool.span, a.child("effect", "c-17").span);
    }

    #[test]
    fn a_new_generation_run_or_profile_starts_a_new_trace() {
        let base = SpanIds::attempt(&attempt("r-1", 4));
        assert_ne!(base.trace, SpanIds::attempt(&attempt("r-1", 5)).trace);
        assert_ne!(base.trace, SpanIds::attempt(&attempt("r-2", 4)).trace);
        let mut other = attempt("r-1", 4);
        other.profile = ProfileId("beta".into());
        assert_ne!(base.trace, SpanIds::attempt(&other).trace);
        // 조각 경계가 달라도 같은 문자열 이어붙임이 되지 않는다.
        let x = SpanIds::attempt(&attempt("ab", 1)).trace;
        let mut y = attempt("b", 1);
        y.profile = ProfileId("alphaa".into());
        assert_ne!(x, SpanIds::attempt(&y).trace);
    }

    #[test]
    fn ids_are_unique_and_nonzero_over_many_attempts() {
        let mut seen = HashSet::new();
        for run in 0..2000u64 {
            for generation in 1..4u64 {
                let a = SpanIds::attempt(&attempt(&format!("r-{run}"), generation));
                assert!(a.trace != [0; 16] && a.span != [0; 8]);
                assert!(
                    seen.insert(a.trace)
                        && seen.insert({
                            let mut t = [0u8; 16];
                            t[..8].copy_from_slice(&a.span);
                            t
                        })
                );
            }
        }
    }

    #[test]
    fn sampling_follows_the_trace_id() {
        let t = SpanIds::attempt(&attempt("r-1", 1)).trace;
        assert!(sample_hit(&t, 1.0) && !sample_hit(&t, 0.0) && !sample_hit(&t, f64::NAN));
        assert_eq!(sample_hit(&t, 0.5), sample_hit(&t, 0.5));
        let hits = (0..4000u64)
            .filter(|n| {
                sample_hit(
                    &SpanIds::attempt(&attempt(&format!("r-{n}"), 1)).trace,
                    0.25,
                )
            })
            .count();
        assert!((800..1200).contains(&hits), "{hits}");
    }

    #[test]
    fn vocabulary_is_unique_lowercase_and_closed() {
        let all: HashSet<_> = key::ALLOWED.iter().chain(key::CONTENT.iter()).collect();
        assert_eq!(all.len(), key::ALLOWED.len() + key::CONTENT.len());
        for k in &all {
            assert!(
                k.bytes().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || c == b'.'
                    || c == b'_'),
                "{k}"
            );
        }
        assert!(!all.contains(&key::BOT_PRIVATE));
        assert!(key::IDENTITY.iter().all(|k| key::ALLOWED.contains(k)));
        assert!(!key::ALLOWED.iter().any(|k| key::CONTENT.contains(k)));
        // 승인 nonce·인자·본문·영수증은 어휘에 없다.
        for banned in ["nonce", "args", "receipt", "body", "secret"] {
            assert!(!key::ALLOWED.iter().any(|k| k.contains(banned)), "{banned}");
        }
    }
}

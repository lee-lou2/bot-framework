//! 정책을 거친 스팬(`Out`)을 OTLP/HTTP protobuf(`ExportTraceServiceRequest`)로 직접 인코딩한다(외부 의존 없음).
//! 필드 번호는 opentelemetry-proto의 trace·common·resource·collector 정의를 따른다(시험이 독립 디코더로 대조한다).

use super::{Key, SpanIds, SpanKind, Val};

/// (이름, 시각, 속성).
pub(super) type EventOut = (String, u64, Vec<(Key, Val)>);

/// 내보낼 스팬. 정책(`policy::apply`)이 만든 값만 여기까지 온다.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Out {
    pub ids: SpanIds,
    pub name: String,
    pub kind: SpanKind,
    pub start: u64,
    pub end: u64,
    pub attrs: Vec<(Key, Val)>,
    pub events: Vec<EventOut>,
    pub links: Vec<([u8; 16], [u8; 8])>,
    pub error: Option<String>,
    pub dropped: u32,
}

fn varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    varint(buf, (u64::from(field) << 3) | u64::from(wire));
}

fn bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    tag(buf, field, 2);
    varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn text(buf: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        bytes(buf, field, s.as_bytes());
    }
}

fn fixed64(buf: &mut Vec<u8>, field: u32, v: u64) {
    tag(buf, field, 1);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn number(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        tag(buf, field, 0);
        varint(buf, v);
    }
}

/// 하위 메시지: 안쪽을 먼저 만들어 길이를 앞에 붙인다.
fn message(buf: &mut Vec<u8>, field: u32, fill: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    fill(&mut inner);
    bytes(buf, field, &inner);
}

/// `AnyValue`는 oneof라 빈 문자열·거짓·0도 설정된 값으로 그대로 싣는다.
fn any(buf: &mut Vec<u8>, v: &Val) {
    match v {
        Val::Str(s) => bytes(buf, 1, s.as_bytes()),
        Val::Bool(b) => {
            tag(buf, 2, 0);
            varint(buf, u64::from(*b));
        }
        Val::Int(n) => {
            tag(buf, 3, 0);
            varint(buf, *n as u64);
        }
    }
}

fn key_value(buf: &mut Vec<u8>, field: u32, k: &str, v: &Val) {
    message(buf, field, |kv| {
        text(kv, 1, k);
        message(kv, 2, |a| any(a, v));
    });
}

const fn kind(k: SpanKind) -> u64 {
    match k {
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Producer => 4,
        SpanKind::Consumer => 5,
    }
}

fn span(buf: &mut Vec<u8>, s: &Out) {
    bytes(buf, 1, &s.ids.trace);
    bytes(buf, 2, &s.ids.span);
    if let Some(p) = s.ids.parent {
        bytes(buf, 4, &p);
    }
    text(buf, 5, &s.name);
    number(buf, 6, kind(s.kind));
    fixed64(buf, 7, s.start);
    fixed64(buf, 8, s.end);
    for (k, v) in &s.attrs {
        key_value(buf, 9, k, v);
    }
    number(buf, 10, u64::from(s.dropped));
    for (name, at, attrs) in &s.events {
        message(buf, 11, |e| {
            fixed64(e, 1, *at);
            text(e, 2, name);
            for (k, v) in attrs {
                key_value(e, 3, k, v);
            }
        });
    }
    for (trace, id) in &s.links {
        message(buf, 13, |l| {
            bytes(l, 1, trace);
            bytes(l, 2, id);
        });
    }
    if let Some(class) = &s.error {
        message(buf, 15, |st| {
            text(st, 2, class);
            number(st, 3, 2);
        });
    }
}

/// 한 번의 내보내기 요청. 리소스는 프로세스·프로필 정보만, 범위는 이 계측의 이름이다.
pub(super) fn request(resource: &[(&str, Val)], scope: (&str, &str), spans: &[Out]) -> Vec<u8> {
    let mut out = Vec::new();
    message(&mut out, 1, |rs| {
        message(rs, 1, |r| {
            for (k, v) in resource {
                key_value(r, 1, k, v);
            }
        });
        message(rs, 2, |ss| {
            message(ss, 1, |sc| {
                text(sc, 1, scope.0);
                text(sc, 2, scope.1);
            });
            for s in spans {
                message(ss, 2, |sp| span(sp, s));
            }
        });
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_bytes(v: u64) -> Vec<u8> {
        let mut b = Vec::new();
        varint(&mut b, v);
        b
    }

    #[test]
    fn varints_follow_the_protobuf_spec() {
        assert_eq!(varint_bytes(0), [0x00]);
        assert_eq!(varint_bytes(1), [0x01]);
        assert_eq!(varint_bytes(127), [0x7f]);
        assert_eq!(varint_bytes(128), [0x80, 0x01]);
        assert_eq!(varint_bytes(300), [0xac, 0x02]);
        assert_eq!(varint_bytes(u64::MAX).len(), 10);
        let mut b = Vec::new();
        any(&mut b, &Val::Int(-1));
        assert_eq!(
            b,
            [
                0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
            ],
            "음수 int64는 10바이트"
        );
    }

    #[test]
    fn any_values_keep_empty_and_zero_because_oneof_members_are_always_sent() {
        let enc = |v: &Val| {
            let mut b = Vec::new();
            any(&mut b, v);
            b
        };
        assert_eq!(enc(&Val::Str(String::new())), [0x0a, 0x00]);
        assert_eq!(enc(&Val::Bool(false)), [0x10, 0x00]);
        assert_eq!(enc(&Val::Bool(true)), [0x10, 0x01]);
        assert_eq!(enc(&Val::Int(0)), [0x18, 0x00]);
        assert_eq!(enc(&Val::Str("ab".into())), [0x0a, 0x02, b'a', b'b']);
    }

    #[test]
    fn fixed64_is_little_endian_and_messages_are_length_prefixed() {
        let mut b = Vec::new();
        fixed64(&mut b, 7, 0x0102_0304_0506_0708);
        assert_eq!(b, [0x39, 8, 7, 6, 5, 4, 3, 2, 1]);
        let mut m = Vec::new();
        message(&mut m, 2, |x| text(x, 1, "hi"));
        assert_eq!(m, [0x12, 0x04, 0x0a, 0x02, b'h', b'i']);
        let mut e = Vec::new();
        message(&mut e, 2, |_| {});
        assert_eq!(e, [0x12, 0x00], "빈 하위 메시지도 자리는 남는다");
    }
}

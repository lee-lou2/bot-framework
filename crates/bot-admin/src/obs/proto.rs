//! Dependency-free protobuf wire encoder shared by OTLP trace and log exporters.

use bot_core::trace::Val;

pub(crate) fn varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub(crate) fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    varint(buf, (u64::from(field) << 3) | u64::from(wire));
}

pub(crate) fn bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    tag(buf, field, 2);
    varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

pub(crate) fn text(buf: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        bytes(buf, field, s.as_bytes());
    }
}

pub(crate) fn fixed64(buf: &mut Vec<u8>, field: u32, v: u64) {
    tag(buf, field, 1);
    buf.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn number(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        tag(buf, field, 0);
        varint(buf, v);
    }
}

pub(crate) fn message(buf: &mut Vec<u8>, field: u32, fill: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    fill(&mut inner);
    bytes(buf, field, &inner);
}

/// `AnyValue` is a protobuf `oneof`, so empty strings, `false`, and `0` must still be emitted.
pub(crate) fn any(buf: &mut Vec<u8>, v: &Val) {
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

pub(crate) fn key_value(buf: &mut Vec<u8>, field: u32, k: &str, v: &Val) {
    message(buf, field, |kv| {
        text(kv, 1, k);
        message(kv, 2, |a| any(a, v));
    });
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
            "negative int64 encodes as 10-byte varint"
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
        assert_eq!(e, [0x12, 0x00], "empty submessage still emits length-0 tag");
    }
}

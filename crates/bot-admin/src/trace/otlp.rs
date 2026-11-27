//! Dependency-free OTLP/HTTP protobuf encoder for `ExportTraceServiceRequest` following opentelemetry-proto field numbers.

use super::{Key, SpanIds, SpanKind, Val};
use crate::obs::proto::{bytes, fixed64, key_value, message, number, text};

pub(super) type EventOut = (String, u64, Vec<(Key, Val)>);

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

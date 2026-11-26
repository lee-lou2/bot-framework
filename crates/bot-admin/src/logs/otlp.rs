//! 정책·가림을 거친 로그(`Out`)를 OTLP/HTTP protobuf(`ExportLogsServiceRequest`)로 인코딩한다(외부 의존 없음).
//! 필드 번호는 opentelemetry-proto의 logs·common·resource·collector 정의를 따른다.

use super::LogLevel;
use crate::obs::proto::{any, bytes, fixed64, key_value, message, number, text};
use bot_core::trace::Val;

/// 내보낼 로그 한 건(가림·길이 제한 적용 완료).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Out {
    pub timestamp_nano: u64,
    pub level: LogLevel,
    pub body: String,
    pub attrs: Vec<(String, String)>,
    pub dropped: u32,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
}

const fn severity_number(level: LogLevel) -> u64 {
    match level {
        LogLevel::Debug => 5,
        LogLevel::Info => 9,
        LogLevel::Warn => 13,
        LogLevel::Error => 17,
    }
}

fn log_record(buf: &mut Vec<u8>, rec: &Out) {
    fixed64(buf, 1, rec.timestamp_nano);
    number(buf, 2, severity_number(rec.level));
    text(buf, 3, rec.level.severity_text());
    message(buf, 5, |v| any(v, &Val::Str(rec.body.clone())));
    for (k, v) in &rec.attrs {
        key_value(buf, 6, k, &Val::Str(v.clone()));
    }
    number(buf, 7, u64::from(rec.dropped));
    if let Some(trace_id) = &rec.trace_id {
        bytes(buf, 9, trace_id);
    }
    if let Some(span_id) = &rec.span_id {
        bytes(buf, 10, span_id);
    }
    fixed64(buf, 11, rec.timestamp_nano);
}

/// 한 번의 OTLP 로그 내보내기 요청(`ExportLogsServiceRequest`).
pub(super) fn request(resource: &[(&str, Val)], scope: (&str, &str), records: &[Out]) -> Vec<u8> {
    let mut out = Vec::new();
    message(&mut out, 1, |rl| {
        message(rl, 1, |r| {
            for (k, v) in resource {
                key_value(r, 1, k, v);
            }
        });
        message(rl, 2, |sl| {
            message(sl, 1, |sc| {
                text(sc, 1, scope.0);
                text(sc, 2, scope.1);
            });
            for rec in records {
                message(sl, 2, |lr| log_record(lr, rec));
            }
        });
    });
    out
}

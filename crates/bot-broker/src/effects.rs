//! 단계 원장 콜백 채널(`effect.request`·`effect.status`). 여러 번 쓰는 도구만 쓴다(`effects: true`).
//!
//! UTF-8 NDJSON 한 줄이 요청 또는 응답 하나다(64 KiB 상한). 채널이 원래 호출·도구와 시도 세대를 식별하므로
//! 도구는 주체나 권한을 지정하지 못한다(프레임의 식별자는 읽지 않는다). 세대 확인은 원장(`EffectJournal`)이 메시지마다 한다.
//! 오류 응답에는 invalid·denied·conflict·stale·unavailable 분류만 싣는다.
//!
//! `effect.status`는 두 양식이다. **효과 ID 양식**(`effect` [+ `outcome`…])은 단계 하나를 ID로 읽거나 결과를 기록한다. **조회 양식**(`step`+`args`)은
//! `effect.request`와 같은 정규 인자·지문으로 이 호출의 그 단계만 읽는 **조회 전용**이다: 단계·영수증·감사를 쓰지 않고 점유하지 않으며 전송을 허가하지 않는다
//! (`send`는 늘 `false`, 시작만 한 단계도 보류로 바꾸지 않는다). 단계가 없으면 `"state":"none"`, 있으면 `started`·`held`·`rejected`·`confirmed`와 `effect`,
//! 같은 단계의 인자 지문이 다르면 `conflict`다. 수신처 영수증(`receipt`)은 `confirmed`에만 싣는다(보류 사유 코드는 싣지 않는다). 운영자가 «미실행»을 입증한 보류
//! 단계에만 `"resolution":"not_executed"`를 싣는다 — 이 판정은 운영자만 정하고 도구는 어떤 프레임으로도 정하지 못한다(`resolution` 키가 든 프레임은 `invalid`다).
//! 그때(또는 `none`일 때) 도구는 사전 확인 뒤 같은 단계로 `effect.request`를 보내고 `send:true`일 때만 쓴다. 두 양식을 섞은 프레임(`step`과 `effect`·`outcome`·`receipt`·
//! `reason`)은 `invalid`다.
//!
//! **자격 검사.** 도구가 보고하는 글(영수증·보류 사유)은 원장과 감사에 남으므로, 최종 결과와 같은 검사(`guard::blocks_output`: 토큰 모양과 이 호출이 쓰는 선언
//! 자격·프록시 암호의 바이트)를 저장하기 **전에** 한다. 걸리면 원문을 저장하지 않고 그 단계를 고정 사유(`receipt_credential`·`reason_credential`)로 **보류**한다
//! (도구에는 `held`와 그 고정 사유만 돌려준다). 단계 이름이 자격이면 프레임을 `invalid`로 거절한다(`effect.start` 감사에 이름이 남는다).

use crate::{
    canon::canonicalize,
    guard::blocks_output,
    json::parse,
    lines::read_line,
    sandbox::{BoxRead, BoxWrite},
    text::json_quote,
};
use bot_core::{
    command::*,
    id::*,
    port::{EffectJournal, Error},
    state::{EffectResolution, EffectState},
};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    sync::watch,
};

/// 프레임 한 줄의 상한.
pub const MAX_FRAME: usize = 64 * 1024;

/// 확정 보고의 영수증에 알려진 자격이 섞여 원문을 저장하지 않고 보류한 단계의 고정 사유.
pub const RECEIPT_CREDENTIAL: &str = "receipt_credential";
/// 보류 보고의 사유에 알려진 자격이 섞여 원문을 저장하지 않고 보류한 단계의 고정 사유.
pub const REASON_CREDENTIAL: &str = "reason_credential";

pub struct EffectChannel {
    journal: Arc<dyn EffectJournal>,
    attempt: Attempt,
    call: CallId,
    /// 이 채널을 연 호출의 도구. 조회가 호출 ID와 도구가 맞는 호출인지 원장에서 확인할 때 쓴다(프레임이 정하지 못한다).
    tool: ToolId,
    /// 이 호출이 쓰는 자격의 바이트(선언 자격 값·프록시 암호). 보고하는 글에서 이 값이나 토큰 모양이 보이면 저장하지 않는다.
    credentials: Vec<Vec<u8>>,
}

fn state_name(state: EffectState) -> &'static str {
    match state {
        EffectState::Started => "started",
        EffectState::Confirmed => "confirmed",
        EffectState::Rejected => "rejected",
        EffectState::Held => "held",
    }
}

fn error_name(error: &Error) -> &'static str {
    match error {
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Unavailable => "unavailable",
        Error::Invalid | Error::Missing => "invalid",
    }
}

fn failure(id: Option<&str>, class: &str) -> String {
    match id {
        Some(id) => format!(
            "{{\"v\":1,\"id\":{},\"error\":{}}}",
            json_quote(id),
            json_quote(class)
        ),
        None => format!("{{\"v\":1,\"error\":{}}}", json_quote(class)),
    }
}

fn answer(id: &str, status: &EffectStatus, send: bool, with_effect: bool) -> String {
    let mut out = format!(
        "{{\"v\":1,\"id\":{},\"state\":{}",
        json_quote(id),
        json_quote(state_name(status.state))
    );
    if with_effect {
        out.push_str(&format!(",\"effect\":{}", json_quote(&status.effect.0)));
    }
    if let Some(receipt) = &status.receipt {
        out.push_str(&format!(",\"receipt\":{}", json_quote(receipt)));
    }
    out.push_str(&format!(",\"send\":{send}}}"));
    out
}

/// 조회(`step`+`args`) 응답. 수신처 영수증은 `confirmed`에만 싣고, 운영자가 미실행을 입증한 **보류** 단계에만 `resolution`을 싣는다(`send`는 늘 `false`).
fn lookup_answer(id: &str, status: &EffectStatus) -> String {
    let mut out = format!(
        "{{\"v\":1,\"id\":{},\"state\":{},\"effect\":{}",
        json_quote(id),
        json_quote(state_name(status.state)),
        json_quote(&status.effect.0)
    );
    if status.state == EffectState::Confirmed
        && let Some(receipt) = &status.receipt
    {
        out.push_str(&format!(",\"receipt\":{}", json_quote(receipt)));
    }
    if status.state == EffectState::Held && status.resolution == Some(EffectResolution::NotExecuted)
    {
        out.push_str(",\"resolution\":\"not_executed\"");
    }
    out.push_str(",\"send\":false}");
    out
}

/// 단계 이름: 소문자로 시작하는 63자 이내의 `[a-z0-9_-]`.
fn valid_step(step: &str) -> bool {
    let mut bytes = step.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && step.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn text<'a>(frame: &'a Map<String, Value>, key: &str, max: usize) -> Option<&'a str> {
    frame
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= max)
}

impl EffectChannel {
    pub fn new(
        journal: Arc<dyn EffectJournal>,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
    ) -> Self {
        Self {
            journal,
            attempt,
            call,
            tool,
            credentials: Vec::new(),
        }
    }

    /// 이 호출이 쓰는 자격의 바이트를 알려 준다. 도구가 보고하는 글에 이 값이 섞이면 원문을 저장하지 않는다(토큰 모양은 자격을 몰라도 늘 막는다).
    pub fn with_credentials(mut self, credentials: Vec<Vec<u8>>) -> Self {
        self.credentials = credentials;
        self
    }

    /// 이 글에 토큰 모양이나 이 호출의 자격이 섞였는가(최종 결과와 같은 검사).
    fn leaks(&self, text: &str) -> bool {
        blocks_output(text, &self.credentials)
    }

    /// 프레임 한 줄을 처리하고 응답 한 줄(개행 없음)을 돌려준다.
    pub async fn handle(&self, line: &[u8]) -> String {
        let Some(Value::Object(frame)) = std::str::from_utf8(line).ok().and_then(parse) else {
            return failure(None, "invalid");
        };
        let Some(id) = text(&frame, "id", 64) else {
            return failure(None, "invalid");
        };
        if frame.get("v").and_then(Value::as_u64) != Some(1) {
            return failure(Some(id), "invalid");
        }
        // 상태 판정(`resolution`)은 운영자만 정한다: 도구는 어떤 프레임으로도 주입하지 못한다
        if frame.contains_key("resolution") {
            return failure(Some(id), "invalid");
        }
        let result = match frame.get("method").and_then(Value::as_str) {
            Some("effect.request") => self.request(id, &frame).await,
            Some("effect.status") => self.status(id, &frame).await,
            _ => Err(Error::Invalid),
        };
        result.unwrap_or_else(|error| failure(Some(id), error_name(&error)))
    }

    async fn request(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        let step = text(frame, "step", 63)
            .filter(|s| valid_step(s) && !self.leaks(s))
            .ok_or(Error::Invalid)?;
        let args = frame
            .get("args")
            .filter(|a| a.is_object())
            .ok_or(Error::Invalid)?;
        let args = JsonPayload(canonicalize(args).ok_or(Error::Invalid)?);
        let permit = self
            .journal
            .request(EffectRequest {
                attempt: self.attempt.clone(),
                call: self.call.clone(),
                step: step.to_owned(),
                args,
            })
            .await?;
        Ok(answer(id, &permit.status, permit.send, true))
    }

    /// 조회 양식: 이 호출의 그 단계만 읽는다(기록·감사·점유·전송 허가 0). 양식을 섞은 프레임은 어느 쪽도 믿지 않는다.
    async fn lookup(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        if ["effect", "outcome", "receipt", "reason"]
            .iter()
            .any(|key| frame.contains_key(*key))
        {
            return Err(Error::Invalid);
        }
        let step = text(frame, "step", 63)
            .filter(|s| valid_step(s) && !self.leaks(s))
            .ok_or(Error::Invalid)?;
        let args = frame
            .get("args")
            .filter(|a| a.is_object())
            .ok_or(Error::Invalid)?;
        let args = JsonPayload(canonicalize(args).ok_or(Error::Invalid)?);
        let found = self
            .journal
            .lookup(
                EffectRequest {
                    attempt: self.attempt.clone(),
                    call: self.call.clone(),
                    step: step.to_owned(),
                    args,
                },
                self.tool.clone(),
            )
            .await?;
        Ok(match found {
            None => format!(
                "{{\"v\":1,\"id\":{},\"state\":\"none\",\"send\":false}}",
                json_quote(id)
            ),
            Some(status) => lookup_answer(id, &status),
        })
    }

    async fn status(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        if frame.contains_key("step") || frame.contains_key("args") {
            return self.lookup(id, frame).await;
        }
        let effect = EffectId(text(frame, "effect", 64).ok_or(Error::Invalid)?.to_owned());
        let Some(outcome) = frame.get("outcome") else {
            let status = self
                .journal
                .status(self.attempt.clone(), self.call.clone(), effect)
                .await?;
            return Ok(answer(id, &status, false, false));
        };
        let receipt = text(frame, "receipt", 512).map(str::to_owned);
        let reason = text(frame, "reason", 64)
            .filter(|r| {
                r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .map(str::to_owned);
        let (state, receipt) = match outcome.as_str() {
            // 확정에는 수신처 영수증이 있어야 한다. 영수증 없이 불명을 보냈다고 하지 못한다.
            Some("confirmed") => (EffectState::Confirmed, Some(receipt.ok_or(Error::Invalid)?)),
            Some("rejected") => (EffectState::Rejected, None),
            // 보류 사유 코드는 원장의 영수증 칸에 싣는다.
            Some("held") => (EffectState::Held, Some(reason.ok_or(Error::Invalid)?)),
            _ => return Err(Error::Invalid),
        };
        // 저장하기 전에 자격 검사: 알려진 자격이 섞인 영수증·보류 사유는 원문을 저장하지 않고 고정 사유로 보류한다
        let (state, receipt) = match (state, receipt) {
            (EffectState::Confirmed, Some(text)) if self.leaks(&text) => {
                (EffectState::Held, Some(RECEIPT_CREDENTIAL.to_owned()))
            }
            (EffectState::Held, Some(text)) if self.leaks(&text) => {
                (EffectState::Held, Some(REASON_CREDENTIAL.to_owned()))
            }
            other => other,
        };
        let status = EffectStatus {
            effect,
            state,
            resolution: None,
            receipt,
        };
        self.journal
            .record(self.attempt.clone(), self.call.clone(), status.clone())
            .await?;
        Ok(answer(id, &status, false, false))
    }

    /// `stop`이 오거나 연결이 끝날 때까지 프레임을 처리한다. 처리 중인 프레임은 끝까지 마친다.
    pub async fn serve(self, read: BoxRead, mut write: BoxWrite, mut stop: watch::Receiver<bool>) {
        let mut read = BufReader::new(read);
        loop {
            let line = tokio::select! {
                biased;
                _ = stop.changed() => return,
                line = read_line(&mut read, MAX_FRAME) => line,
            };
            let reply = match line {
                Ok(Some(line)) => self.handle(&line).await,
                Ok(None) => return,
                Err(_) => {
                    let _ = write
                        .write_all(format!("{}\n", failure(None, "invalid")).as_bytes())
                        .await;
                    return;
                }
            };
            if write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
                || write.flush().await.is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod secret_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod verdict_tests;

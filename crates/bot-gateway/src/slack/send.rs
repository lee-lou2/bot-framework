//! Slack 회신 발송. 메시지 하나(본문 markdown 블록 + 승인 카드)는 `chat.postMessage` 한 번이고, 첨부는 외부 업로드
//! 흐름(주소 받기 → 바이트 → 완료)이며 완료 호출만이 게시한다. 결과 분류:
//! 실행 확정은 `Sent`, 요청 전 연결 실패·Slack의 429·봇 토큰 거절(`invalid_auth`·`token_revoked`·`token_expired`)은 `NotSent`,
//! Slack가 이유를 밝힌 거절은 `Rejected`, 시간 초과·5xx 같은 불명은 `Held`다. 토큰 거절은 인증 단계에서 걸러져 아무것도 실행되지 않았고,
//! 토큰을 바꾸는 동안 옛 토큰이 받는 오류라서 영구 거절이 아니다 — 기억한 토큰을 버려(`Credentials::call`) 다시 시도가 자격을 새로 읽게 한다.
//! 블록 형식 거절(`invalid_blocks` 등)만 글자판으로 한 번 내려 다시 시도한다
//! (Slack가 아무것도 올리지 않았다고 확정한 경우라서 중복이 생기지 않는다).
use super::{
    api::{Body, Raw, SlackApi},
    blocks::{
        PLAIN_LIMIT, card_blocks, card_fits, card_text, fits_block, markdown_block, neutralize,
    },
    creds::Credentials,
};
use crate::{
    delivery::{Delivery, Sender},
    reply::ReplyBody,
    schedule_routes::{is_ts, split_route},
};
use bot_core::{
    intake::{NotSentReason, Outgoing},
    profile::{Adapter, Snapshot},
    state::ConversationKind,
};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};

/// Slack가 아무것도 올리지 않았다고 확정하는 형식 오류. 글자판으로 내려 다시 시도해도 된다.
const FORMAT_ERRORS: [&str; 3] = [
    "invalid_blocks",
    "invalid_blocks_format",
    "msg_blocks_too_long",
];
/// `ok:false`여도 서버 쪽 사정이라 결과를 알 수 없는 코드
const UNKNOWN_ERRORS: [&str; 5] = [
    "internal_error",
    "fatal_error",
    "service_unavailable",
    "request_timeout",
    "timeout",
];

pub struct SlackSender {
    api: SlackApi,
    creds: Arc<Credentials>,
}

impl SlackSender {
    pub fn new(api: SlackApi, creds: Arc<Credentials>) -> Self {
        Self { api, creds }
    }
}

enum Step {
    Done(Delivery),
    /// 형식 거절(글자판으로 내려 다시 시도할 수 있다)
    Format,
}

/// 성공 응답이 «메시지가 잘렸다»고 경고했는가(`response_metadata.warnings`). 이미 일부가 게시됐으니 확정도 거절도 아니다 — 자동으로 다시 보내지 않고
/// 불명으로 둔다. 관련 없는 경고는 정상 완료를 막지 않는다.
fn truncated(value: &Value) -> bool {
    value["response_metadata"]["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|w| w == "message_truncated"))
}

fn step(raw: Raw, done: impl FnOnce(&Value) -> Option<String>) -> Step {
    match raw {
        Raw::Ok(value) if truncated(&value) => Step::Done(Delivery::Held),
        Raw::Ok(value) => match done(&value) {
            Some(receipt) => Step::Done(Delivery::Sent { receipt }),
            // 올라갔는지 영수증이 모자라면 불명이다
            None => Step::Done(Delivery::Held),
        },
        Raw::Error(code) if FORMAT_ERRORS.contains(&code.as_str()) => Step::Format,
        Raw::Error(code) if UNKNOWN_ERRORS.contains(&code.as_str()) => Step::Done(Delivery::Held),
        Raw::Error(_) => Step::Done(Delivery::Rejected),
        // 요청을 쓰기 전의 연결 실패와, 인증 단계에서 걸러진 토큰 거절: 둘 다 아무것도 실행되지 않았다
        Raw::BeforeWrite | Raw::TokenRejected => Step::Done(Delivery::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None,
        }),
        Raw::RateLimited(wait) => Step::Done(Delivery::NotSent {
            reason: NotSentReason::RateLimited,
            retry_after: Some(wait),
        }),
        Raw::Unknown => Step::Done(Delivery::Held),
    }
}

/// 본문을 자르지 않고 보낼 수 있는 모양인가: 승인 카드는 첨부와 함께, 또 블록 한도를 넘는 본문과 함께 보내지 않고, 글자판 한도를 넘는 본문은 보낼 곳이 없다.
fn sendable(body: &ReplyBody) -> bool {
    if body.approval.is_some() && !body.attachments.is_empty() {
        return false;
    }
    match body.text.as_deref() {
        Some(text) if !fits_block(text) => {
            body.approval.is_none() && neutralize(text).chars().count() <= PLAIN_LIMIT
        }
        _ => true,
    }
}

/// 이 게시 응답으로 «올라갔다»를 확정할 수 있는가: **Slack의 응답이 말한** 채널이 보낸 곳과 같고 `ts`가 시각 모양이며 잘림 경고가 없을 때만 `(채널, ts)`다. 그렇지 않으면
/// 올라간 곳을 확인할 수 없다(`None` → 불명). 운영 발송과 시험 도우미(가짜 Slack의 `Call::posted`)가 같은 판정을 쓴다.
#[doc(hidden)]
pub fn confirms_post(channel: &str, reply: &Value) -> Option<(String, String)> {
    if truncated(reply) {
        return None;
    }
    let ts = reply["ts"].as_str().filter(|ts| is_ts(ts))?;
    (reply["channel"].as_str()? == channel).then(|| (channel.to_owned(), ts.to_owned()))
}

/// 게시 영수증(`채널:ts`)
fn receipt_of(channel: &str, value: &Value) -> Option<String> {
    confirms_post(channel, value).map(|(channel, ts)| format!("{channel}:{ts}"))
}

/// 업로드 주소 응답에서 `(주소, 파일 ID)`: 둘 다 비어 있지 않은 문자열이어야 한다. 이 단계는 아무것도 게시하지 않으므로 응답이 이 모양이 아니면(누락·숫자·빈 값) 완료 요청을 보내기 전에
/// 거절한다 — 불명(`Held`)이 아니다.
fn upload_slot(value: &Value) -> Option<(String, String)> {
    let url = value["upload_url"].as_str().filter(|url| !url.is_empty())?;
    let id = value["file_id"].as_str().filter(|id| !id.is_empty())?;
    Some((url.to_owned(), id.to_owned()))
}

/// 업로드 완료 영수증: 올린 파일들의 ID가 비어 있지 않고 서로 달라야 하고, 응답의 **모든 항목**이 문자열 ID를 가져야 하며(하나라도 ID가 없거나 문자열이 아니면
/// 확인할 수 없다 — 버리고 세지 않는다), 그 ID들(개수 포함)이 올린 것과 정확히 같아야 올라간 것을 확인한 것이다. 아니면 `None` → 불명.
/// (올린 ID는 비어 있지 않으므로 응답의 빈 ID는 같을 수 없다.)
fn files_receipt(channel: &str, value: &Value, requested: &[&str]) -> Option<String> {
    if !unique_ids(requested) {
        return None;
    }
    let mut returned = value["files"]
        .as_array()?
        .iter()
        .map(|file| file["id"].as_str())
        .collect::<Option<Vec<&str>>>()?;
    returned.sort_unstable();
    let mut wanted = requested.to_vec();
    wanted.sort_unstable();
    (returned == wanted).then(|| format!("{channel}:{}", requested.join(",")))
}

/// 비어 있지 않고 서로 다른 ID들인가
fn unique_ids(ids: &[&str]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    ids.iter().all(|id| !id.is_empty() && seen.insert(*id))
}

impl SlackSender {
    fn workspace<'a>(&self, snapshot: &'a Snapshot, item: &Outgoing) -> Option<&'a str> {
        match &snapshot.bindings.get(&item.binding)?.source {
            Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace } => {
                Some(workspace)
            }
            _ => None,
        }
    }

    async fn post(
        &self,
        token: &str,
        channel: &str,
        thread: Option<&str>,
        body: &ReplyBody,
    ) -> Delivery {
        let card = body.approval.as_ref();
        // 본문은 자르지 않는다: markdown 블록 하나에 다 들어가면 블록으로, 아니면 글자판 메시지 하나로 보낸다(카드와는 함께 못 보낸다 — 보내지 않는다)
        if body.text.as_deref().is_some_and(|text| !fits_block(text)) {
            return match (card, body.text.as_deref()) {
                (None, Some(text)) => {
                    self.post_plain(token, channel, thread, &neutralize(text))
                        .await
                }
                _ => Delivery::Rejected,
            };
        }
        let mut blocks: Vec<Value> = body.text.iter().map(|t| markdown_block(t)).collect();
        match card {
            Some(card) if card_fits(card) => blocks.extend(card_blocks(card)),
            Some(_) => {
                // 잘라서 보여 줄 수 없는 카드는 보내지 않고 인증된 상세 화면으로 안내한다
                let notice = self.notice();
                blocks.push(
                    json!({"type": "section", "text": {"type": "plain_text", "text": notice}}),
                );
            }
            None => {}
        }
        let mut message = json!({"channel": channel, "blocks": blocks, "text": self.summary(body),
            "unfurl_links": false, "unfurl_media": false});
        if let Some(thread) = thread {
            message["thread_ts"] = json!(thread);
        }
        let raw = self
            .creds
            .call("chat.postMessage", token, Body::Json(&message))
            .await;
        match step(raw, |v| receipt_of(channel, v)) {
            Step::Done(delivery) => delivery,
            // 카드는 버튼 없이 의미가 없어 글자판으로 내리지 않는다
            Step::Format if card.is_some() => Delivery::Rejected,
            // 블록 형식 거절 뒤에는 **전체** 본문을 글자판으로 한 번 보낸다(잘라서 완료로 기록하지 않는다)
            Step::Format => match body.text.as_deref() {
                Some(text) => {
                    self.post_plain(token, channel, thread, &neutralize(text))
                        .await
                }
                None => Delivery::Rejected,
            },
        }
    }

    /// 글자판 메시지 한 개: 본문을 자르지 않는다. 한도(`PLAIN_LIMIT`)를 넘어 다 보낼 수 없으면 아무것도 보내지 않고 거절한다.
    async fn post_plain(
        &self,
        token: &str,
        channel: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Delivery {
        if text.chars().count() > PLAIN_LIMIT {
            return Delivery::Rejected;
        }
        let mut plain = json!({"channel": channel, "text": text,
            "unfurl_links": false, "unfurl_media": false});
        if let Some(thread) = thread {
            plain["thread_ts"] = json!(thread);
        }
        let raw = self
            .creds
            .call("chat.postMessage", token, Body::Json(&plain))
            .await;
        match step(raw, |v| receipt_of(channel, v)) {
            Step::Done(delivery) => delivery,
            Step::Format => Delivery::Rejected,
        }
    }

    /// 알림·화면 읽기 프로그램이 읽는 최상위 글: **본문 전체**와 승인 카드의 필수 내용(제목·줄, 카드를 담지 못하면 안내)을 모두 담는다. 블록을 못 보는 사람도 무엇을
    /// 승인하는지 알 수 있어야 한다. 알림 멘션은 글자 그대로 바꾼다(최상위 글은 멘션을 해석한다).
    fn summary(&self, body: &ReplyBody) -> String {
        let mut parts: Vec<String> = body.text.iter().map(|text| neutralize(text)).collect();
        match &body.approval {
            Some(card) if card_fits(card) => parts.push(neutralize(&card_text(card))),
            Some(_) => parts.push(self.notice()),
            None => {}
        }
        parts.join("\n\n")
    }

    fn notice(&self) -> String {
        match &self.creds.host().admin_url {
            Some(url) => format!("승인이 필요한 요청이에요. 내용이 길어 여기서는 보여 줄 수 없어요 — 관리 화면에서 확인하고 승인해 주세요: {url}"),
            None => "승인이 필요한 요청이에요. 내용이 길어 여기서는 보여 줄 수 없어요 — 관리 화면에서 확인하고 승인해 주세요.".to_owned(),
        }
    }

    /// 첨부: 주소 받기 → 바이트 → 완료. 게시는 완료 호출 한 번뿐이라 그 앞의 단계는 아무것도 올리지 않는다.
    async fn upload(
        &self,
        token: &str,
        channel: &str,
        thread: Option<&str>,
        body: &ReplyBody,
    ) -> Delivery {
        let mut files: Vec<Value> = Vec::new();
        for attachment in &body.attachments {
            let form = vec![
                ("filename", attachment.name.clone()),
                ("length", attachment.data.len().to_string()),
            ];
            let slot = self
                .creds
                .call("files.getUploadURLExternal", token, Body::Form(form))
                .await;
            let (url, id) = match slot {
                Raw::Ok(value) => match upload_slot(&value) {
                    Some(slot) => slot,
                    // 아직 아무것도 게시되지 않았다: 쓸 수 없는 주소·ID는 완료 요청 전에 거절한다
                    None => return Delivery::Rejected,
                },
                raw => match step(raw, |_| None) {
                    Step::Done(Delivery::Sent { .. }) | Step::Format => return Delivery::Rejected,
                    Step::Done(other) => return other,
                },
            };
            // 파일마다 고유 ID여야 완료 응답을 맞춰 볼 수 있다(아직 아무것도 게시되지 않았다)
            if files.iter().any(|f| f["id"] == id.as_str()) {
                return Delivery::Rejected;
            }
            match self.api.put_bytes(&url, attachment.data.clone()).await {
                Raw::Ok(_) => {}
                raw => match step(raw, |_| None) {
                    Step::Done(Delivery::Sent { .. }) | Step::Format => {
                        return Delivery::Rejected;
                    }
                    Step::Done(other) => return other,
                },
            }
            files.push(json!({"id": id, "title": attachment.name}));
        }
        let mut form = vec![
            ("files", Value::Array(files.clone()).to_string()),
            ("channel_id", channel.to_owned()),
        ];
        if let Some(thread) = thread {
            form.push(("thread_ts", thread.to_owned()));
        }
        if let Some(text) = &body.text {
            form.push(("initial_comment", neutralize(text)));
        }
        let ids: Vec<&str> = files.iter().filter_map(|f| f["id"].as_str()).collect();
        let raw = self
            .creds
            .call("files.completeUploadExternal", token, Body::Form(form))
            .await;
        match step(raw, |v| files_receipt(channel, v, &ids)) {
            Step::Done(delivery) => delivery,
            // 완료 호출의 형식 거절은 아무것도 게시되지 않았다는 확정이다
            Step::Format => Delivery::Rejected,
        }
    }
}

impl Sender for SlackSender {
    fn kinds(&self) -> Vec<ConversationKind> {
        vec![ConversationKind::Channel, ConversationKind::Direct]
    }

    /// 바인딩이 받을 수 있는 곳에만 보낸다: 채널 바인딩은 **지금 등록부의** 그 채널로(원래 대화의 채널 ID 또는 고정된 정규 경로
    /// `팀ID/채널ID` — 팀과 채널이 모두 지금 등록부와 같아야 한다), DM 바인딩은 DM 채널로만. 스레드는 `ts` 모양이어야 한다.
    fn allows(&self, snapshot: &Snapshot, item: &Outgoing) -> bool {
        let Some(binding) = snapshot.bindings.get(&item.binding) else {
            return false;
        };
        let destination = &item.destination;
        let place = match (&binding.source, &destination.kind) {
            (Adapter::SlackChannel { workspace, channel }, ConversationKind::Channel) => {
                let host = self.creds.host();
                host.slack
                    .workspaces
                    .get(workspace)
                    .and_then(|ws| Some((ws, ws.channels.get(channel)?)))
                    .is_some_and(|(ws, id)| match split_route(&destination.id) {
                        Some((team, chosen)) => team == ws.team_id && chosen == id,
                        None => *id == destination.id,
                    })
            }
            (Adapter::SlackDm { .. }, ConversationKind::Direct) => destination.id.starts_with('D'),
            _ => false,
        };
        place && destination.thread.as_deref().is_none_or(is_ts)
    }

    fn send<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        item: &'a Outgoing,
        body: &'a ReplyBody,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + 'a>> {
        Box::pin(async move {
            // 보내기 직전에 한 번 더: 등록부에 없거나 팀이 다르면 요청 자체를 만들지 않는다
            if !self.allows(snapshot, item) {
                return Delivery::Rejected;
            }
            let Some(workspace) = self.workspace(snapshot, item) else {
                return Delivery::Rejected;
            };
            // 이 모양은 어떤 방법으로도 다 보낼 수 없다: 자격을 만지기 전에 보내지 않고 거절한다
            if !sendable(body) {
                return Delivery::Rejected;
            }
            let Some(token) = self.creds.bot_token(&item.key.profile, workspace).await else {
                // 자격을 읽지 못했다 — 아무것도 보내지 않았다
                return Delivery::NotSent {
                    reason: NotSentReason::BeforeWrite,
                    retry_after: None,
                };
            };
            // Slack API에는 채널 ID만 간다(정규 경로의 팀은 위에서 대조를 마쳤다)
            let channel = split_route(&item.destination.id)
                .map_or(item.destination.id.as_str(), |(_, channel)| channel);
            let thread = item.destination.thread.as_deref();
            match (body.attachments.is_empty(), &body.approval) {
                (true, _) => self.post(&token, channel, thread, body).await,
                (false, None) => self.upload(&token, channel, thread, body).await,
                // 승인 카드와 첨부는 한 회신에 함께 보내지 않는다(`sendable`이 이미 걸렀다)
                (false, Some(_)) => Delivery::Rejected,
            }
        })
    }
}

mod card;
pub use card::{CardPost, slack_user};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn delivery(raw: Raw) -> Delivery {
        match step(raw, |v| receipt_of("C1", v)) {
            Step::Done(delivery) => delivery,
            Step::Format => Delivery::Rejected,
        }
    }

    #[test]
    fn outcomes_follow_the_documented_classification() {
        assert_eq!(
            delivery(Raw::Ok(json!({"channel": "C1", "ts": "1.5"}))),
            Delivery::Sent {
                receipt: "C1:1.5".into()
            }
        );
        // ok인데 영수증을 확인할 수 없으면 올라갔는지 알 수 없다: ts가 없거나 모양이 아니거나, 응답의 채널이 없거나 보낸 곳과 다르다
        assert_eq!(delivery(Raw::Ok(json!({}))), Delivery::Held);
        for reply in [
            json!({"channel": "C1", "ts": ""}),
            json!({"channel": "C1", "ts": "later"}),
            json!({"channel": "C1"}),
            json!({"ts": "1.5"}),
            json!({"channel": "C2", "ts": "1.5"}),
            json!({"channel": "c1", "ts": "1.5"}),
            json!({"channel": 7, "ts": "1.5"}),
        ] {
            assert_eq!(delivery(Raw::Ok(reply.clone())), Delivery::Held, "{reply}");
        }
        assert_eq!(
            delivery(Raw::Error("channel_not_found".into())),
            Delivery::Rejected
        );
        assert_eq!(
            delivery(Raw::Error("not_in_channel".into())),
            Delivery::Rejected
        );
        assert_eq!(
            delivery(Raw::Error("internal_error".into())),
            Delivery::Held
        );
        assert_eq!(delivery(Raw::Unknown), Delivery::Held);
        // 토큰을 Slack가 거절했다: 아무것도 실행되지 않았다 — 영구 거절이 아니라 다시 시도할 수 있는 미실행이다
        assert_eq!(
            delivery(Raw::TokenRejected),
            Delivery::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None
            }
        );
        assert_eq!(
            delivery(Raw::BeforeWrite),
            Delivery::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None
            }
        );
        assert_eq!(
            delivery(Raw::RateLimited(Duration::from_secs(9))),
            Delivery::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(Duration::from_secs(9))
            }
        );
        for code in FORMAT_ERRORS {
            assert!(
                matches!(step(Raw::Error(code.into()), |_| None), Step::Format),
                "{code}"
            );
        }
    }

    #[test]
    fn thread_timestamps_must_look_like_slack_ts() {
        for ok in ["1700000000.000100", "1.1", "123456789012.123456789"] {
            assert!(is_ts(ok), "{ok}");
        }
        for bad in [
            "",
            "1",
            ".1",
            "1.",
            "a.1",
            "1.b",
            "1.2.3",
            "1234567890123.1",
            "1.1234567890",
            "1 .1",
        ] {
            assert!(!is_ts(bad), "{bad}");
        }
    }

    #[test]
    fn an_upload_receipt_needs_exactly_the_files_we_sent_with_nonempty_unique_ids() {
        let receipt = |reply: Value, asked: &[&str]| files_receipt("C1", &reply, asked);
        assert_eq!(
            receipt(
                json!({"files": [{"id": "F2"}, {"id": "F1", "title": "x"}]}),
                &["F1", "F2"]
            )
            .as_deref(),
            Some("C1:F1,F2")
        );
        for (reply, asked) in [
            (json!({}), vec!["F1"]),
            (json!({"files": []}), vec!["F1"]),
            (json!({"files": [{"id": "F1"}]}), vec!["F1", "F2"]),
            (
                json!({"files": [{"id": "F1"}, {"id": "F9"}]}),
                vec!["F1", "F2"],
            ),
            (json!({"files": [{"title": "no id"}]}), vec!["F1"]),
            // 올린 것 외의 항목·ID 없는 항목·빈 ID·문자열이 아닌 ID·중복은 하나라도 있으면 확인할 수 없다
            (
                json!({"files": [{"id": "F1"}, {"id": "F2"}, {"id": "F3"}]}),
                vec!["F1", "F2"],
            ),
            (
                json!({"files": [{"id": "F1"}, {"title": "extra"}]}),
                vec!["F1", "F2"],
            ),
            (json!({"files": [{"id": "F1"}, {}]}), vec!["F1"]),
            (json!({"files": [{"id": ""}]}), vec![""]),
            (
                json!({"files": [{"id": "F1"}, {"id": "F1"}]}),
                vec!["F1", "F1"],
            ),
            (
                json!({"files": [{"id": "F1"}, {"id": "F1"}]}),
                vec!["F1", "F2"],
            ),
            (json!({"files": [{"id": 1}]}), vec!["F1"]),
        ] {
            assert_eq!(receipt(reply.clone(), &asked), None, "{reply} {asked:?}");
        }
    }

    #[test]
    fn only_the_truncation_warning_holds_a_successful_post() {
        let reply = |warnings: Value| json!({"ok": true, "channel": "C1", "ts": "1.5", "response_metadata": {"warnings": warnings}});
        assert!(truncated(&reply(json!(["message_truncated"]))));
        assert!(truncated(&reply(json!([
            "missing_charset",
            "message_truncated"
        ]))));
        assert!(!truncated(&reply(json!(["missing_charset"]))));
        assert!(!truncated(&reply(json!([]))));
        assert!(!truncated(
            &json!({"ok": true, "channel": "C1", "ts": "1.5"})
        ));
        assert!(!truncated(&reply(json!("message_truncated"))));
    }
}

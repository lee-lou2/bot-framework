//! 인증된 설치(소켓)가 받은 Slack 이벤트 처리: 글은 `Ingress`로, 승인 버튼은 `Broker::decide`로.
//! 소켓 밖 프로필로 폴백하지 않는다 — 이 설치의 팀·프로필과 맞지 않는 이벤트는 받았다는 표시만 한다.
use super::{
    SlackGateway,
    api::Body,
    blocks::{APPROVE, DENY, resolved_blocks},
    cards::is_the_card,
    events::SlackAdmission,
    frame::{Button, Frame, Message, StopRequested},
    socket::Installation,
    ux::{STOP_CONFIRM, STOP_CONTINUE, StopMention, is_stop_phrase},
};
use crate::{
    entrance::Real,
    identity,
    inbound::{Event, valid_request_id},
    observe,
    reply::parse_card,
};
use bot_core::{
    id::ApprovalId, ledger::ApprovalState, port::Error, slack::SourceKind, state::ConversationKind,
    trace::now_nano,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// 글에서 봇 멘션(`<@봇>` 또는 `<@봇|이름>`)을 뗀다. 다른 사람의 멘션은 그대로 둔다.
pub fn strip_mention(text: &str, bot_user: &str) -> String {
    let open = format!("<@{bot_user}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        // `<@U1>` 또는 `<@U1|name>` 꼴만 멘션이다(`<@U12>`처럼 ID가 더 이어지면 다른 사람이다)
        let tail = after.strip_prefix('>').or_else(|| {
            after
                .strip_prefix('|')
                .and_then(|named| named.find('>').map(|end| &named[end + 1..]))
        });
        match tail {
            Some(tail) => {
                out.push_str(&rest[..at]);
                rest = tail;
            }
            None => {
                out.push_str(&rest[..at + open.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// 글에 봇 멘션(`<@봇>` 또는 `<@봇|이름>`)이 있는가(`strip_mention`이 떼는 것과 같은 모양).
pub fn mentions_bot(text: &str, bot_user: &str) -> bool {
    let open = format!("<@{bot_user}");
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        if after.starts_with('>')
            || after
                .strip_prefix('|')
                .is_some_and(|named| named.contains('>'))
        {
            return true;
        }
        rest = after;
    }
    false
}

const RETRY_NOTICE: &str = "지금은 처리하지 못했어요 — 잠시 뒤 다시 눌러 주세요";

/// 버튼 클릭 처리 결과
enum Click {
    /// 이 설치와 무관하거나 모르는 버튼 — 조용히 받았다고만 한다
    Ignored,
    /// 판정했거나 영구적으로 처리할 수 없는 클릭(계측용 결과 이름)
    Done(&'static str),
    /// 일시 실패 — ack하지 않는다
    Retry,
}

fn approval_id(nonce: &str) -> ApprovalId {
    ApprovalId(nonce.to_owned())
}

fn status_line(state: ApprovalState) -> Option<&'static str> {
    match state {
        ApprovalState::Pending => None,
        ApprovalState::Approved | ApprovalState::Consumed => Some("✅ 승인됨"),
        ApprovalState::Denied => Some("⛔ 거절됨"),
        ApprovalState::Expired => Some("⌛ 만료됨"),
        ApprovalState::Superseded => Some("↻ 조건이 바뀌어 새 승인이 필요해요"),
        ApprovalState::Revoked => Some("⛔ 철회됨"),
    }
}

impl SlackGateway {
    /// 받은 프레임을 처리하고 «ack해도 되는가»를 돌려준다.
    pub async fn on_frame(&self, install: &Installation, frame: Frame) -> bool {
        self.on_frame_with_stop(install, frame, &self.shutdown)
            .await
    }

    pub(super) async fn on_frame_with_stop(
        &self,
        install: &Installation,
        frame: Frame,
        stop: &CancellationToken,
    ) -> bool {
        match frame {
            // 멘션 이벤트(채널)는 `events`에 따라 가르고, DM은 그대로 접수한다
            Frame::Message(message) if !message.direct => {
                self.on_channel_message(install, message.into(), stop).await
            }
            Frame::Message(message) => self.on_message(install, message, stop).await,
            Frame::Post(post) => self.on_post(install, post, stop).await,
            Frame::Reaction(reaction) => self.on_reaction(install, reaction).await,
            Frame::Button(button) => self.on_button(install, button).await,
            Frame::StopRequested(req) => self.on_stop_requested(install, req).await,
            _ => true,
        }
    }

    async fn on_stop_requested(&self, install: &Installation, req: StopRequested) -> bool {
        if req.team != install.team {
            return true;
        }
        let Some(ux) = &self.ux else { return true };
        let Ok(routes) = self.ingress.routes() else {
            return false;
        };
        let snapshot = routes.snapshot(&install.profile);
        ux.on_stop_requested(
            &self.ingress,
            install,
            snapshot.map(|v| &**v),
            &req.channel,
            &req.thread_ts,
            &req.user,
        )
        .await
    }

    /// 1:1 DM 글 접수(`on_frame_with_stop`이 `message.direct`만 여기로 보낸다).
    async fn on_message(
        &self,
        install: &Installation,
        message: Message,
        stop: &CancellationToken,
    ) -> bool {
        // 이 설치의 워크스페이스가 아니면 받았다는 표시만 한다
        if message.team != install.team {
            return true;
        }
        let real = Real::SlackDm {
            team: message.team.clone(),
        };
        let Ok(routes) = self.ingress.routes() else {
            return false; // 표를 만들 수 없으면 ack하지 않아 Slack이 다시 보낸다
        };
        // 소켓 밖 프로필로 폴백하지 않는다: 이 설치의 프로필 바인딩만 쓴다
        match routes.find(&real) {
            Some(route) if route.snapshot.id == install.profile => {}
            _ => return true,
        }
        let Some(principal) = identity::slack(&install.alias, &message.user) else {
            return true;
        };
        // 같은 글은 어떤 이벤트·재전달로 와도 같은 요청이다(채널 + 글 ts)
        let request = format!("{}.{}", message.channel, message.ts);
        if !valid_request_id(&request) {
            return true;
        }
        let stripped = strip_mention(&message.text, &install.bot_user);
        let thread_ts = message
            .thread_ts
            .clone()
            .unwrap_or_else(|| message.ts.clone());
        if is_stop_phrase(&stripped) {
            match self
                .try_stop_mention(
                    install,
                    &routes,
                    &message.channel,
                    &thread_ts,
                    &message.user,
                )
                .await
            {
                StopMention::Pass => {}
                StopMention::Handled => return true,
                StopMention::Retry => return false,
            }
        }
        self.admit_slack_message(
            install,
            &routes,
            SlackAdmission {
                real,
                principal,
                request,
                kind: ConversationKind::Direct,
                source_kind: SourceKind::Message,
                team: message.team,
                channel: message.channel,
                user: message.user,
                ts: message.ts,
                raw_thread_ts: message.thread_ts,
                thread_ts,
                files: message.files,
                event_id: message.event_id,
                body: json!({"text": stripped}),
                event: Event::Message,
                track_ux: true,
            },
            stop,
        )
        .await
    }

    /// 버튼 클릭 한 건. 돌려주는 값은 «ack해도 되는가»다 — 판정이 커밋됐거나 영구적으로 처리할 수 없는 클릭만 ack하고,
    /// 일시 실패(저장소·브로커 장애)는 ack하지 않는다. 판정을 커밋한 뒤 카드 갱신만 실패한 경우는 ack다.
    async fn on_button(&self, install: &Installation, button: Button) -> bool {
        if matches!(button.action_id.as_str(), STOP_CONFIRM | STOP_CONTINUE) {
            if button.team == install.team
                && let Some(ux) = &self.ux
            {
                let Ok(routes) = self.ingress.routes() else {
                    return false;
                };
                let snapshot = routes.snapshot(&install.profile);
                return ux
                    .on_stop_button(
                        &self.ingress,
                        install,
                        snapshot.map(|v| &**v),
                        &button,
                        button.action_id == STOP_CONFIRM,
                    )
                    .await;
            }
            return true;
        }
        let started = now_nano();
        let principal = identity::slack(&install.alias, &button.user);
        let click = self.decide_click(install, &button).await;
        let outcome = match &click {
            Click::Ignored => None,
            Click::Done(outcome) => Some(*outcome),
            Click::Retry => Some("unavailable"),
        };
        if let Some(outcome) = outcome {
            observe::record_approval(
                &**self.ingress.tracer(),
                &install.profile,
                principal.as_ref().map_or("", |p| p.0.as_str()),
                outcome,
                started,
            );
        }
        !matches!(click, Click::Retry)
    }

    async fn decide_click(&self, install: &Installation, button: &Button) -> Click {
        if button.team != install.team {
            return Click::Ignored;
        }
        let allow = match button.action_id.as_str() {
            APPROVE => true,
            DENY => false,
            _ => return Click::Ignored,
        };
        let nonce = button.value.clone();
        if nonce.is_empty() || nonce.len() > 200 || !nonce.bytes().all(|b| b.is_ascii_graphic()) {
            return Click::Ignored;
        }
        // 구판 프로세스는 자기 표로 승인 결정을 내리지 않는다 — 카드 상태를 그대로 두고 재전달에 맡긴다
        if self.ingress.verify_active().await.is_err() {
            self.notice(install, button, RETRY_NOTICE).await;
            return Click::Retry;
        }
        let Ok(routes) = self.ingress.routes() else {
            return Click::Retry;
        };
        let Some(principal) = identity::slack(&install.alias, &button.user) else {
            return Click::Ignored;
        };
        let approval = ApprovalId(nonce.clone());
        // 승인 한 건을 한 번만 읽는다: 이 프로필의 카드인지(다른 프로필의 nonce로 결정하지 못하게)와, 소유자 DM으로 전달한 카드의 영수증(`delivery`)에 함께 쓴다.
        // 읽지 못하면 다른 길로 결정하지 않고 재전달에 맡긴다.
        let stored = match self.ledger.approval(approval.clone()).await {
            Ok(stored) => Some(stored),
            Err(Error::Unavailable | Error::Stale) => {
                self.notice(install, button, RETRY_NOTICE).await;
                return Click::Retry;
            }
            Err(_) => None,
        };
        // 영수증이 있는 카드(소유자 DM으로 전달했고 게시를 확인했다)는 그 메시지의 버튼만 결정할 수 있다. 이때는 DM 입력 바인딩이 없어도 된다.
        let snapshot = match stored.as_ref().and_then(|stored| stored.delivery.as_ref()) {
            Some(receipt) => {
                let Some(snapshot) = routes.snapshot(&install.profile).cloned() else {
                    return Click::Ignored;
                };
                if !is_the_card(receipt, install, &snapshot, button) {
                    self.notice(install, button, "이 카드로는 승인할 수 없어요")
                        .await;
                    return Click::Done("wrong_card");
                }
                snapshot
            }
            None => {
                let real = if button.channel.starts_with('D') {
                    Real::SlackDm {
                        team: button.team.clone(),
                    }
                } else {
                    Real::SlackChannel {
                        team: button.team.clone(),
                        channel: button.channel.clone(),
                    }
                };
                match routes.find(&real) {
                    Some(route) if route.snapshot.id == install.profile => route.snapshot.clone(),
                    // 일반 DM 입력이 없는 설치: 소유자 DM으로 간 승인 카드가 아직 열리지 않았거나 이미 닫혔다면 결정하지 않고 본인에게만 알린다
                    _ => {
                        let state = stored
                            .as_ref()
                            .filter(|stored| stored.approval.attempt.profile == install.profile)
                            .map(|stored| stored.state);
                        let member = routes
                            .snapshot(&install.profile)
                            .is_some_and(|snapshot| snapshot.members.contains_key(&principal));
                        drop(routes);
                        return match state {
                            Some(state) if button.channel.starts_with('D') && member => {
                                self.unopened_card(install, button, state).await
                            }
                            _ => Click::Ignored,
                        };
                    }
                }
            }
        };
        drop(routes);
        if !snapshot.members.contains_key(&principal) {
            self.notice(install, button, "이 승인은 등록된 사용자만 누를 수 있어요")
                .await;
            return Click::Done("unregistered");
        }
        if !stored
            .as_ref()
            .is_some_and(|stored| stored.approval.attempt.profile == install.profile)
        {
            self.notice(install, button, "알 수 없는 승인 카드예요")
                .await;
            return Click::Done("unknown_card");
        }
        let outcome = match self.broker.decide(approval, principal, allow).await {
            Ok(()) if allow => "approved",
            Ok(()) => "denied",
            Err(Error::Denied) => {
                self.notice(install, button, "이 승인은 지정된 승인자만 누를 수 있어요")
                    .await;
                return Click::Done("forbidden");
            }
            Err(Error::Stale | Error::Missing) => {
                self.notice(install, button, "이 카드는 더 이상 유효하지 않아요")
                    .await;
                "stale"
            }
            // 반대 판정이 이미 확정된 카드(승인 뒤 거절·거절 뒤 승인 등). 같은 클릭을 다시 받아도 결론이 같으니 재전달을
            // 기다리지 않는다 — 원장을 다시 읽어 확정 상태면 카드를 그 상태로 갱신하고 ack한다. 확정 상태를 확인하지
            // 못하면(저장소 장애·아직 대기) 재전달에 맡긴다.
            Err(Error::Conflict) => match self.ledger.approval(approval_id(&nonce)).await {
                Ok(stored) if status_line(stored.state).is_some() => {
                    self.notice(install, button, "이미 다른 결정이 확정된 카드예요")
                        .await;
                    "conflict"
                }
                _ => {
                    self.notice(install, button, RETRY_NOTICE).await;
                    return Click::Retry;
                }
            },
            Err(_) => {
                self.notice(install, button, RETRY_NOTICE).await;
                return Click::Retry;
            }
        };
        // 원장 상태로 카드를 갱신한다. 갱신이 실패해도 결정을 다시 하지 않고 ack한다.
        self.refresh_card(install, button, &nonce).await;
        Click::Done(outcome)
    }

    /// 영수증이 없는 소유자 DM 카드의 버튼: 카드가 아직 열리지 않았거나(게시를 확인하는 순간) 이미 닫혔다. 결정하지 않고 누른 사람에게만 알린다.
    async fn unopened_card(
        &self,
        install: &Installation,
        button: &Button,
        state: ApprovalState,
    ) -> Click {
        if state == ApprovalState::Pending {
            self.notice(
                install,
                button,
                "아직 열리지 않은 카드예요 — 잠시 뒤 다시 눌러 주세요",
            )
            .await;
            Click::Done("not_open")
        } else {
            self.notice(install, button, "이 요청은 이미 닫혔어요")
                .await;
            Click::Done("closed")
        }
    }

    async fn refresh_card(&self, install: &Installation, button: &Button, nonce: &str) {
        let Ok(stored) = self.ledger.approval(ApprovalId(nonce.to_owned())).await else {
            return;
        };
        let Some(status) = status_line(stored.state) else {
            return;
        };
        if button.message_ts.is_empty() {
            return;
        }
        let card = serde_json::from_str::<Value>(&stored.card.0)
            .ok()
            .and_then(|value| parse_card(&value).ok());
        let blocks = match card {
            Some(card) => resolved_blocks(&card, status),
            None => vec![
                json!({"type": "context", "elements": [{"type": "plain_text", "text": status}]}),
            ],
        };
        let Some(token) = self.creds.bot_token(&install.profile, &install.alias).await else {
            return;
        };
        let body = json!({"channel": button.channel, "ts": button.message_ts, "blocks": blocks, "text": status});
        let result = self
            .creds
            .call("chat.update", &token, Body::Json(&body))
            .await;
        if !matches!(result, super::api::Raw::Ok(_)) {
            tracing::warn!("승인 카드를 갱신하지 못했어요(결정은 이미 기록됐어요)");
        }
    }

    /// 누른 사람에게만 보이는 안내. 실패해도 흐름에 영향이 없다.
    async fn notice(&self, install: &Installation, button: &Button, text: &str) {
        let Some(token) = self.creds.bot_token(&install.profile, &install.alias).await else {
            return;
        };
        let body = json!({"channel": button.channel, "user": button.user, "text": text});
        self.creds
            .call("chat.postEphemeral", &token, Body::Json(&body))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_the_bots_own_mention() {
        assert_eq!(strip_mention("<@UB> 안녕", "UB"), "안녕");
        assert_eq!(strip_mention("안녕 <@UB|bot> 하세요", "UB"), "안녕  하세요");
        assert_eq!(strip_mention("<@UB><@UB> 둘", "UB"), "둘");
        // 다른 사람 멘션은 남긴다. ID가 더 길게 이어지는 경우도 다른 사람이다
        assert_eq!(strip_mention("<@UB> <@U1> 부탁해", "UB"), "<@U1> 부탁해");
        assert_eq!(strip_mention("<@UB1> 안녕", "UB"), "<@UB1> 안녕");
        assert_eq!(strip_mention("<@UB 열린 채로", "UB"), "<@UB 열린 채로");
        assert_eq!(strip_mention("멘션 없음", "UB"), "멘션 없음");
        assert_eq!(strip_mention("   ", "UB"), "");
    }

    #[test]
    fn approval_states_map_to_one_status_line() {
        assert_eq!(status_line(ApprovalState::Pending), None);
        assert_eq!(status_line(ApprovalState::Approved), Some("✅ 승인됨"));
        assert_eq!(status_line(ApprovalState::Consumed), Some("✅ 승인됨"));
        for state in [ApprovalState::Denied, ApprovalState::Revoked] {
            assert_eq!(
                status_line(state),
                Some(if state == ApprovalState::Denied {
                    "⛔ 거절됨"
                } else {
                    "⛔ 철회됨"
                })
            );
        }
        assert!(
            status_line(ApprovalState::Expired)
                .unwrap()
                .contains("만료")
        );
        assert!(
            status_line(ApprovalState::Superseded)
                .unwrap()
                .contains("새 승인")
        );
    }
}

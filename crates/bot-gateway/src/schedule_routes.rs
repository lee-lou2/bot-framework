//! 명시 목적지(다른 수신자)의 호스트 등록부 대조(`ScheduleRoutes`). 근거는 **같은 프로필의 운영자 등록부 하나**다: 프로필의 Slack 채널 바인딩이 호스트
//! 등록부의 워크스페이스·채널 별칭을 거쳐 실제 팀 ID·채널 ID로 풀린다. 외부 서비스(Slack API)는 묻지 않는다. 수신자 허용 목록은 프로필 멤버가 공유한다.
//!
//! - 받는 이름: 바인딩이 가리키는 채널의 **별칭**, 실제 **채널 ID**, 이미 고정된 **정규 경로** `팀ID/채널ID`. 정규 경로는 지금 등록부의 같은 팀·같은 채널일 때만
//!   그대로 돌려준다(팀이 바뀌었거나 다른 채널이면 거절).
//! - 거절(`Denied`): 이 프로필에 없는 바인딩(다른 프로필·미등록)·Slack 채널이 아닌 입구(DM 바인딩은 수신자를 하나로 정하지 못하는 와일드카드, HTTP·CLI는
//!   원래 대화에만 답한다)·채널이 아닌 대화 종류·등록부에 없는 별칭·다른 팀·다른 채널. 잘못된 입력(빈 ID·모양이 틀린 경로·Slack `ts` 모양이 아닌 스레드)은 `Invalid`.
//! - 생성·발화·발송이 각자 **현재** 등록부로 부른다. 돌려준 `conversation.id`(`팀ID/채널ID`)가 outbox에 그대로 쌓이고, 발송은 같은 등록부로 다시 대조한 뒤
//!   API에는 채널 ID만 전달한다(`slack::send`).
use crate::host::{HostConfig, is_slack_id};
use bot_core::{
    id::ProfileId,
    intake::PushTransportId,
    port::Error,
    profile::{Adapter, ProfileCatalog},
    schedule::{DeliveryTarget, ScheduleRoutes},
    state::{Conversation, ConversationKind},
};
use std::sync::Arc;

/// 정규 전송 경로 `팀ID/채널ID`를 (팀, 채널)로 나눈다. 슬래시가 없으면 원래 대화의 채널 ID라 `None`이다. 모양이 틀리면 `None`.
pub fn split_route(id: &str) -> Option<(&str, &str)> {
    let (team, channel) = id.split_once('/')?;
    (is_slack_id(team) && is_slack_id(channel)).then_some((team, channel))
}

/// Slack 스레드 시각(`ts`) 모양(`1700000000.000100`).
pub fn is_ts(text: &str) -> bool {
    match text.split_once('.') {
        Some((secs, micros)) => {
            (1..=12).contains(&secs.len())
                && (1..=9).contains(&micros.len())
                && secs
                    .bytes()
                    .chain(micros.bytes())
                    .all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// 프로필 판본(현재 프로필 집합)과 호스트 등록부로 명시 목적지를 확정하는 등록부.
pub struct HostRoutes {
    catalog: Arc<dyn ProfileCatalog>,
    host: Arc<HostConfig>,
}

impl HostRoutes {
    pub fn new(catalog: Arc<dyn ProfileCatalog>, host: Arc<HostConfig>) -> Self {
        Self { catalog, host }
    }
}

/// 명시 목적지로 허용하는 어댑터의 Slack 채널 별칭과 발송 키를 돌려준다.
/// 현재 저장소(`bot-store/src/delivery.rs::push_to`·`approval_open.rs`)는 명시 목적지를 `PushTransportId::SLACK`으로
/// 고정한다. 새 푸시 어댑터의 명시 목적지를 열려면 `bot_user_schedules`·`bot_deliveries`에 목적지 발송 키를 함께 저장한 뒤
/// 이 분기와 저장소 고정을 같이 바꿔야 한다. 새 `Adapter` 변종이 추가되면 컴파일러가 멈추게 와일드카드를 두지 않는다.
fn slack_destination(adapter: &Adapter) -> Result<(&str, &str, PushTransportId), Error> {
    match adapter {
        Adapter::SlackChannel {
            workspace, channel, ..
        } => {
            let transport = crate::routes::reply_transport(adapter).ok_or(Error::Denied)?;
            if transport != PushTransportId::SLACK {
                return Err(Error::Denied);
            }
            Ok((workspace.as_str(), channel.as_str(), transport))
        }
        Adapter::SlackDm { .. }
        | Adapter::Cli { .. }
        | Adapter::Http { .. }
        | Adapter::OpenAi { .. } => Err(Error::Denied),
    }
}

impl ScheduleRoutes for HostRoutes {
    fn resolve(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error> {
        let conversation = &target.conversation;
        let id = conversation.id.as_str();
        if id.is_empty() || id.len() > 70 || id.chars().any(char::is_control) {
            return Err(Error::Invalid);
        }
        if conversation.thread.as_deref().is_some_and(|t| !is_ts(t)) {
            return Err(Error::Invalid);
        }
        // 채널만 수신자가 될 수 있다: 직접 대화(DM)는 수신자를 하나로 정할 수 없고, CLI·HTTP·예약 대화는 원래 대화에만 답한다
        match conversation.kind {
            ConversationKind::Channel => {}
            ConversationKind::Direct
            | ConversationKind::Cli
            | ConversationKind::Http
            | ConversationKind::Scheduled => return Err(Error::Denied),
        }
        // 같은 프로필의 현재 바인딩만(다른 프로필·없는 바인딩은 거절). 입구는 Slack 채널이어야 한다
        let snapshot = self.catalog.current(profile).ok_or(Error::Denied)?;
        let binding = snapshot
            .bindings
            .get(&target.binding)
            .ok_or(Error::Denied)?;
        let (workspace, channel, _transport) = slack_destination(&binding.source)?;
        // 호스트 등록부의 실제 팀·채널
        let registered = self
            .host
            .slack
            .workspaces
            .get(workspace)
            .ok_or(Error::Denied)?;
        let channel_id = registered.channels.get(channel).ok_or(Error::Denied)?;
        let canonical = format!("{}/{channel_id}", registered.team_id);
        // 받는 이름은 별칭·채널 ID·정규 경로뿐이다. 다른 팀의 정규 경로나 다른 채널은 여기서 걸러진다
        let named = match split_route(id) {
            Some(_) => id == canonical,
            None => id == channel || id == channel_id,
        };
        if !named {
            return Err(Error::Denied);
        }
        Ok(DeliveryTarget {
            binding: target.binding.clone(),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: canonical,
                thread: conversation.thread.clone(),
            },
        })
    }
}

#[cfg(test)]
mod tests;

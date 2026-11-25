//! Slack 이벤트 선택(F2): 바인딩이 받을 이벤트 종류, 어댑터가 인증한 이벤트 원천, 봇이 참여한 스레드의 증거.
use crate::{
    id::{BindingId, ProfileId},
    port::Reply,
};
use std::collections::BTreeSet;

/// 바인딩이 받는 Slack 이벤트 종류(profile.yaml의 `events`). 채널 바인딩은 `mention`·`thread_reply`·`reaction`, DM 바인딩은 `dm`이다.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SlackEvent {
    Mention,
    Dm,
    ThreadReply,
    Reaction,
}

impl SlackEvent {
    /// profile.yaml `events`의 값
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Dm => "dm",
            Self::ThreadReply => "thread_reply",
            Self::Reaction => "reaction",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "mention" => Self::Mention,
            "dm" => Self::Dm,
            "thread_reply" => Self::ThreadReply,
            "reaction" => Self::Reaction,
            _ => return None,
        })
    }

    /// 이 종류의 이벤트를 받는 트리거의 `on.event` 이름. 멘션과 DM은 같은 `message`다.
    pub const fn trigger_event(self) -> &'static str {
        match self {
            Self::Mention | Self::Dm => "message",
            Self::ThreadReply => "thread-reply",
            Self::Reaction => "reaction",
        }
    }
}

/// `events`를 생략한 채널 바인딩의 기본: 멘션만(기존 동작)
pub fn default_channel_events() -> BTreeSet<SlackEvent> {
    BTreeSet::from([SlackEvent::Mention])
}

/// 접수한 이벤트의 종류(트리거 `on.event`와 같은 가름): 멘션·DM은 `Message`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Message,
    ThreadReply,
    Reaction,
}

/// 인증된 Slack 어댑터가 **이번 이벤트에서 직접** 읽은 원천. 본문·모델·도구가 만들 수 없고 접수 명령의 `AcceptExtras`로만 간다(F4가 증거로 쓴다).
/// 멤버 신원은 여기에 없다 — 접수의 `principal`(인증된 행위자)이 정한다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackSource {
    pub kind: SourceKind,
    pub team: String,
    pub channel: String,
    /// 이벤트가 가리키는 글의 ts: 글·답글은 그 글의 ts, 반응은 반응 대상(`item.ts`)
    pub ts: String,
    /// 답글이면 스레드 루트 ts
    pub thread_ts: Option<String>,
    /// 이번에 접수한 Slack 이벤트의 ID(재전달에 같다). 같은 글이 `app_mention`과 `message`로 다른 ID로 올 수 있어 글의 접수는 (팀·채널·ts)로 하나다.
    pub event_id: String,
    /// 반응 이름(반응만)
    pub reaction: Option<String>,
}

impl SlackSource {
    /// 이 Slack 이벤트의 식별: 같은 이벤트면 어떤 전달·바인딩·프로필로 와도 같다(접수는 이 키로 한 건이다).
    /// 글(멘션·스레드 답글·DM)은 (팀, 채널, 글 ts) — `app_mention`과 `message`가 같은 글을 다른 `event_id`로 전달해도 같다 — 이고, 반응은 (팀, `event_id`)다.
    /// 구분자는 제어 문자(`U+001F`)라 식별자(제어 문자를 허용하지 않는다)가 키의 경계를 옮길 수 없다.
    pub fn event_key(&self) -> String {
        match self.kind {
            SourceKind::Message | SourceKind::ThreadReply => {
                format!(
                    "m\u{1f}{}\u{1f}{}\u{1f}{}",
                    self.team, self.channel, self.ts
                )
            }
            SourceKind::Reaction => format!("r\u{1f}{}\u{1f}{}", self.team, self.event_id),
        }
    }
}

/// 봇이 참여했다고 확인된 스레드: (프로필, 바인딩, 팀, 봇, 채널, 루트 ts). 설치(팀·봇)가 바뀌면 다른 소유로 본다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BotThread {
    pub profile: ProfileId,
    pub binding: BindingId,
    pub team: String,
    /// 봇 사용자 ID(`auth.test`)
    pub bot: String,
    pub channel: String,
    pub root_ts: String,
}

/// 봇 참여 스레드의 증거 저장소(F2 `thread_reply`의 입장 증거). 수신자가 준 `thread_ts`만으로는 입장시키지 않는다.
pub trait BotThreads: Send + Sync {
    /// 확인된 게시(영수증)나 인증된 자기 봇 메시지로 이 스레드를 기록한다. 이미 있으면 그대로다.
    fn note(&self, thread: BotThread) -> Reply<'_, ()>;
    /// 기록돼 있는가. 읽지 못하면 `Err(Unavailable)`이다(확인할 수 없으면 접수하지 않는다).
    fn known(&self, thread: BotThread) -> Reply<'_, bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_is_one_event_whatever_delivery_it_came_by_and_a_reaction_is_its_own_event() {
        let source = |kind, event_id: &str| SlackSource {
            kind,
            team: "T1".into(),
            channel: "C1".into(),
            ts: "100.1".into(),
            thread_ts: None,
            event_id: event_id.into(),
            reaction: None,
        };
        // 같은 글: 전달 이벤트 ID와 분류(멘션·답글)가 달라도 키가 같다
        let mention = source(SourceKind::Message, "Ev1");
        assert_eq!(
            mention.event_key(),
            source(SourceKind::Message, "Ev2").event_key()
        );
        assert_eq!(
            mention.event_key(),
            source(SourceKind::ThreadReply, "Ev3").event_key()
        );
        // 반응은 (팀, event_id)다: 같은 글에 달린 다른 반응 이벤트는 다른 이벤트다
        let reaction = source(SourceKind::Reaction, "Ev9");
        assert_ne!(reaction.event_key(), mention.event_key());
        assert_ne!(
            reaction.event_key(),
            source(SourceKind::Reaction, "Ev8").event_key()
        );
        assert_eq!(
            reaction.event_key(),
            source(SourceKind::Reaction, "Ev9").event_key()
        );
        // 팀·채널·글이 다르면 다른 이벤트다
        for other in [
            SlackSource {
                team: "T2".into(),
                ..mention.clone()
            },
            SlackSource {
                channel: "C2".into(),
                ..mention.clone()
            },
            SlackSource {
                ts: "100.2".into(),
                ..mention.clone()
            },
        ] {
            assert_ne!(other.event_key(), mention.event_key());
        }
    }

    #[test]
    fn event_names_round_trip_and_map_to_trigger_events() {
        for event in [
            SlackEvent::Mention,
            SlackEvent::Dm,
            SlackEvent::ThreadReply,
            SlackEvent::Reaction,
        ] {
            assert_eq!(SlackEvent::parse(event.as_str()), Some(event));
        }
        assert_eq!(SlackEvent::parse("message"), None);
        assert_eq!(SlackEvent::parse("thread-reply"), None);
        assert_eq!(SlackEvent::Mention.trigger_event(), "message");
        assert_eq!(SlackEvent::Dm.trigger_event(), "message");
        assert_eq!(SlackEvent::ThreadReply.trigger_event(), "thread-reply");
        assert_eq!(SlackEvent::Reaction.trigger_event(), "reaction");
        assert_eq!(default_channel_events().len(), 1);
    }
}

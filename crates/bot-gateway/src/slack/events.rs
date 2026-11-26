//! Slack 이벤트 선택: 채널의 멘션·스레드 답글·반응을 바인딩의 `events`에 따라 접수한다. 인증된 설치(소켓)가 받은 이벤트만 다루고, 팀·프로필·바인딩·멤버 확인은 일반 접수와 같다.
//!
//! - **한 글은 한 번만**: 같은 글이 `app_mention`과 `message.channels`로 다른 이벤트 ID로 올 수 있다. 어느 쪽이 먼저 오든 같은 판정이 나오게 글의 **내용**으로 가른다 —
//!   봇 멘션이 있고 `mention`이 켜져 있으면 멘션(우선), 아니면 스레드 답글이고 `thread_reply`가 켜져 있고 증거가 있으면 답글이다. 요청 ID는 `채널.ts`라 접수는 하나다.
//! - **스레드 증거**: 답글은 봇이 참여했다고 확인된 스레드(`BotThreads`)에서만 받는다. 수신자가 준 `thread_ts`만으로는 들이지 않는다. 증거를 확인할 수 없으면(저장소 장애)
//!   ack하지 않아 다시 받고, 증거가 없으면 받았다고만 한다(`thread_unverified`). 자기 봇의 글은 증거로만 기록하고 트리거를 실행하지 않는다.
//! - **반응**: 등록 채널 글에 단 `reaction_added`. 행위자(`event.user`)가 멤버여야 하고 봇 자신의 반응은 받지 않는다. 이 사건을 받는 트리거가 없으면 한도를 쓰지 않고 받았다고만 한다.
use super::{
    SlackGateway,
    frame::{Message, Post, Reaction},
    handle::mentions_bot,
    human::{Actor, NOT_HUMAN},
    socket::Installation,
    ux::{TrackedRun, is_stop_phrase},
};
use crate::{
    entrance::Real,
    identity,
    inbound::{Event, Inbound, valid_request_id},
    ingress::Admission,
    routes::Routes,
    trigger,
};
use bot_core::{
    id::{BindingId, RequestId},
    profile::Adapter,
    slack::{BotThread, SlackEvent, SlackSource, SourceKind},
    state::{Conversation, ConversationKind},
};
use serde_json::json;
use std::collections::BTreeSet;

/// 채널 글 한 건(`app_mention`과 `message.channels`가 같은 모양으로 합쳐진다)
pub(super) struct ChannelMsg {
    pub team: String,
    pub channel: String,
    pub user: String,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub files: Vec<String>,
    pub event_id: Option<String>,
    /// `app_mention` 이벤트로 왔다(그 자체로 멘션이다)
    pub mention_event: bool,
}

impl From<Message> for ChannelMsg {
    fn from(message: Message) -> Self {
        Self {
            team: message.team,
            channel: message.channel,
            user: message.user,
            text: message.text,
            ts: message.ts,
            thread_ts: message.thread_ts,
            files: message.files,
            event_id: message.event_id,
            mention_event: true,
        }
    }
}

/// 이 글이 어떤 종류로 받아지는가
enum Pick {
    Mention,
    /// 스레드 답글(루트 ts)
    Reply(String),
}

impl SlackGateway {
    /// 이 설치의 프로필이 소유한 채널 바인딩과 그 `events`. 설치와 맞지 않는 채널·바인딩이 아니면 `None`.
    fn channel_route<'a>(
        &self,
        install: &Installation,
        routes: &'a Routes,
        team: &str,
        channel: &str,
    ) -> Option<(&'a BindingId, &'a BTreeSet<SlackEvent>)> {
        let route = routes.find(&Real::SlackChannel {
            team: team.to_owned(),
            channel: channel.to_owned(),
        })?;
        if route.snapshot.id != install.profile {
            return None;
        }
        match &route.binding.source {
            Adapter::SlackChannel { events, .. } => Some((route.binding_id, events)),
            _ => None,
        }
    }

    /// 봇이 이 스레드에 참여했다고 기록돼 있는가. 저장소를 읽지 못하면 `None`(확인할 수 없다 — 접수하지 않고 ack도 하지 않는다).
    async fn thread_known(
        &self,
        install: &Installation,
        binding: &BindingId,
        channel: &str,
        root_ts: &str,
    ) -> Option<bool> {
        let Some(threads) = &self.threads else {
            return Some(false);
        };
        threads
            .known(thread_key(install, binding, channel, root_ts))
            .await
            .ok()
    }

    /// 채널 글(멘션 또는 스레드 답글) 접수. 돌려주는 값은 «ack해도 되는가»다.
    pub(super) async fn on_channel_message(&self, install: &Installation, msg: ChannelMsg) -> bool {
        if msg.team != install.team {
            return true;
        }
        let Ok(routes) = self.ingress.routes() else {
            return false; // 표를 만들 수 없으면 ack하지 않아 Slack이 다시 보낸다
        };
        let Some((binding, events)) = self.channel_route(install, &routes, &msg.team, &msg.channel)
        else {
            return true;
        };
        let mentioned = msg.mention_event || mentions_bot(&msg.text, &install.bot_user);
        // 스레드 답글: `thread_ts`가 있고 글 자신이 루트가 아니다
        let root = msg.thread_ts.as_deref().filter(|root| *root != msg.ts);
        let pick = if mentioned && events.contains(&SlackEvent::Mention) {
            Pick::Mention
        } else if let (Some(root), true) = (root, events.contains(&SlackEvent::ThreadReply)) {
            match self
                .thread_known(install, binding, &msg.channel, root)
                .await
            {
                Some(true) => Pick::Reply(root.to_owned()),
                // 봇이 참여한 것으로 확인되지 않은 스레드(`thread_unverified`): 접수하지 않는다
                Some(false) => return true,
                None => return false,
            }
        } else {
            // 이 바인딩이 받지 않는 종류(`event_not_allowed`)
            return true;
        };
        let Some(principal) = identity::slack(&install.alias, &msg.user) else {
            return true;
        };
        // 같은 글은 어떤 이벤트·재전달로 와도 같은 요청이다(채널 + 글 ts)
        let request = format!("{}.{}", msg.channel, msg.ts);
        if !valid_request_id(&request) {
            return true;
        }
        let text = super::handle::strip_mention(&msg.text, &install.bot_user);
        let thread_ts = msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone());
        if matches!(pick, Pick::Mention)
            && is_stop_phrase(&text)
            && let Some(ux) = &self.ux
            && ux
                .on_stop_mention(
                    install,
                    routes.snapshot(&install.profile).map(|v| &**v),
                    &msg.channel,
                    &thread_ts,
                    &msg.user,
                )
                .await
        {
            return true;
        }
        let (event, kind, body) = match &pick {
            Pick::Mention => (Event::Message, SourceKind::Message, json!({"text": text})),
            Pick::Reply(root) => (
                Event::ThreadReply,
                SourceKind::ThreadReply,
                json!({"text": text, "channel": msg.channel, "ts": msg.ts, "thread_ts": root}),
            ),
        };
        let inbound = Inbound {
            real: Real::SlackChannel {
                team: msg.team.clone(),
                channel: msg.channel.clone(),
            },
            principal: principal.clone(),
            request: RequestId(request.clone()),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: msg.channel.clone(),
                thread: Some(thread_ts.clone()),
            },
            attachments: msg
                .files
                .iter()
                .map(|id| format!("slack:{}/{id}", install.alias))
                .collect(),
            body,
            event,
        };
        let source = SlackSource {
            kind,
            team: msg.team.clone(),
            channel: msg.channel.clone(),
            ts: msg.ts.clone(),
            thread_ts: msg.thread_ts,
            event_id: msg.event_id.unwrap_or(request),
            reaction: None,
        };
        // 소유자를 확인한 바로 그 표로 접수한다(그 사이 입구 소유자가 바뀌어도 이 설치의 프로필 밖으로 새지 않는다)
        let admission = self
            .ingress
            .admit_sourced(&routes, inbound, Some(source))
            .await;
        if matches!(pick, Pick::Mention)
            && let (Some(ux), Admission::Accepted { run, .. }) = (&self.ux, &admission)
        {
            ux.track(TrackedRun {
                profile: install.profile.clone(),
                workspace: install.alias.clone(),
                team: msg.team,
                channel: msg.channel,
                thread_ts,
                msg_ts: msg.ts,
                user: msg.user,
                principal,
                run: run.clone(),
            });
        }
        admission.settled()
    }

    /// 채널의 글 한 건(`message.channels`·`message.groups`). 자기 봇의 글은 스레드 증거로만 기록하고, 사람의 글은 접수 판정으로 보낸다.
    pub(super) async fn on_post(&self, install: &Installation, post: Post) -> bool {
        if post.team != install.team {
            return true;
        }
        if post.user.as_deref() == Some(install.bot_user.as_str()) {
            return self.note_own_post(install, &post).await;
        }
        let (true, Some(user)) = (post.human, post.user) else {
            return true; // 다른 앱·봇의 글은 받지 않는다
        };
        self.on_channel_message(
            install,
            ChannelMsg {
                team: post.team,
                channel: post.channel,
                user,
                text: post.text,
                ts: post.ts,
                thread_ts: post.thread_ts,
                files: post.files,
                event_id: post.event_id,
                mention_event: false,
            },
        )
        .await
    }

    /// 인증된 자기 봇의 글을 스레드 증거로 기록한다(트리거는 실행하지 않는다). 글이 스레드 안이면 그 루트, 아니면 글 자신이 루트다.
    async fn note_own_post(&self, install: &Installation, post: &Post) -> bool {
        let (Some(threads), Ok(routes)) = (&self.threads, self.ingress.routes()) else {
            return self.threads.is_none() || self.ingress.routes().is_ok();
        };
        let Some((binding, events)) =
            self.channel_route(install, &routes, &post.team, &post.channel)
        else {
            return true;
        };
        if !events.contains(&SlackEvent::ThreadReply) {
            return true;
        }
        let root = post.thread_ts.as_deref().unwrap_or(&post.ts);
        threads
            .note(thread_key(install, binding, &post.channel, root))
            .await
            .is_ok()
    }

    /// 등록 채널 글에 단 반응 접수. 돌려주는 값은 «ack해도 되는가»다.
    /// 순서: 팀·봇 자신 → 등록 채널·`events`·이 사건을 받는 트리거 → 프로필 멤버 → **지금 활성 사람 계정인가**(`users.info`, 기억하지 않는다) → 회신 스레드 확인 → 접수.
    /// 사람이 아니거나 확인하지 못하면 후보·기본 대화·승인 증거 없이 고정 사유만 남기고 ack한다(Slack의 ack 기한 안에 끝난다).
    pub(super) async fn on_reaction(&self, install: &Installation, reaction: Reaction) -> bool {
        if reaction.team != install.team || reaction.user == install.bot_user {
            return true; // 다른 팀·봇 자신의 반응
        }
        let Ok(routes) = self.ingress.routes() else {
            return false;
        };
        let Some((binding, events)) =
            self.channel_route(install, &routes, &reaction.team, &reaction.channel)
        else {
            return true;
        };
        if !events.contains(&SlackEvent::Reaction) {
            return true;
        }
        // 이 사건을 받는 트리거가 하나도 없으면 한도를 쓰지 않고 받았다고만 한다(기본 대화는 직접 도구 후보가 있을 때만 의미가 있다)
        let route = routes.find(&Real::SlackChannel {
            team: reaction.team.clone(),
            channel: reaction.channel.clone(),
        });
        let wanted = route.as_ref().is_some_and(|route| {
            route
                .triggers
                .iter()
                .any(|bound| trigger::applies(&bound.trigger, binding, &Event::Reaction))
        });
        let Some(principal) = identity::slack(&install.alias, &reaction.user) else {
            return true;
        };
        if !wanted || !valid_request_id(&reaction.event_id) {
            return true;
        }
        // 멤버가 아니면 Slack을 부르지 않는다(멤버·한도 확인은 접수가 다시 한다)
        if !route.is_some_and(|route| route.snapshot.members.contains_key(&principal)) {
            return true;
        }
        if let Actor::Refused(reason) = self.reaction_actor(install, &reaction.user).await {
            // 고정 사유만 남긴다(사용자·글 내용은 싣지 않는다). scope 부족은 운영자가 고쳐야 하니 눈에 띄게 남긴다
            if reason == NOT_HUMAN {
                tracing::debug!(workspace = %install.alias, reason, "사람이 아닌 계정의 반응은 받지 않아요");
            } else {
                tracing::warn!(workspace = %install.alias, reason, "반응한 사람을 확인하지 못해 받지 않아요 — 봇 scope `users:read`와 Slack 연결을 확인하세요");
            }
            return true;
        }
        // 회신은 봇이 참여한 것으로 확인된 스레드(그 글이 루트)로만 간다. 반응이 달린 글의 ts를 스레드 루트로 짐작하지 않는다(답글일 수 있다): 확인되지 않으면 채널 최상위다.
        let thread = match self
            .thread_known(install, binding, &reaction.channel, &reaction.item_ts)
            .await
        {
            Some(true) => Some(reaction.item_ts.clone()),
            Some(false) => None,
            None => return false,
        };
        let inbound = Inbound {
            real: Real::SlackChannel {
                team: reaction.team.clone(),
                channel: reaction.channel.clone(),
            },
            principal,
            request: RequestId(reaction.event_id.clone()),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: reaction.channel.clone(),
                thread,
            },
            attachments: Vec::new(),
            body: json!({
                "reaction": reaction.reaction,
                "channel": reaction.channel,
                "item_ts": reaction.item_ts,
                "item_user": reaction.item_user,
            }),
            event: Event::Reaction,
        };
        let source = SlackSource {
            kind: SourceKind::Reaction,
            team: reaction.team,
            channel: reaction.channel,
            ts: reaction.item_ts,
            thread_ts: None,
            event_id: reaction.event_id,
            reaction: Some(reaction.reaction),
        };
        self.ingress
            .admit_sourced(&routes, inbound, Some(source))
            .await
            .settled()
    }
}

fn thread_key(
    install: &Installation,
    binding: &BindingId,
    channel: &str,
    root_ts: &str,
) -> BotThread {
    BotThread {
        profile: install.profile.clone(),
        binding: binding.clone(),
        team: install.team.clone(),
        bot: install.bot_user.clone(),
        channel: channel.to_owned(),
        root_ts: root_ts.to_owned(),
    }
}

//! Slack channel event routing for mentions, verified thread replies, and human reactions.
use super::{
    SlackGateway,
    frame::{Message, Post, Reaction},
    handle::mentions_bot,
    human::{Actor, NOT_HUMAN},
    socket::Installation,
    ux::{StopMention, TrackedRun, is_stop_phrase},
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
    id::{BindingId, PrincipalId, RequestId},
    profile::Adapter,
    slack::{BotThread, SlackEvent, SlackSource, SourceKind},
    state::{Conversation, ConversationKind},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tokio_util::sync::CancellationToken;

pub(super) struct SlackAdmission {
    pub real: Real,
    pub principal: PrincipalId,
    pub request: String,
    pub kind: ConversationKind,
    pub source_kind: SourceKind,
    pub team: String,
    pub channel: String,
    pub user: String,
    pub ts: String,
    pub raw_thread_ts: Option<String>,
    pub thread_ts: String,
    pub files: Vec<String>,
    pub event_id: Option<String>,
    pub body: Value,
    pub event: Event,
    pub track_ux: bool,
}

pub(super) struct ChannelMsg {
    pub team: String,
    pub channel: String,
    pub user: String,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub files: Vec<String>,
    pub event_id: Option<String>,
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

enum Pick {
    Mention,
    Reply(String),
}

impl SlackGateway {
    pub(super) async fn try_stop_mention(
        &self,
        install: &Installation,
        routes: &Routes,
        channel: &str,
        thread_ts: &str,
        user: &str,
    ) -> StopMention {
        let Some(ux) = &self.ux else {
            return StopMention::Pass;
        };
        ux.on_stop_mention(
            &self.ingress,
            install,
            routes.snapshot(&install.profile).map(|v| &**v),
            channel,
            thread_ts,
            user,
        )
        .await
    }

    pub(super) async fn admit_slack_message(
        &self,
        install: &Installation,
        routes: &Routes,
        msg: SlackAdmission,
        stop: &CancellationToken,
    ) -> bool {
        let inbound = Inbound {
            real: msg.real,
            principal: msg.principal.clone(),
            request: RequestId(msg.request.clone()),
            conversation: Conversation {
                kind: msg.kind,
                id: msg.channel.clone(),
                thread: Some(msg.thread_ts.clone()),
            },
            attachments: msg
                .files
                .iter()
                .map(|id| format!("slack:{}/{id}", install.alias))
                .collect(),
            body: msg.body,
            event: msg.event,
        };
        let source = SlackSource {
            kind: msg.source_kind,
            team: msg.team.clone(),
            channel: msg.channel.clone(),
            ts: msg.ts.clone(),
            thread_ts: msg.raw_thread_ts,
            event_id: msg.event_id.unwrap_or(msg.request),
            reaction: None,
        };
        // Admit against the ownership-verified `routes` snapshot so concurrent updates cannot cross profiles.
        let admission = self
            .ingress
            .admit_sourced(routes, inbound, Some(source))
            .await;
        if msg.track_ux
            && let (Some(ux), Admission::Accepted { run, .. }) = (&self.ux, &admission)
        {
            ux.track(
                TrackedRun {
                    profile: install.profile.clone(),
                    workspace: install.alias.clone(),
                    team: msg.team,
                    channel: msg.channel,
                    thread_ts: msg.thread_ts,
                    msg_ts: msg.ts,
                    user: msg.user,
                    principal: msg.principal,
                    run: run.clone(),
                },
                stop.clone(),
            );
        }
        admission.settled()
    }

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

    /// Returns `None` on store outage so the caller leaves the event unacked for redelivery.
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

    /// Classifies a channel message consistently across `app_mention` and `message.channels` deliveries.
    /// Returns whether the socket envelope may be acknowledged.
    pub(super) async fn on_channel_message(
        &self,
        install: &Installation,
        msg: ChannelMsg,
        stop: &CancellationToken,
    ) -> bool {
        if msg.team != install.team {
            return true;
        }
        let Ok(routes) = self.ingress.routes() else {
            return false;
        };
        let Some((binding, events)) = self.channel_route(install, &routes, &msg.team, &msg.channel)
        else {
            return true;
        };
        let mentioned = msg.mention_event || mentions_bot(&msg.text, &install.bot_user);
        let root = msg.thread_ts.as_deref().filter(|root| *root != msg.ts);
        let pick = if mentioned && events.contains(&SlackEvent::Mention) {
            Pick::Mention
        } else if let (Some(root), true) = (root, events.contains(&SlackEvent::ThreadReply)) {
            match self
                .thread_known(install, binding, &msg.channel, root)
                .await
            {
                Some(true) => Pick::Reply(root.to_owned()),
                Some(false) => return true,
                None => return false,
            }
        } else {
            return true;
        };
        let Some(principal) = identity::slack(&install.alias, &msg.user) else {
            return true;
        };
        // Deduplicate `app_mention` and `message.channels` for the same post via `channel.ts`.
        let request = format!("{}.{}", msg.channel, msg.ts);
        if !valid_request_id(&request) {
            return true;
        }
        let text = super::handle::strip_mention(&msg.text, &install.bot_user);
        let thread_ts = msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone());
        let is_mention = matches!(pick, Pick::Mention);
        if is_mention && is_stop_phrase(&text) {
            match self
                .try_stop_mention(install, &routes, &msg.channel, &thread_ts, &msg.user)
                .await
            {
                StopMention::Pass => {}
                StopMention::Handled => return true,
                StopMention::Retry => return false,
            }
        }
        let (event, source_kind, body) = match &pick {
            Pick::Mention => (Event::Message, SourceKind::Message, json!({"text": text})),
            Pick::Reply(root) => (
                Event::ThreadReply,
                SourceKind::ThreadReply,
                json!({"text": text, "channel": msg.channel, "ts": msg.ts, "thread_ts": root}),
            ),
        };
        self.admit_slack_message(
            install,
            &routes,
            SlackAdmission {
                real: Real::SlackChannel {
                    team: msg.team.clone(),
                    channel: msg.channel.clone(),
                },
                principal,
                request,
                kind: ConversationKind::Channel,
                source_kind,
                team: msg.team,
                channel: msg.channel,
                user: msg.user,
                ts: msg.ts,
                raw_thread_ts: msg.thread_ts,
                thread_ts,
                files: msg.files,
                event_id: msg.event_id,
                body,
                event,
                track_ux: is_mention,
            },
            stop,
        )
        .await
    }

    pub(super) async fn on_post(
        &self,
        install: &Installation,
        post: Post,
        stop: &CancellationToken,
    ) -> bool {
        if post.team != install.team {
            return true;
        }
        if post.user.as_deref() == Some(install.bot_user.as_str()) {
            return self.note_own_post(install, &post).await;
        }
        let (true, Some(user)) = (post.human, post.user) else {
            return true;
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
            stop,
        )
        .await
    }

    /// Records the bot's own channel post as thread-participation evidence without firing triggers.
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

    /// Admits channel reactions only after verifying trigger match, membership, and live human actor status.
    pub(super) async fn on_reaction(&self, install: &Installation, reaction: Reaction) -> bool {
        if reaction.team != install.team || reaction.user == install.bot_user {
            return true;
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
        // Skip Slack `users.info` lookup for non-members.
        if !route.is_some_and(|route| route.snapshot.members.contains_key(&principal)) {
            return true;
        }
        if let Actor::Refused(reason) = self.reaction_actor(install, &reaction.user).await {
            if reason == NOT_HUMAN {
                tracing::debug!(workspace = %install.alias, reason, "ignoring reaction from non-human account");
            } else {
                tracing::warn!(workspace = %install.alias, reason, "could not verify reacting user — check bot scope `users:read` and Slack connectivity");
            }
            return true;
        }
        // Only reply in-thread when `item_ts` is a verified bot thread root; otherwise post top-level.
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

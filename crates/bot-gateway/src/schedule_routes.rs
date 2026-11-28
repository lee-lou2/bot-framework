//! Scheduled delivery destination resolver verifying Slack channel routes against the profile and host registry.
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

/// Splits a canonical `TEAM_ID/CHANNEL_ID` delivery route; returns `None` for bare channel IDs or invalid shapes.
pub fn split_route(id: &str) -> Option<(&str, &str)> {
    let (team, channel) = id.split_once('/')?;
    (is_slack_id(team) && is_slack_id(channel)).then_some((team, channel))
}

/// Validates a Slack timestamp (`ts`) such as `1700000000.000100`.
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

pub struct HostRoutes {
    catalog: Arc<dyn ProfileCatalog>,
    host: Arc<HostConfig>,
}

impl HostRoutes {
    pub fn new(catalog: Arc<dyn ProfileCatalog>, host: Arc<HostConfig>) -> Self {
        Self { catalog, host }
    }
}

/// Explicit schedule destinations are currently coupled to `PushTransportId::SLACK` in `bot-store`.
/// Keep this match exhaustive so adding a new `Adapter` variant forces updating persistence and routing together.
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
        | Adapter::TelegramDm { .. }
        | Adapter::TelegramGroups { .. }
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
        // Only explicit channels are valid schedule targets (DMs are wildcards; CLI/HTTP reply only in-thread).
        match conversation.kind {
            ConversationKind::Channel => {}
            ConversationKind::Direct
            | ConversationKind::Cli
            | ConversationKind::Http
            | ConversationKind::Scheduled => return Err(Error::Denied),
        }
        let snapshot = self.catalog.current(profile).ok_or(Error::Denied)?;
        let binding = snapshot
            .bindings
            .get(&target.binding)
            .ok_or(Error::Denied)?;
        let (workspace, channel, _transport) = slack_destination(&binding.source)?;
        let registered = self
            .host
            .slack
            .workspaces
            .get(workspace)
            .ok_or(Error::Denied)?;
        let channel_id = registered.channels.get(channel).ok_or(Error::Denied)?;
        let canonical = format!("{}/{channel_id}", registered.team_id);
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

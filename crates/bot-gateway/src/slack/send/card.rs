//! Owner 1:1 DM approval card delivery via `conversations.open` and `chat.postMessage`.
//!
//! Opens a verified `D...` DM channel for one Slack principal on the same installation, posts the intact card once
//! without plain-text fallback or truncation, and confirms the receipt solely from Slack's response channel and `ts`.
use super::{SlackSender, confirms_post};
use crate::{
    identity,
    reply::Card,
    slack::{
        api::{Body, Raw},
        blocks::{card_blocks, card_fits, card_text, neutralize},
        creds::BotCheck,
    },
};
use bot_core::{
    id::{PrincipalId, ProfileId},
    intake::NotSentReason,
};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardPost {
    Posted {
        team: String,
        dm: String,
        ts: String,
    },
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    Unavailable,
    Held,
}

/// Splits a validated `slack:<alias>/<user_id>` principal into `(alias, user_id)`.
pub fn slack_user(principal: &PrincipalId) -> Option<(&str, &str)> {
    let (alias, user) = principal.0.strip_prefix("slack:")?.split_once('/')?;
    (identity::slack(alias, user).as_ref() == Some(principal)).then_some((alias, user))
}

fn dm_channel(value: &Value) -> Option<&str> {
    value["channel"]["id"]
        .as_str()
        .filter(|id| id.starts_with('D') && crate::host::is_slack_id(id))
}

impl SlackSender {
    pub async fn post_owner_card(
        &self,
        profile: &ProfileId,
        workspace: &str,
        owner: &PrincipalId,
        card: &Card,
    ) -> CardPost {
        let Some((alias, user)) = slack_user(owner) else {
            return CardPost::Unavailable;
        };
        if alias != workspace || !card_fits(card) {
            return CardPost::Unavailable;
        }
        let BotCheck::Verified(token, auth) = self.creds.check_bot(profile, workspace).await else {
            return CardPost::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None,
            };
        };
        let opened = self
            .creds
            .call(
                "conversations.open",
                &token,
                Body::Form(vec![("users", user.to_owned())]),
            )
            .await;
        let dm = match opened {
            Raw::Ok(value) => match dm_channel(&value) {
                Some(dm) => dm.to_owned(),
                None => return CardPost::Unavailable,
            },
            raw => return open_failed(raw),
        };
        let message = json!({
            "channel": dm,
            "blocks": card_blocks(card),
            "text": neutralize(&card_text(card)),
            "unfurl_links": false,
            "unfurl_media": false,
        });
        let raw = self
            .creds
            .call("chat.postMessage", &token, Body::Json(&message))
            .await;
        match raw {
            Raw::Ok(value) => {
                confirms_post(&dm, &value).map_or(CardPost::Held, |(dm, ts)| CardPost::Posted {
                    team: auth.team_id,
                    dm,
                    ts,
                })
            }
            Raw::Error(code) if super::UNKNOWN_ERRORS.contains(&code.as_str()) => CardPost::Held,
            Raw::Error(_) => CardPost::Unavailable,
            Raw::RateLimited(wait) => CardPost::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(wait),
            },
            Raw::BeforeWrite | Raw::TokenRejected => CardPost::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None,
            },
            Raw::Unknown => CardPost::Held,
        }
    }
}

/// `conversations.open` is idempotent and posts nothing: definite Slack errors map to `Unavailable`, transient ones to `NotSent`.
fn open_failed(raw: Raw) -> CardPost {
    match raw {
        Raw::Ok(_) => CardPost::Unavailable,
        Raw::RateLimited(wait) => CardPost::NotSent {
            reason: NotSentReason::RateLimited,
            retry_after: Some(wait),
        },
        Raw::BeforeWrite | Raw::Unknown | Raw::TokenRejected => CardPost::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None,
        },
        Raw::Error(code) if super::UNKNOWN_ERRORS.contains(&code.as_str()) => CardPost::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None,
        },
        Raw::Error(_) => CardPost::Unavailable,
    }
}

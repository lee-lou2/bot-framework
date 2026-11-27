//! Live `users.info` verification that a Slack reaction actor is an active human account.
use super::{
    SlackGateway,
    api::{Body, Raw},
    socket::Installation,
};
use serde_json::Value;
use std::time::Duration;

/// Must complete inside Slack's 3-second Socket Mode ack deadline.
const LOOKUP_LIMIT: Duration = Duration::from_secs(2);
/// Slackbot reports `is_bot: false` in `users.info` and must be filtered explicitly.
const SLACKBOT: &str = "USLACKBOT";

pub(super) const NOT_HUMAN: &str = "actor_not_human";
pub(super) const SCOPE_MISSING: &str = "actor_scope_missing";
pub(super) const UNVERIFIED: &str = "actor_unverified";

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Actor {
    Human,
    Refused(&'static str),
}

pub(super) fn judge(reply: &Value, asked: &str) -> Actor {
    let user = &reply["user"];
    if user["id"].as_str() != Some(asked) {
        return Actor::Refused(UNVERIFIED);
    }
    let Some(is_bot) = user["is_bot"].as_bool() else {
        return Actor::Refused(UNVERIFIED);
    };
    // Per Slack API, omitted `deleted` means active, whereas non-boolean values fail closed.
    let deleted = match user.get("deleted") {
        None => false,
        Some(Value::Bool(deleted)) => *deleted,
        Some(_) => return Actor::Refused(UNVERIFIED),
    };
    if is_bot || deleted {
        Actor::Refused(NOT_HUMAN)
    } else {
        Actor::Human
    }
}

impl SlackGateway {
    /// Never cached: queries `users.info` per event so deactivated accounts lose access immediately.
    pub(super) async fn reaction_actor(&self, install: &Installation, user: &str) -> Actor {
        if user == install.bot_user || user == SLACKBOT {
            return Actor::Refused(NOT_HUMAN);
        }
        // Bound both `auth.test` token check and `users.info` inside the same ack budget.
        let lookup = async {
            let token = self
                .creds
                .bot_token(&install.profile, &install.alias)
                .await?;
            Some(
                self.creds
                    .call(
                        "users.info",
                        &token,
                        Body::Form(vec![("user", user.to_owned())]),
                    )
                    .await,
            )
        };
        match tokio::time::timeout(LOOKUP_LIMIT, lookup).await {
            Ok(Some(Raw::Ok(reply))) => judge(&reply, user),
            Ok(Some(Raw::Error(code))) if code == "missing_scope" => Actor::Refused(SCOPE_MISSING),
            Ok(_) | Err(_) => Actor::Refused(UNVERIFIED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(user: Value) -> Value {
        json!({"ok": true, "user": user})
    }

    #[test]
    fn only_an_active_non_bot_account_with_the_asked_id_is_human() {
        let human = |extra: Value| {
            let mut user = json!({"id": "U1", "is_bot": false});
            if let (Some(base), Some(add)) = (user.as_object_mut(), extra.as_object()) {
                base.extend(add.clone());
            }
            judge(&reply(user), "U1")
        };
        assert_eq!(human(json!({})), Actor::Human, "omitted deleted is active");
        assert_eq!(human(json!({"deleted": false})), Actor::Human);
        assert_eq!(human(json!({"is_app_user": true})), Actor::Human);
        assert_eq!(human(json!({"deleted": true})), Actor::Refused(NOT_HUMAN));
        assert_eq!(human(json!({"is_bot": true})), Actor::Refused(NOT_HUMAN));
    }

    #[test]
    fn an_unknown_shape_or_another_users_reply_is_never_a_human_verdict() {
        for bad in [
            reply(json!({"id": "U2", "is_bot": false})),
            reply(json!({"is_bot": false})),
            reply(json!({"id": "U1"})),
            reply(json!({"id": "U1", "is_bot": "false"})),
            reply(json!({"id": "U1", "is_bot": null})),
            reply(json!({"id": "U1", "is_bot": false, "deleted": "no"})),
            reply(json!({"id": "U1", "is_bot": false, "deleted": null})),
            reply(json!({"id": "U1", "is_bot": false, "deleted": 0})),
            json!({"ok": true}),
            json!({"ok": true, "user": []}),
        ] {
            assert_eq!(judge(&bad, "U1"), Actor::Refused(UNVERIFIED), "{bad}");
        }
    }
}

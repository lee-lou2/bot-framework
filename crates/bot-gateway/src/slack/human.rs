//! Slack `reaction` 이벤트에서 반응한 사람이 **지금도 활성 «사람» 계정인가**를 확인한다. Slack의 `reaction_added`는 행위자가 봇인지 알려 주지 않으므로, 멤버 확인을 마친 뒤 같은 설치의 봇 토큰으로
//! `users.info`(봇 scope `users:read`)를 이번 이벤트마다 부른다. 사람 판정은 **기억하지 않는다** — 비활성화한 직후 옛 판정으로 승인이 가능해지지 않게 한다.
//!
//! 사람은 응답의 `user.id`가 물은 ID와 같고 `is_bot`이 불리언 `false`이며 `deleted`가 없거나 `false`인 계정이다(`deleted`를 생략하는 것은 Slack 명세상 정상).
//! 봇 자신과 `USLACKBOT`(Slackbot은 `is_bot=false`다)은 따로 거른다. `is_app_user`는 호출한 앱의 승인 사용자 표시이므로 거절 근거가 아니다.
//! 사람이 아니거나 확인하지 못하면(scope 부족·429·조회 실패·시간 초과·ID나 필드를 알 수 없음) 접수하지 않는다: 후보·기본 대화·승인 증거가 모두 0이고, 호출한 쪽은
//! Slack의 ack 기한 안에 고정 사유만 남기고 ack한다.
use super::{
    SlackGateway,
    api::{Body, Raw},
    socket::Installation,
};
use serde_json::Value;
use std::time::Duration;

/// `users.info`를 기다리는 시간: Slack이 요구하는 ack 기한(3초) 안에 끝나야 한다.
const LOOKUP_LIMIT: Duration = Duration::from_secs(2);
/// Slackbot의 사용자 ID. `is_bot`이 `false`지만 사람이 아니다.
const SLACKBOT: &str = "USLACKBOT";

/// 사람이 아니다: 봇·Slackbot·비활성 계정.
pub(super) const NOT_HUMAN: &str = "actor_not_human";
/// `users.info`의 scope(`users:read`)가 없다.
pub(super) const SCOPE_MISSING: &str = "actor_scope_missing";
/// 사람인지 확인하지 못했다: 연결·속도 제한·시간 초과·응답 모양 불명.
pub(super) const UNVERIFIED: &str = "actor_unverified";

/// 판정. `Refused`의 값은 고정 사유다(위 상수).
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Actor {
    Human,
    Refused(&'static str),
}

/// `users.info` 응답이 «활성 사람»인지 본다(순수). `asked`는 우리가 물은 사용자 ID다.
pub(super) fn judge(reply: &Value, asked: &str) -> Actor {
    let user = &reply["user"];
    // 응답이 우리가 물은 사용자의 것이어야 한다(다른 사용자의 응답으로 판정하지 않는다)
    if user["id"].as_str() != Some(asked) {
        return Actor::Refused(UNVERIFIED);
    }
    let Some(is_bot) = user["is_bot"].as_bool() else {
        return Actor::Refused(UNVERIFIED);
    };
    // `deleted`는 생략할 때만 «비활성 아님»이다(명세상 정상). `null`·문자열처럼 모양이 틀린 값은 판정할 수 없다
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
    /// 이 사용자가 지금 활성 사람 계정인가. 성공을 기억하지 않으므로 이벤트마다 `users.info`를 한 번 부른다.
    pub(super) async fn reaction_actor(&self, install: &Installation, user: &str) -> Actor {
        if user == install.bot_user || user == SLACKBOT {
            return Actor::Refused(NOT_HUMAN);
        }
        // 봇 토큰 확인(`auth.test`)과 조회가 모두 같은 시한 안에 끝나야 한다: ack 기한을 넘겨 Slack이 같은 이벤트를 다시 보내게 하지 않는다
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
        assert_eq!(
            human(json!({})),
            Actor::Human,
            "deleted를 생략해도 정상이다"
        );
        assert_eq!(human(json!({"deleted": false})), Actor::Human);
        // `is_app_user`는 거절 근거가 아니다
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

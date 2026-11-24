//! 소유자 한 명의 1:1 DM에 승인 카드를 올린다(⑧ «Slack 소유자에게 승인 카드를 전달할 때», docs/reliability.md).
//!
//! 같은 설치(`bot_token`이 속한 팀)에서 인증된 Slack 소유자 한 명으로 `conversations.open`을 호출해 DM 채널을 확인하고, 그 `D…` 채널에 **온전한** 카드를 한 번 올린 뒤
//! **응답이 말한** 채널·`ts`로 영수증을 확인한다. 채널의 스레드·대화에는 아무것도 올리지 않는다. 결과는 증명된 만큼만 가른다:
//! - `Posted`: 응답이 확인한 `D…` 채널과 메시지 `ts`.
//! - `NotSent`: 아무것도 올라가지 않았다(자격을 못 읽음·열기 일시 실패·429·연결 전 실패). 열기는 멱등이라 다시 잡아도 된다.
//! - `Unavailable`: DM 경로가 없거나(주소를 정할 수 없음·열기 확정 실패·`D…`가 아님) 카드를 올릴 수 없다(크기 초과·Slack의 확정 거절). 관리 화면 안내·잘린 카드로 바꿔 올리지 않는다.
//! - `Held`: 올렸는데 올라갔는지 알 수 없다(시간 초과·5xx·확인할 수 없는 응답). 자동으로 다시 올리지 않는다.
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
        /// 봇 토큰이 확인한 설치의 팀 ID(`auth.test`)
        team: String,
        /// 응답이 확인한 1:1 DM 채널(`D…`)
        dm: String,
        /// 응답이 확인한 메시지 `ts`
        ts: String,
    },
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    Unavailable,
    Held,
}

/// `slack:<별칭>/<사용자 ID>` 주체를 (별칭, 사용자 ID)로 나눈다. 모양이 틀리면 `None`.
pub fn slack_user(principal: &PrincipalId) -> Option<(&str, &str)> {
    let (alias, user) = principal.0.strip_prefix("slack:")?.split_once('/')?;
    // 같은 규칙으로 다시 만들어 보아 같을 때만(구분자 주입·소문자 ID 거절)
    (identity::slack(alias, user).as_ref() == Some(principal)).then_some((alias, user))
}

/// `conversations.open` 응답이 말한 1:1 DM 채널(`D…`)
fn dm_channel(value: &Value) -> Option<&str> {
    value["channel"]["id"]
        .as_str()
        .filter(|id| id.starts_with('D') && crate::host::is_slack_id(id))
}

impl SlackSender {
    /// 이 설치(`workspace`)의 소유자 DM에 카드를 올린다. `owner`는 `slack:<이 설치의 별칭>/<U…>`여야 한다.
    pub async fn post_owner_card(
        &self,
        profile: &ProfileId,
        workspace: &str,
        owner: &PrincipalId,
        card: &Card,
    ) -> CardPost {
        // 같은 설치의 Slack 사용자 한 명만 받을 수 있다: 다른 설치·다른 종류의 주체나 모양이 틀린 ID는 DM 주소를 정할 수 없다
        let Some((alias, user)) = slack_user(owner) else {
            return CardPost::Unavailable;
        };
        if alias != workspace || !card_fits(card) {
            return CardPost::Unavailable;
        }
        let BotCheck::Verified(token, auth) = self.creds.check_bot(profile, workspace).await else {
            // 자격을 읽지 못했거나 등록한 팀의 토큰으로 확인하지 못했다 — 아무것도 보내지 않았다
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
                // ok인데 1:1 DM 채널이 아니다: 카드를 다른 곳에 올리지 않는다
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
        // 응답이 말한 채널·ts로만 «올라갔다»를 확정한다. 이 카드는 버튼 없이 의미가 없으니 글자판으로 내려 다시 올리지 않는다
        match raw {
            Raw::Ok(value) => {
                confirms_post(&dm, &value).map_or(CardPost::Held, |(dm, ts)| CardPost::Posted {
                    team: auth.team_id,
                    dm,
                    ts,
                })
            }
            // 서버 사정으로 끝났다: 올라갔는지 알 수 없다
            Raw::Error(code) if super::UNKNOWN_ERRORS.contains(&code.as_str()) => CardPost::Held,
            // 이유를 밝힌 거절(블록 형식 거절 포함): 아무것도 올라가지 않았다
            Raw::Error(_) => CardPost::Unavailable,
            Raw::RateLimited(wait) => CardPost::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(wait),
            },
            // 연결 전 실패·인증 단계의 토큰 거절(교체 중인 옛 토큰): 아무것도 올라가지 않았다
            Raw::BeforeWrite | Raw::TokenRejected => CardPost::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None,
            },
            Raw::Unknown => CardPost::Held,
        }
    }
}

/// `conversations.open`이 DM을 열지 못했다. 이 호출은 아무것도 게시하지 않고 멱등이다: 확정 거절은 DM 경로 없음, 일시 실패는 다시 잡을 수 있다.
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
        // `user_not_found`·`cannot_dm_bot`·`missing_scope`처럼 Slack이 이유를 밝힌 거절: 이 소유자에게 보낼 DM 경로가 없다
        Raw::Error(_) => CardPost::Unavailable,
    }
}

//! Slack 자격(봇·앱 토큰). 이름은 호스트 gateway 절이 워크스페이스 별칭별로 정하고, 값은 **바인딩 소유 프로필**의
//! `bot/<profile>/<name>`에서 읽는다. 다른 프로필의 값으로 폴백하지 않고, 토큰 값이 같다는 이유로 합치지 않는다.
use super::api::{AuthInfo, Body, Raw, SlackApi};
use crate::host::HostConfig;
use bot_core::{
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const BOT_TTL: Duration = Duration::from_secs(30);

/// 확인한 봇 토큰: 값과 그 값으로 확인한 `auth.test` 결과
struct Entry {
    at: Instant,
    token: Arc<String>,
    auth: AuthInfo,
}

type Cache = Mutex<HashMap<(String, String), Entry>>;

/// 봇 토큰을 쓸 수 있는가: 보관소 값이 **등록한 워크스페이스의 것**으로 확인돼야 한다.
pub enum BotCheck {
    Verified(Arc<String>, AuthInfo),
    /// 자격 이름이 없거나 보관소를 읽지 못했다
    Unreadable,
    /// Slack에 `auth.test`로 확인하지 못했다(연결·거절)
    Unverified,
    /// 다른 워크스페이스의 토큰이다
    WrongTeam,
}

pub struct Credentials {
    secrets: Arc<dyn SecretStore>,
    host: Arc<HostConfig>,
    api: SlackApi,
    ttl: Duration,
    bot: Cache,
}

impl Credentials {
    pub fn new(secrets: Arc<dyn SecretStore>, host: Arc<HostConfig>, api: SlackApi) -> Self {
        Self {
            secrets,
            host,
            api,
            ttl: BOT_TTL,
            bot: Mutex::new(HashMap::new()),
        }
    }

    /// 봇 토큰을 보관소에서 다시 읽는 간격을 바꾼다(시험).
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn host(&self) -> &Arc<HostConfig> {
        &self.host
    }

    async fn fetch(&self, profile: &ProfileId, name: &str) -> Option<Arc<String>> {
        let name = SecretName::parse(name)?;
        let handle = self.secrets.for_profile(profile.clone()).await.ok()?;
        let value = handle.resolve(name).await.ok()?;
        let text = String::from_utf8(value.0).ok()?;
        let text = text.trim().to_owned();
        (!text.is_empty()).then(|| Arc::new(text))
    }

    /// 봇 토큰과 그 인증 정보. 잠깐만 기억해 회신마다 보관소를 두드리지 않는다. 기억이 끝나 다시 읽은 값이 **처음 보는 값이거나 바뀐 값**이면
    /// 등록한 워크스페이스(`team_id`)의 토큰인지 `auth.test`로 확인한 뒤에만 쓴다 — 보관소의 토큰을 바꿔도 다른 팀으로 게시되지 않는다.
    /// 발송·카드 갱신·안내가 모두 이 길을 지난다. 같은 값이면 확인을 되풀이하지 않는다.
    pub async fn check_bot(&self, profile: &ProfileId, workspace: &str) -> BotCheck {
        let id = (profile.0.clone(), workspace.to_owned());
        if let Some(entry) = self.bot.lock().unwrap_or_else(|e| e.into_inner()).get(&id)
            && entry.at.elapsed() < self.ttl
        {
            return BotCheck::Verified(entry.token.clone(), entry.auth.clone());
        }
        let Some(registered) = self.host.slack.workspaces.get(workspace) else {
            return BotCheck::Unreadable;
        };
        let Some(fresh) = self.fetch(profile, &registered.bot_token).await else {
            return BotCheck::Unreadable;
        };
        if let Some(entry) = self
            .bot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&id)
            && *entry.token == *fresh
        {
            entry.at = Instant::now();
            return BotCheck::Verified(entry.token.clone(), entry.auth.clone());
        }
        let Some(auth) = self.api.auth_test(&fresh).await else {
            return BotCheck::Unverified;
        };
        if auth.team_id != registered.team_id {
            return BotCheck::WrongTeam;
        }
        self.bot.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Entry {
                at: Instant::now(),
                token: fresh.clone(),
                auth: auth.clone(),
            },
        );
        BotCheck::Verified(fresh, auth)
    }

    /// 이 토큰을 Slack가 거절했다: 기억에서 버린다. 다음 `check_bot`이 보관소를 다시 읽고(값이 바뀌었으면 새 값) 처음 보는 값으로 `auth.test` 확인을 거친다.
    /// 기억이 이미 다른 값으로 바뀌었으면 건드리지 않는다.
    pub fn reject(&self, token: &str) {
        self.bot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| entry.token.as_str() != token);
    }

    /// 봇 토큰으로 Slack Web API를 한 번 부른다. Slack가 그 토큰 자체를 거절하면(`Raw::TokenRejected`) 기억에서 버려 다음 시도가 자격을 다시 읽게 한다 —
    /// 토큰을 바꾸는 30초 동안 옛 토큰으로 계속 보내지 않는다. 발송·카드 게시·카드 갱신·안내가 모두 이 길을 지난다.
    pub async fn call(&self, method: &str, token: &str, body: Body<'_>) -> Raw {
        let raw = self.api.call(method, token, body).await;
        if matches!(raw, Raw::TokenRejected) {
            // 값은 싣지 않는다. 같은 거절이 계속되면(보관소가 아직 옛 값) 이 줄이 되풀이돼 운영자가 원인을 알 수 있다
            tracing::warn!(
                method,
                "Slack가 봇 토큰을 거절했어요 — 기억한 토큰을 버리고 다음 시도에서 보관소를 다시 읽어요"
            );
            self.reject(token);
        }
        raw
    }

    /// 확인된 봇 토큰(`check_bot`). 못 읽거나 확인하지 못하거나 다른 팀의 것이면 `None`이다.
    pub async fn bot_token(&self, profile: &ProfileId, workspace: &str) -> Option<Arc<String>> {
        match self.check_bot(profile, workspace).await {
            BotCheck::Verified(token, _) => Some(token),
            _ => None,
        }
    }

    /// 앱 토큰(소켓용). 연결할 때마다 읽어 교체를 다음 재접속에 반영한다.
    pub async fn app_token(&self, profile: &ProfileId, workspace: &str) -> Option<Arc<String>> {
        let name = self
            .host
            .slack
            .workspaces
            .get(workspace)?
            .app_token
            .as_ref()?;
        self.fetch(profile, name).await
    }

    /// 서명 비밀(HTTP 수신용)
    pub async fn signing_secret(
        &self,
        profile: &ProfileId,
        workspace: &str,
    ) -> Option<Arc<String>> {
        let name = self
            .host
            .slack
            .workspaces
            .get(workspace)?
            .signing_secret
            .as_ref()?;
        self.fetch(profile, name).await
    }
}

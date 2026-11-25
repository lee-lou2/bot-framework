//! `GET /v1/models`, `GET /v1/models/{id}`. 모델은 바인딩의 고정 공개 이름이고, 이 키가 가리키는 입구·프로필에서 **이 주체가 쓸 수 있는** 것만 보인다
//! (멤버 역할 ∩ 프로필 정책 ∩ 에이전트 상한이 `run`을 덮는 모델). 없는 모델과 쓸 수 없는 모델은 같은 404다.
//!
//! 순서: 값을 읽는 것(`created`: 판본 번들을 처음 보관한 시각)을 **먼저** 하고, 응답을 만들기 직전에 키·활성 집합을 다시 확인한 다음 그 시점의 표로 보이는 모델을 정한다.
//! 기다린 뒤에는 새로 확인한 것만 믿는다: 읽는 동안 키가 회전·철회됐거나 멤버·모델이 바뀌었으면 옛 표로 만든 목록이 나가지 않는다.
use super::{auth::AuthError, auth::Authed, error::Failure, render};
use crate::http::HttpGateway;
use bot_core::{
    id::{ProfileId, Revision},
    scope::{RUN_SCOPE, effective},
};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;

/// 목록·조회 한 번에 주는 기본 총 시간. 저장소가 멈추면 일시 장애로 끝낸다.
pub const BUDGET: Duration = Duration::from_secs(10);

impl HttpGateway {
    /// `which`가 None이면 목록, Some이면 그 모델 하나.
    pub(super) async fn models(
        &self,
        authed: &Authed,
        which: Option<&str>,
    ) -> Result<Value, Failure> {
        timeout(self.models_budget, self.models_inner(authed, which))
            .await
            .unwrap_or_else(|_| Err(Failure::unavailable()))
    }

    async fn models_inner(&self, authed: &Authed, which: Option<&str>) -> Result<Value, Failure> {
        // 1) 지금의 표에서 이 입구의 프로필·판본을 정하고 값을 읽는다(기다림은 여기서 끝낸다)
        let routes = self.ingress.routes().map_err(|_| Failure::unavailable())?;
        let Some(profile) = routes.openai_profile(&authed.endpoint).cloned() else {
            return self.settle_models(authed, which, None).await;
        };
        let revision = routes
            .snapshot(&profile)
            .ok_or_else(Failure::model_not_found)?
            .revision
            .clone();
        drop(routes);
        let created = self.created(&profile, &revision).await?;
        // 2) 내보내기 직전 확인과 응답 만들기
        self.settle_models(authed, which, Some((profile, revision, created)))
            .await
    }

    /// 키·활성 집합을 다시 확인한 뒤(이 사이의 기다림은 모두 끝났다) 그 시점의 표로 응답을 정한다.
    async fn settle_models(
        &self,
        authed: &Authed,
        which: Option<&str>,
        read: Option<(ProfileId, Revision, u64)>,
    ) -> Result<Value, Failure> {
        match self.openai_reauthenticate(authed).await {
            Ok(()) => {}
            Err(AuthError::Invalid) => return Err(Failure::invalid_api_key()),
            Err(AuthError::Unavailable) => return Err(Failure::unavailable()),
        }
        // 구판 프로세스의 낡은 표로 목록을 내지 않는다
        self.ingress
            .verify_active()
            .await
            .map_err(|_| Failure::unavailable())?;
        // 여기부터는 기다림이 없다: 이 표로 멤버 여부와 보이는 모델을 정한다
        let routes = self.ingress.routes().map_err(|_| Failure::unavailable())?;
        let Some(profile) = routes.openai_profile(&authed.endpoint) else {
            return none_visible(which);
        };
        let snapshot = routes
            .snapshot(profile)
            .ok_or_else(Failure::model_not_found)?;
        if !snapshot.members.contains_key(&authed.principal) {
            return Err(Failure::access_denied());
        }
        let visible: Vec<&str> = routes
            .openai_models(&authed.endpoint)
            .filter(|(model, _)| which.is_none_or(|wanted| wanted == *model))
            .filter(|(_, owner)| {
                snapshot
                    .bindings
                    .get(&owner.binding)
                    .is_some_and(|binding| {
                        effective(snapshot, &authed.principal, &binding.agent)
                            .is_some_and(|scopes| scopes.contains(RUN_SCOPE))
                    })
            })
            .map(|(model, _)| model)
            .collect();
        if visible.is_empty() {
            return none_visible(which);
        }
        // 읽어 둔 시각은 읽을 때의 판본의 것이다: 그 사이 판본이 바뀌었으면 옛 값을 새 목록에 붙이지 않는다(다시 시도하게 한다)
        let created = match read {
            Some((read_profile, read_revision, created))
                if read_profile == *profile && read_revision == snapshot.revision =>
            {
                created
            }
            _ => return Err(Failure::unavailable()),
        };
        let objects: Vec<Value> = visible
            .into_iter()
            .map(|model| render::model(model, created))
            .collect();
        match which {
            None => Ok(render::model_list(objects)),
            Some(_) => objects
                .into_iter()
                .next()
                .ok_or_else(Failure::model_not_found),
        }
    }

    async fn created(&self, profile: &ProfileId, revision: &Revision) -> Result<u64, Failure> {
        let Some(bundles) = &self.bundles else {
            return Err(Failure::unavailable());
        };
        match bundles.created_ms(profile, revision).await {
            Ok(Some(ms)) if ms >= 0 => Ok(u64::try_from(ms / 1000).unwrap_or(0)),
            _ => Err(Failure::unavailable()),
        }
    }
}

/// 이 주체에게 보이는 모델이 없다: 목록은 비어 있고 단건은 없는 모델과 같은 404다.
fn none_visible(which: Option<&str>) -> Result<Value, Failure> {
    match which {
        None => Ok(render::model_list(Vec::new())),
        Some(_) => Err(Failure::model_not_found()),
    }
}

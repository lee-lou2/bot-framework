//! OpenAI-compatible model listing (`GET /v1/models`) and lookup (`GET /v1/models/{id}`).
//!
//! Fetches `created` first, then re-authenticates the key and verifies the active revision immediately before
//! filtering models against the current route table so key revocation or revision changes never leak stale entries.
use super::{auth::AuthError, auth::Authed, error::Failure, render};
use crate::http::HttpGateway;
use bot_core::{
    id::{ProfileId, Revision},
    scope::{RUN_SCOPE, effective},
};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;

pub const BUDGET: Duration = Duration::from_secs(10);

impl HttpGateway {
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
        self.settle_models(authed, which, Some((profile, revision, created)))
            .await
    }

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
        self.ingress
            .verify_active()
            .await
            .map_err(|_| Failure::unavailable())?;
        // No awaits past this point: compute visibility atomically from the verified table.
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
        // Reject if the revision changed while reading `created` so timestamps never mix across revisions.
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

fn none_visible(which: Option<&str>) -> Result<Value, Failure> {
    match which {
        None => Ok(render::model_list(Vec::new())),
        Some(_) => Err(Failure::model_not_found()),
    }
}

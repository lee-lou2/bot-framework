//! 완료한 자식의 저장 결과를 현재 위임 권한으로 읽는다.
use crate::{active::guard_active, codec::canonical, db::Store, finish::lock_leased};
use bot_core::{
    command::{Delegate, JsonPayload},
    port::Error,
    scope::RUN_SCOPE,
};
use serde_json::{Value, json};
use sqlx::Row;

impl Store {
    pub(crate) async fn delegate_result(&self, d: Delegate) -> crate::db::R<Option<JsonPayload>> {
        let (_, input) = canonical(&JsonPayload(json!({"text": d.instruction}).to_string()))?;
        let mut tx = self.begin_authorized().await?;
        guard_active(&mut tx, &d.parent.profile.0, &d.checked_revision).await?;
        let parent = lock_leased(&mut tx, &d.parent).await?;
        if parent.private || parent.cancel_requested || !d.scopes.contains(RUN_SCOPE) {
            return Err(Error::Denied.into());
        }
        let row = sqlx::query(
            "select agent, input, scopes, state, result, error from bot_runs
             where profile = $1 and parent = $2 and parent_step = $3",
        )
        .bind(&parent.profile)
        .bind(&parent.id)
        .bind(&d.step)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else { return Ok(None) };
        if row.get::<String, _>("agent") != d.child.0 || row.get::<String, _>("input") != input {
            return Err(Error::Conflict.into());
        }
        if !row
            .get::<Vec<String>, _>("scopes")
            .iter()
            .all(|s| d.scopes.contains(s))
        {
            return Err(Error::Denied.into());
        }
        let state: String = row.get("state");
        let body = match state.as_str() {
            "succeeded" => {
                let document = row
                    .get::<Option<String>, _>("result")
                    .ok_or(Error::Invalid)?;
                let result: Value = serde_json::from_str(&document).map_err(|_| Error::Invalid)?;
                json!({"state": state, "result": result})
            }
            "dead" | "cancelled" => {
                json!({"state": state, "error": row.get::<Option<String>, _>("error")})
            }
            _ => return Ok(None),
        };
        tx.commit().await?;
        Ok(Some(JsonPayload(body.to_string())))
    }
}

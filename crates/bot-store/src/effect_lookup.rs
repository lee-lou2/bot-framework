//! 단계 조회(`EffectJournal::lookup`): 이 호출의 그 단계 하나만 같은 인자 지문으로 읽는다. 단계·영수증·감사를 쓰지 않고 전송을 허가하지 않는다
//! (시작만 한 옛 세대의 단계도 보류로 바꾸지 않는다: 그것은 `request`만 한다). 보류 단계의 미소비 운영자 판정(`resolution`)은 실어 주되 반복 조회해도 소비하지 않는다.
//!
//! 검증 순서: 입력(`Invalid`) → run과 현재 claim(`Missing`·`Denied`·`Stale`) → 호출이 기록돼 있고 채널이 묶인 도구의 것인지(`Denied`: 없는 호출과 다른 도구의 호출을
//! 가르지 않는다) → 단계(`None`) → 인자 지문(`Conflict`). 모든 읽기는 (프로필, run, 호출, 단계) 키 안에서만 한다.

use crate::{
    calls::fence,
    codec::ids_ok,
    db::{R, Store},
    effects::{args_digest, status_of},
};
use bot_core::{
    command::{EffectRequest, EffectStatus},
    id::ToolId,
    port::Error,
};
use sqlx::Row;

impl Store {
    pub(crate) async fn effect_lookup(
        &self,
        r: EffectRequest,
        tool: &ToolId,
    ) -> R<Option<EffectStatus>> {
        ids_ok(&[&r.call.0, &r.step, &tool.0])?;
        let digest = args_digest(&r.args)?;
        let at = &r.attempt;
        let mut tx = self.pool.begin().await?;
        fence(&mut tx, at).await?;
        let bound: Option<String> = sqlx::query_scalar(
            "select tool from bot_calls where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&r.call.0)
        .fetch_optional(&mut *tx)
        .await?;
        if bound.as_deref() != Some(tool.0.as_str()) {
            return Err(Error::Denied.into());
        }
        let row = sqlx::query(
            &format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 and step = $4", crate::cols::EFFECTS),
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&r.call.0)
        .bind(&r.step)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.get::<Vec<u8>, _>("args_sha256") != digest {
            return Err(Error::Conflict.into());
        }
        status_of(&row).map(Some)
    }
}

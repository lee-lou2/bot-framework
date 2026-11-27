//! 게이트웨이·관리 핸들이 인증에 쓴 활성 집합 판본을 고정하고 요청 트랜잭션에서 대조한다.
//! `begin_authorized`는 `bot_active` 행의 `FOR SHARE` 잠금으로 판본 교체와 외부 요청을 직렬화한다.
//! 활성 집합의 게시·교체 자체는 `active.rs`가 맡는다.

use crate::db::{R, Store, Tx};
use bot_core::{id::Revision, port::Error};
use std::sync::{Arc, RwLock};

impl Store {
    /// 게이트웨이·관리 역할이 인증에 쓴 입구 매핑과 프로필 집합 판본을 고정한다.
    pub fn for_activation(&self, revision: Revision) -> Self {
        self.for_live_activation(Arc::new(RwLock::new(revision)))
    }

    /// 실행 중 스킬·MCP 즉시 활성화로 갱신되는 공유 활성 집합 판본에 이 핸들을 묶는다.
    pub fn for_live_activation(&self, revision: Arc<RwLock<Revision>>) -> Self {
        Self {
            activation: Some(revision),
            ..self.clone()
        }
    }

    pub(crate) fn pinned_revision(&self) -> Option<Revision> {
        self.activation
            .as_ref()
            .map(|rev| rev.read().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// 읽기 전용 경로의 겹친 방어: 이 핸들이 고정한 활성 집합이 지금도 현재인지 확인한다(`call_step`과 같은 조건). 고정하지 않은 핸들은 그대로 통과한다.
    pub(crate) async fn check_pinned(&self, tx: &mut Tx<'_>) -> R<()> {
        if let Some(expected) = self.pinned_revision() {
            sqlx::query(
                "select 1 from bot_active where singleton and revision = $1 and revision <> ''",
            )
            .bind(&expected.0)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Stale)?;
        }
        Ok(())
    }

    /// 판본 교체와 외부 요청을 같은 행의 잠금으로 직렬화한다. 발급한 전송의 영수증은 교체 후에도 정산한다.
    pub(crate) async fn begin_authorized(&self) -> R<Tx<'_>> {
        let mut tx = self.pool.begin().await?;
        if let Some(expected) = self.pinned_revision() {
            sqlx::query("select 1 from bot_active where singleton and revision = $1 and revision <> '' for share")
                .bind(&expected.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Stale)?;
        }
        Ok(tx)
    }
}

//! 운영자 조치: dead letter 재개와 취소. 둘 다 감사에 남는다(주체·대상·사유 앞부분).
//! 누가 이 조치를 할 수 있는지는 관리 API가 인증으로 정한다. 저장소는 `actor`를 기록한다.

use crate::{
    audit::{self, Entry},
    db::{R, Store},
    finish::*,
};
use bot_core::{id::*, port::Error};
use sqlx::Row;

/// 사유는 감사 판정 칸에 앞 200자만 남긴다.
fn short(reason: &str) -> String {
    reason.chars().take(200).collect()
}

impl Store {
    /// dead letter 재개. 같은 논리 run·효과 키를 유지하고 시도 횟수만 비운다. 다시 집을 때 세대는 오른다.
    pub(crate) async fn resume_run(
        &self,
        profile: &ProfileId,
        run: &RunId,
        actor: &PrincipalId,
        reason: &str,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let moved = sqlx::query(
            "update bot_runs set state = 'ready', attempts = 0, error = null, result = null, finished_ms = null,
                    keep_generation = false, cancel_requested = false, updated_ms = bot_now_ms()
             where profile = $1 and id = $2 and state = 'dead' and not private",
        )
        .bind(&profile.0)
        .bind(&run.0)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved == 0 {
            let exists: bool = sqlx::query_scalar("select exists (select 1 from bot_runs where profile = $1 and id = $2 and not private)")
                .bind(&profile.0)
                .bind(&run.0)
                .fetch_one(&mut *tx)
                .await?;
            return Err(if exists {
                Error::Conflict
            } else {
                Error::Missing
            }
            .into());
        }
        audit::append(
            &mut tx,
            &profile.0,
            &Entry::new(&actor.0, "run.resume", &run.0, &short(reason)),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 취소 요청. 대기 중인 run은 바로 취소로 끝내고, 실행 중인 run은 표시만 한다(소유자가 다음 외부 효과 전에 멈춘다).
    /// 자식도 함께 처리한다. 잠금 순서는 위에서 아래(부모 → 자식)다.
    pub(crate) async fn cancel_run(
        &self,
        profile: &ProfileId,
        run: &RunId,
        actor: &PrincipalId,
        reason: &str,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let target = sqlx::query(
            "select parent from bot_runs where profile = $1 and id = $2 and not private",
        )
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        lock_parent(
            &mut tx,
            &profile.0,
            target.get::<Option<String>, _>("parent").as_deref(),
        )
        .await?;
        // 위임이 쥔 부모 잠금을 기다리는 동안 그 위임의 자식이 커밋될 수 있다.
        // 이미 잠근 부모에게는 새 위임이 생기지 않는다. 새 문장의 스냅숏에서 노드 수가 같아질 때까지 잠근다.
        let mut locked = 0;
        let nodes = loop {
            let nodes = sqlx::query(
                "with recursive tree as (
                     select profile, id, depth from bot_runs where profile = $1 and id = $2
                     union all
                     select c.profile, c.id, c.depth from bot_runs c join tree t on c.profile = t.profile and c.parent = t.id
                 )
                 select r.profile, r.id, r.parent, r.parent_step, r.attempts, r.cancel_requested, r.private, r.state
                 from bot_runs r join tree t on t.profile = r.profile and t.id = r.id
                 order by r.depth, r.id for update of r",
            )
            .bind(&profile.0)
            .bind(&run.0)
            .fetch_all(&mut *tx)
            .await?;
            if nodes.len() == locked {
                break nodes;
            }
            locked = nodes.len();
        };
        let mut touched = 0;
        for node in &nodes {
            let state: String = node.get("state");
            let row = RunRow {
                profile: node.get("profile"),
                id: node.get("id"),
                parent: node.get("parent"),
                parent_step: node.get("parent_step"),
                attempts: node.get("attempts"),
                cancel_requested: true,
                private: false,
            };
            match state.as_str() {
                "ready" | "retry" | "waiting" => {
                    end_run(&mut tx, &row, End::Cancelled("cancelled"), &actor.0).await?;
                    touched += 1;
                }
                "leased" => {
                    sqlx::query("update bot_runs set cancel_requested = true, updated_ms = bot_now_ms() where profile = $1 and id = $2")
                        .bind(&row.profile)
                        .bind(&row.id)
                        .execute(&mut *tx)
                        .await?;
                    touched += 1;
                }
                _ => {}
            }
        }
        let verdict = if touched == 0 {
            "no_effect".to_owned()
        } else {
            short(reason)
        };
        audit::append(
            &mut tx,
            &profile.0,
            &Entry::new(&actor.0, "run.cancel", &run.0, &verdict),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

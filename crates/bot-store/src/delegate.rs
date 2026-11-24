//! 위임: 부모가 직계 자식 에이전트에게 일을 맡긴다. 자식 run 생성과 부모 대기가 한 커밋이다.
//! (부모 run, 단계 key)가 유일해서 부모가 죽었다 다시 와도 같은 자식을 쓴다. 자식 결과는 끝나는 커밋에서 부모에게 **한 번만** 간다(finish.rs).

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store},
    finish::*,
};
use bot_core::{
    command::{Delegate, JsonPayload, Target},
    id::*,
    port::Error,
};
use serde_json::json;
use sqlx::Row;
use std::collections::BTreeSet;

/// 위임 깊이 상한(루트가 0). 에이전트 트리가 이보다 깊을 수 없다.
const MAX_DEPTH: i32 = 8;

/// `child`가 `parent`의 직계 자식 경로인가(`main` → `main/research`, `main/research/x`는 아님).
fn is_direct_child(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

impl Store {
    pub(crate) async fn delegate_run(&self, d: Delegate) -> R<RunId> {
        ids_ok(&[&d.step, &d.child.0, &d.checked_revision.0])?;
        if d.instruction.trim().is_empty() || d.scopes.iter().any(|s| !id_ok(s)) {
            return invalid();
        }
        let target = target_json(&Target::Agent {
            agent: d.child.clone(),
            instruction: d.instruction.clone(),
        })?;
        let (_, input) = canonical(&JsonPayload(json!({"text": d.instruction}).to_string()))?;

        // 잠금 순서: 호스트 집합 펜스 → 활성 판본 → 부모 run. 권한을 판정한 판본이 지금도 활성인 채로 자식 생성과 부모 대기를 커밋한다
        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &d.parent.profile.0, &d.checked_revision).await?;
        let parent = lock_leased(&mut tx, &d.parent).await?;
        if parent.private || parent.cancel_requested {
            return Err(Error::Denied.into());
        }
        let p = sqlx::query(
            "select root, depth, agent, principal, approver, scopes, revision, binding, conv_kind, conv_id, thread
             from bot_runs where profile = $1 and id = $2",
        )
        .bind(&parent.profile)
        .bind(&parent.id)
        .fetch_one(&mut *tx)
        .await?;
        let agent: String = p.get("agent");
        let depth: i32 = p.get("depth");
        let parent_scopes: Vec<String> = p.get("scopes");
        let parent_scopes: BTreeSet<String> = parent_scopes.into_iter().collect();
        if !is_direct_child(&agent, &d.child.0)
            || depth + 1 > MAX_DEPTH
            || !d.scopes.is_subset(&parent_scopes)
        {
            return Err(Error::Denied.into());
        }
        // 같은 단계의 재요청은 같은 자식이다(대상·지시가 같을 때). 다르면 충돌.
        let existing = sqlx::query(
            "select id, agent, input, scopes from bot_runs where profile = $1 and parent = $2 and parent_step = $3",
        )
        .bind(&parent.profile)
        .bind(&parent.id)
        .bind(&d.step)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let stored: Vec<String> = row.get("scopes");
            let same = row.get::<String, _>("agent") == d.child.0
                && row.get::<String, _>("input") == input
                && stored.into_iter().collect::<BTreeSet<_>>() == d.scopes;
            tx.rollback().await?;
            return if same {
                Ok(RunId(row.get("id")))
            } else {
                Err(Error::Conflict.into())
            };
        }
        let child: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        let scopes: Vec<String> = d.scopes.iter().cloned().collect();
        sqlx::query(
            "insert into bot_runs (profile, id, root, parent, parent_step, depth, agent, principal, approver, scopes, revision,
                                   binding, conv_kind, conv_id, thread, target, input)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&parent.profile)
        .bind(&child)
        .bind(p.get::<String, _>("root"))
        .bind(&parent.id)
        .bind(&d.step)
        .bind(depth + 1)
        .bind(&d.child.0)
        .bind(p.get::<String, _>("principal"))
        .bind(p.get::<Option<String>, _>("approver"))
        .bind(&scopes)
        .bind(p.get::<String, _>("revision"))
        .bind(p.get::<String, _>("binding"))
        .bind(p.get::<String, _>("conv_kind"))
        .bind(p.get::<String, _>("conv_id"))
        .bind(p.get::<Option<String>, _>("thread"))
        .bind(&target)
        .bind(&input)
        .execute(&mut *tx)
        .await?;
        park(&mut tx, &parent, &Wait::Child(child.clone())).await?;
        let principal: String = p.get("principal");
        audit::append(
            &mut tx,
            &parent.profile,
            &Entry::new(&principal, "run.delegate", &child, &d.step),
        )
        .await?;
        tx.commit().await?;
        Ok(RunId(child))
    }
}

#[cfg(test)]
mod tests {
    use super::is_direct_child;

    #[test]
    fn only_direct_children_qualify() {
        assert!(is_direct_child("main", "main/research"));
        assert!(!is_direct_child("main", "main"));
        assert!(!is_direct_child("main", "main/research/deep"));
        assert!(!is_direct_child("main", "mainx/research"));
        assert!(!is_direct_child("main/research", "main/other"));
        assert!(!is_direct_child("main", "main/"));
    }
}

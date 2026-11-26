//! 운영자 조치: dead letter 재개와 취소. 관리 API는 인증한 운영자와 화면에서 확인한 실행 조건을 넘긴다.
//! 같은 명령의 재전송은 전이와 함께 기록한 감사 행으로 확인한다.

use crate::{
    audit::{self, Entry},
    codec::{ids_ok, target_from},
    db::{R, Store, Tx},
    finish::*,
};
use bot_core::{
    admin::{RunAction, RunCommand, RunCommandOutcome},
    id::*,
    port::Error,
    state::RunState,
};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};

/// 사유는 감사 판정 칸에 앞 200자만 남긴다.
fn short(reason: &str) -> String {
    reason.chars().take(200).collect()
}

fn state_name(state: RunState) -> &'static str {
    match state {
        RunState::Ready => "ready",
        RunState::Leased => "leased",
        RunState::Waiting => "waiting",
        RunState::Retry => "retry",
        RunState::Succeeded => "succeeded",
        RunState::Dead => "dead",
        RunState::Cancelled => "cancelled",
    }
}

impl Store {
    /// 일반 루트의 상태를 잠근 뒤 확인하고, 성공한 명령은 감사와 함께 커밋한다.
    pub(crate) async fn control_root_run(
        &self,
        actor: &PrincipalId,
        command: &RunCommand,
    ) -> R<RunCommandOutcome> {
        let (profile, run) = (&command.profile.0, &command.run.0);
        ids_ok(&[profile, run, &actor.0, &command.id])?;
        let state = state_name(command.state);
        let action = match command.action {
            RunAction::Resume => "run.resume",
            RunAction::Cancel => "run.cancel",
        };
        let digest = bot_core::canon::sha256(
            &json!([
                "run-command-v1",
                command.id,
                profile,
                run,
                actor.0,
                action,
                state,
                command.generation.0,
                command.claim_epoch.0
            ])
            .to_string(),
        );
        let mut tx = self.begin_authorized().await?;
        let row = sqlx::query(
            "select root, state, generation, claim_epoch, cancel_requested
             from bot_runs where profile = $1 and id = $2 and not private for update",
        )
        .bind(profile)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("root") != *run {
            return Err(Error::Conflict.into());
        }
        // 같은 run의 잠금 아래 재전송을 먼저 찾는다. 전이 후 상태나 세대가 바뀌어도 최초 결과를 돌려준다.
        if let Some(previous) = sqlx::query_scalar::<_, Vec<u8>>(
            "select args_sha256 from bot_audit where profile = $1 and target = $2 and run_command_id = $3",
        )
        .bind(profile)
        .bind(run)
        .bind(&command.id)
        .fetch_optional(&mut *tx)
        .await?
        {
            return if previous == digest {
                Ok(RunCommandOutcome::Applied)
            } else {
                Err(Error::Conflict.into())
            };
        }
        if row.get::<String, _>("state") != state
            || row.get::<i64, _>("generation") as u64 != command.generation.0
            || row.get::<i64, _>("claim_epoch") as u64 != command.claim_epoch.0
        {
            return Err(Error::Stale.into());
        }
        match command.action {
            RunAction::Resume if command.state != RunState::Dead => {
                return Err(Error::Conflict.into());
            }
            RunAction::Cancel
                if !matches!(
                    command.state,
                    RunState::Ready | RunState::Retry | RunState::Waiting | RunState::Leased
                ) || row.get::<bool, _>("cancel_requested") =>
            {
                return Err(Error::Conflict.into());
            }
            _ => {}
        }

        let tree = lock_subtree(&mut tx, profile, run).await?;
        if tree_has_open_hold(&mut tx, profile, run).await? {
            return Ok(RunCommandOutcome::Held);
        }
        match command.action {
            RunAction::Resume => {
                if !move_dead_run(&mut tx, profile, run).await? {
                    return Err(Error::Stale.into());
                }
            }
            RunAction::Cancel => {
                cancel_locked(&mut tx, &tree, actor).await?;
            }
        }
        let mut entry = Entry::new(&actor.0, action, run, "operator_requested");
        entry.args = Some(digest);
        let seq = audit::append(&mut tx, profile, &entry).await?;
        sqlx::query("update bot_audit set run_command_id = $1 where seq = $2")
            .bind(&command.id)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(RunCommandOutcome::Applied)
    }

    /// dead letter 재개. 같은 논리 run·효과 키를 유지하고 시도 횟수만 비운다. 다시 집을 때 세대는 오른다.
    pub(crate) async fn resume_run(
        &self,
        profile: &ProfileId,
        run: &RunId,
        actor: &PrincipalId,
        reason: &str,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        if !move_dead_run(&mut tx, &profile.0, &run.0).await? {
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
        let nodes = lock_subtree(&mut tx, &profile.0, &run.0).await?;
        let touched = cancel_locked(&mut tx, &nodes, actor).await?;
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

async fn move_dead_run(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<bool> {
    let target: Option<Value> = sqlx::query_scalar(
        "update bot_runs set state = 'ready', attempts = 0, error = null, result = null, finished_ms = null,
                keep_generation = false, cancel_requested = false, updated_ms = bot_now_ms()
         where profile = $1 and id = $2 and state = 'dead' and not private returning target",
    )
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(target) = target else {
        return Ok(false);
    };
    target_from(&target)?;
    Ok(true)
}

/// 조회는 상세 스냅숏에서, 명령은 하위 트리 잠금 안에서 같은 열린 발생을 확인한다.
pub(crate) async fn tree_has_open_hold(tx: &mut Tx<'_>, profile: &str, root: &str) -> R<bool> {
    Ok(sqlx::query_scalar(
        "select exists (
             select 1 from bot_runs r join bot_holds h on h.profile = r.profile and h.run = r.id
             where r.profile = $1 and r.root = $2 and h.closed_ms is null
         )",
    )
    .bind(profile)
    .bind(root)
    .fetch_one(&mut **tx)
    .await?)
}

async fn lock_subtree(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Vec<PgRow>> {
    // 위임이 쥔 부모 잠금을 기다리는 동안 그 위임의 자식이 커밋될 수 있다.
    // 이미 잠근 부모에게는 새 위임이 생기지 않는다. 새 문장의 스냅숏에서 노드 수가 같아질 때까지 잠근다.
    let mut locked = 0;
    loop {
        let nodes = sqlx::query(
            "with recursive tree as (
                 select profile, id, depth from bot_runs where profile = $1 and id = $2
                 union all
                 select c.profile, c.id, c.depth from bot_runs c join tree t on c.profile = t.profile and c.parent = t.id
             )
             select r.profile, r.id, r.parent, r.parent_step, r.attempts, r.state
             from bot_runs r join tree t on t.profile = r.profile and t.id = r.id
             order by r.depth, r.id for update of r",
        )
        .bind(profile)
        .bind(run)
        .fetch_all(&mut **tx)
        .await?;
        if nodes.len() == locked {
            return Ok(nodes);
        }
        locked = nodes.len();
    }
}

async fn cancel_locked(tx: &mut Tx<'_>, nodes: &[PgRow], actor: &PrincipalId) -> R<usize> {
    let mut touched = 0;
    for node in nodes {
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
                end_run(tx, &row, End::Cancelled("cancelled"), &actor.0).await?;
                touched += 1;
            }
            "leased" => {
                sqlx::query("update bot_runs set cancel_requested = true, updated_ms = bot_now_ms() where profile = $1 and id = $2")
                    .bind(&row.profile)
                    .bind(&row.id)
                    .execute(&mut **tx)
                    .await?;
                touched += 1;
            }
            _ => {}
        }
    }
    Ok(touched)
}

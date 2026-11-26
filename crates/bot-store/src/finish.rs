//! 종료·대기·깨움: 결과·outbox·부모 깨우기를 한 커밋으로. 잠금 순서는 부모 run → 자식 run → 감사 head.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store, Tx},
    replies,
};
use bot_core::{
    command::{Completion, Finish, RunOutcome},
    id::{Attempt, ClaimEpoch},
    port::Error,
    schedule::quiet_completion,
};
use serde_json::{Value, json};
use sqlx::Row;

/// 잠근 run 한 줄. 종료·펜싱에 필요한 열만 읽는다.
pub(crate) struct RunRow {
    pub profile: String,
    pub id: String,
    pub parent: Option<String>,
    pub parent_step: Option<String>,
    pub attempts: i32,
    pub cancel_requested: bool,
    pub private: bool,
}

pub(crate) enum End<'a> {
    Succeeded(&'a str),
    Dead(&'static str, i32),
    Cancelled(&'static str),
}

pub(crate) enum Wait {
    Child(String),
    Approval,
    Hold,
}

/// 부모를 먼저 잠근다(순서 규칙). 부모가 없으면 아무것도 하지 않는다.
pub(crate) async fn lock_parent(tx: &mut Tx<'_>, profile: &str, parent: Option<&str>) -> R<()> {
    if let Some(parent) = parent {
        sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update")
            .bind(profile)
            .bind(parent)
            .fetch_optional(&mut **tx)
            .await?;
    }
    Ok(())
}

/// 잠금을 기다리지 않는 부모 잠금. 잠겨 있으면 false이고 트랜잭션은 이미 중단된 상태다(호출자가 롤백한다).
pub(crate) async fn try_lock_parent(
    tx: &mut Tx<'_>,
    profile: &str,
    parent: Option<&str>,
) -> R<bool> {
    let Some(parent) = parent else {
        return Ok(true);
    };
    let locked =
        sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update nowait")
            .bind(profile)
            .bind(parent)
            .fetch_optional(&mut **tx)
            .await;
    match locked {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("55P03") => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn parent_of(tx: &mut Tx<'_>, at: &Attempt) -> R<Option<String>> {
    let row = sqlx::query("select parent from bot_runs where profile = $1 and id = $2")
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>("parent")))
}

fn run_row(row: &sqlx::postgres::PgRow) -> RunRow {
    RunRow {
        profile: row.get("profile"),
        id: row.get("id"),
        parent: row.get("parent"),
        parent_step: row.get("parent_step"),
        attempts: row.get("attempts"),
        cancel_requested: row.get("cancel_requested"),
        private: row.get("private"),
    }
}

/// 이 세대의 실행이 지금도 소유자인가: 행 잠금 + 같은 세대 + leased + 리스가 안 끝남.
/// 시각은 잠금을 얻은 **다음 문장**에서 읽는다(잠금을 기다리는 동안 끝난 리스를 통과시키지 않는다).
pub(crate) async fn lock_leased(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    let row = sqlx::query(
        "select profile, id, parent, parent_step, attempts, cancel_requested, private, state, generation, claim_epoch,
                lease_until_ms
         from bot_runs where profile = $1 and id = $2 for update",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(Error::Stale.into());
    };
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?;
    let lease: Option<i64> = row.get("lease_until_ms");
    // 실행기 접근은 세대·claim 토큰·leased 상태·리스 만료를 모두 대조한다(토큰이 없는 attempt는 신원 확인용일 뿐이다)
    let live = row.get::<String, _>("state") == "leased"
        && row.get::<i64, _>("generation") as u64 == at.generation.0
        && at.claim == Some(ClaimEpoch(row.get::<i64, _>("claim_epoch") as u64))
        && lease.is_some_and(|until| until >= now);
    if !live {
        return Err(Error::Stale.into());
    }
    Ok(run_row(&row))
}

/// 깨운다: waiting → ready, 같은 세대로 재개한다. 조건에 맞지 않으면(이미 깨어남·취소) 0행이다.
pub(crate) async fn wake(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    kind: &str,
    child: Option<&str>,
) -> R<bool> {
    let woke = sqlx::query(
        "update bot_runs set state = 'ready', wait_kind = null, wait_child = null, keep_generation = true,
                updated_ms = bot_now_ms()
         where profile = $1 and id = $2 and state = 'waiting' and wait_kind = $3 and wait_child is not distinct from $4",
    )
    .bind(profile)
    .bind(run)
    .bind(kind)
    .bind(child)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(woke == 1)
}

/// 자발적 대기: leased → waiting, 리스를 놓는다. 시도 횟수는 쓰지 않는다. 호출자가 run을 이미 잠갔다.
pub(crate) async fn park(tx: &mut Tx<'_>, run: &RunRow, wait: &Wait) -> R<()> {
    let (kind, child) = match wait {
        Wait::Child(child) => ("child", Some(child.as_str())),
        Wait::Approval => ("approval", None),
        Wait::Hold => ("hold", None),
    };
    sqlx::query(
        "update bot_runs set state = 'waiting', wait_kind = $3, wait_child = $4, lease_until_ms = null, lease_owner = null,
                updated_ms = bot_now_ms()
         where profile = $1 and id = $2",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .bind(kind)
    .bind(child)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// run을 끝내고(성공·dead·취소) 부모가 이 자식을 기다리는 중이면 결과를 체크포인트로 넣어 **한 번만** 깨운다.
/// 호출자가 부모와 이 run을 이미 잠갔다. 마지막에 감사 한 줄(+깨움 한 줄)을 잇는다.
pub(crate) async fn end_run(tx: &mut Tx<'_>, run: &RunRow, end: End<'_>, actor: &str) -> R<()> {
    // 예약 미발송 안내 등 성공에 딸린 쓰기도 같은 트랜잭션이므로 취소가 먼저면 함께 롤백한다.
    if run.cancel_requested && matches!(end, End::Succeeded(_)) {
        return Err(Error::Stale.into());
    }
    let (state, result, error, action, attempts) = match &end {
        End::Succeeded(result) => (
            "succeeded",
            Some(*result),
            None,
            "run.succeeded",
            run.attempts,
        ),
        End::Dead(code, attempts) => ("dead", None, Some(*code), "run.dead", *attempts),
        End::Cancelled(code) => (
            "cancelled",
            None,
            Some(*code),
            "run.cancelled",
            run.attempts,
        ),
    };
    sqlx::query(
        "update bot_runs set state = $3, result = $4, error = $5, attempts = $6, lease_until_ms = null, lease_owner = null,
                wait_kind = null, wait_child = null, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
         where profile = $1 and id = $2",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .bind(state)
    .bind(result)
    .bind(error)
    .bind(attempts)
    .execute(&mut **tx)
    .await?;
    // 끝난 run의 열린 승인은 쓸 수 없다
    crate::approvals::close_for_run(tx, &run.profile, &run.id).await?;
    // 실행 중인 도구의 판정은 닫는다. 별도 회신 원장의 전달 판정은 독립적이다.
    sqlx::query(
        "update bot_holds set closed_ms = bot_now_ms()
         where profile = $1 and run = $2 and kind = 'tool' and closed_ms is null",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .execute(&mut **tx)
    .await?;
    let mut woke = None;
    if let (Some(parent), Some(step)) = (&run.parent, &run.parent_step)
        && wake(tx, &run.profile, parent, "child", Some(&run.id)).await?
    {
        let outcome = match &end {
            End::Succeeded(result) => {
                json!({"state": "succeeded", "result": serde_json::from_str::<Value>(result).unwrap_or(Value::Null)})
            }
            End::Dead(code, _) => json!({"state": "dead", "error": code}),
            End::Cancelled(code) => json!({"state": "cancelled", "error": code}),
        };
        sqlx::query("insert into bot_steps (profile, run, key, body) values ($1, $2, $3, $4) on conflict do nothing")
            .bind(&run.profile)
            .bind(parent)
            .bind(step)
            .bind(outcome.to_string())
            .execute(&mut **tx)
            .await?;
        woke = Some(parent.clone());
    }
    audit::append(
        tx,
        &run.profile,
        &Entry::new(actor, action, &run.id, error.unwrap_or("ok")),
    )
    .await?;
    if let Some(parent) = woke {
        audit::append(
            tx,
            &run.profile,
            &Entry::new(actor, "run.wake", &parent, &run.id),
        )
        .await?;
    }
    Ok(())
}

/// 푸시 어댑터(Slack 등)의 대화에만 회신 행을 쌓는다. CLI·HTTP 결과는 인증된 상태 조회로 가져가고 예약에는 대화가 없다.
/// 순번은 이 run의 기존 행(승인 카드 포함) 다음부터 잇는다. 호출자가 run을 잠갔다.
pub(crate) async fn push_reply(tx: &mut Tx<'_>, profile: &str, run: &str, body: &str) -> R<()> {
    sqlx::query(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, thread, body)
         select r.profile, r.id, n.seq, 'reply:' || r.id || ':' || n.seq::text, r.binding, r.principal,
                r.conv_kind, r.conv_id, r.thread, $3
         from bot_runs r,
              lateral (select coalesce(max(o.seq) + 1, 0) as seq from bot_outbox o where o.profile = r.profile and o.run = r.id) n
         where r.profile = $1 and r.id = $2 and r.conv_kind in ('direct', 'channel')",
    )
    .bind(profile)
    .bind(run)
    .bind(body)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// 성공 완료. 결과·회신(outbox)·부모 깨우기가 한 커밋이다. 옛 세대·리스 만료 뒤는 `Stale`.
    /// 위임한 자식의 답은 저장하지 않는다(부모만 답한다). 자식 결과는 부모 체크포인트로 간다.
    pub(crate) async fn finish_run(&self, at: &Attempt, done: Finish) -> R<Completion> {
        self.finish_run_with(at, done, None).await
    }

    /// `finish_run`과 같되 실행 계량(잘림·사용량)을 같은 커밋에 기록한다. `None`이면 계량 열은 비워 둔다(지어내지 않는다).
    pub(crate) async fn finish_run_with(
        &self,
        at: &Attempt,
        done: Finish,
        outcome: Option<RunOutcome>,
    ) -> R<Completion> {
        let mut tx = self.pool.begin().await?;
        let parent = parent_of(&mut tx, at).await?;
        lock_parent(&mut tx, &at.profile.0, parent.as_deref()).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private {
            return Err(Error::Denied.into());
        }
        if run.cancel_requested {
            end_run(&mut tx, &run, End::Cancelled("cancelled"), "system").await?;
            tx.commit().await?;
            return Ok(Completion::Cancelled);
        }
        let (_, result) = canonical(&done.result)?;
        let answers = done
            .answers
            .iter()
            .map(replies::answer)
            .collect::<R<Vec<_>>>()?;
        if answers.len() > 16 {
            return invalid();
        }
        // 다른 수신자로 가는 예약 발화는 동결한 초안을 소유자가 승인해야 끝난다(`ScheduledReplies`). 일반 완료로 답을 원래 대화로 돌리거나 승인을 건너뛰지 못한다
        // (보낼 것이 없는 조용한 성공만 `dropped`로 일반 완료를 받는다)
        let delivery: Option<String> =
            sqlx::query_scalar("select state from bot_deliveries where profile = $1 and run = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_optional(&mut *tx)
                .await?;
        if delivery.is_some_and(|state| state != "dropped") {
            return Err(Error::Conflict.into());
        }
        end_run(&mut tx, &run, End::Succeeded(&result), "system").await?;
        if let Some(outcome) = outcome {
            sqlx::query(
                "update bot_runs set truncated = $3, usage_prompt = $4, usage_completion = $5 where profile = $1 and id = $2",
            )
            .bind(&run.profile)
            .bind(&run.id)
            .bind(outcome.truncated)
            .bind(outcome.usage.map(|u| i64::try_from(u.prompt).unwrap_or(i64::MAX)))
            .bind(outcome.usage.map(|u| i64::try_from(u.completion).unwrap_or(i64::MAX)))
            .execute(&mut *tx)
            .await?;
        }
        // 조용한 예약: 성공 결과 전체가 `<조용히>`이면 사용자에게 보낼 답이 없다(승인·오류·첨부는 이 경로로 오지 않는다)
        let quiet: bool =
            sqlx::query_scalar("select quiet from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        // 첨부가 있는 답은 조용히 넘기지 않는다(표식 글만 첨부 없이 성공했을 때만 답이 없다)
        let has_attachments = answers.iter().any(|a| has_attachments(a));
        let silent = quiet_completion(quiet, &final_text(&result)) && !has_attachments;
        if run.parent.is_none() && !silent {
            for body in &answers {
                push_reply(&mut tx, &run.profile, &run.id, body).await?;
            }
        }
        tx.commit().await?;
        Ok(Completion::Succeeded)
    }
}

/// 결과 JSON의 최종 글(`{"text": …}`). 글이 없으면 빈 문자열이다.
pub(crate) fn final_text(result: &str) -> String {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|v| {
            v.get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// 정규화한 답 문서에 첨부가 하나라도 있는가.
pub(crate) fn has_attachments(answer: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(answer)
        .ok()
        .and_then(|v| {
            v.get("attachments")
                .and_then(serde_json::Value::as_array)
                .map(|a| !a.is_empty())
        })
        .unwrap_or(false)
}

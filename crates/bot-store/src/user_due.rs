//! 사용자 예약의 발화: `due`가 DB 시각까지 확인할 예약을 읽고, `advance`가 발생 접수(run 생성)·커서·다음 확인 시각을 **한 커밋**으로 쓴다.
//! cron 계산·시간대·DST와 현재 판본(멤버·바인딩·에이전트·권한) 확인은 스케줄러 몫이고, 저장소는 판본 보호·커서·발생 유일키·취소와의 직렬화를 맡는다.
//! 잠금 순서: 활성 판본 → 예약 행. 같은 발생(프로필·예약·예정 시각)은 언제나 같은 run으로 수렴한다.

use crate::{
    accept::{check, insert_run},
    audit::{self, Entry},
    codec::*,
    db::{R, Store, Tx, ms_to_time, time_to_ms},
    user_schedule::{destination_target, origin_target},
};
use bot_core::{
    command::{Accept, Envelope, JsonPayload, Target},
    cron::Cron,
    id::*,
    intake::PushTransportId,
    port::Error,
    schedule::*,
    state::Conversation,
};
use serde_json::json;
use sqlx::{Row, postgres::PgRow};
use std::collections::BTreeSet;

/// 행의 `(다음 확인 시각, 프로필, ID)` 자리.
fn pos_of(row: &PgRow) -> DuePos {
    DuePos {
        at: ms_to_time(row.get("next_ms")),
        profile: ProfileId(row.get("profile")),
        id: ScheduleId(row.get("id")),
    }
}

/// 행의 시각 정의. 읽을 수 없는 cron(저장은 됐지만 지금 해석기가 받지 않는다)이나 맞지 않는 열 조합은 None이다 — 그 한 행만 따로 닫는다.
fn timing_of(row: &PgRow) -> Option<Timing> {
    match (
        row.get::<Option<i64>, _>("at_ms"),
        row.get::<Option<String>, _>("cron"),
    ) {
        (Some(at), None) => Some(Timing::At(ms_to_time(at))),
        (None, Some(cron)) => Some(Timing::Cron {
            expression: Cron::parse(&cron).ok()?,
            timezone: row.get("timezone"),
        }),
        _ => None,
    }
}

/// 행을 읽는다. 저장된 값을 읽을 수 없으면(저장소 장애가 아니라 값이 지금 해석되지 않는다) 닫을 사유를 돌려준다: 시각 정의가 먼저, 그다음 목적지.
fn due_of(row: &PgRow) -> R<Result<DueSchedule, CloseReason>> {
    let Some(timing) = timing_of(row) else {
        return Ok(Err(CloseReason::InvalidTiming));
    };
    let Ok(destination) = destination_target(row) else {
        return Ok(Err(CloseReason::InvalidDestination));
    };
    let ceiling: Vec<String> = row.get("ceiling");
    Ok(Ok(DueSchedule {
        profile: ProfileId(row.get("profile")),
        id: ScheduleId(row.get("id")),
        owner: PrincipalId(row.get("owner")),
        agent: AgentId(row.get("agent")),
        origin: origin_target(row)?,
        ceiling: ceiling.into_iter().collect(),
        spec: ScheduleSpec {
            timing,
            instruction: row.get("instruction"),
            quiet: row.get("quiet"),
            destination,
        },
        cursor: ms_to_time(row.get("cursor_ms")),
        at: ms_to_time(row.get("next_ms")),
    }))
}

/// 발화한 run의 접수 명령: 저장된 정의와 원래 대화에서 만든다(호출자가 바꿔 끼울 수 없다).
fn accept_of(
    row: &PgRow,
    revision: &Revision,
    at_ms: i64,
    scopes: &BTreeSet<String>,
    reply_transport: Option<PushTransportId>,
) -> R<Accept> {
    let id: String = row.get("id");
    let agent = AgentId(row.get("agent"));
    let instruction: String = row.get("instruction");
    Ok(Accept {
        reply_transport,
        event: Envelope {
            profile: ProfileId(row.get("profile")),
            request: RequestId(format!("usr:{id}:{at_ms}")),
            binding: BindingId(row.get("origin_binding")),
            principal: PrincipalId(row.get("owner")),
            conversation: Conversation {
                kind: kind_from(&row.get::<String, _>("origin_kind"))?,
                id: row.get("origin_id"),
                thread: row.get("origin_thread"),
            },
            http: None,
            attachments: Vec::new(),
            body: JsonPayload(json!({ "text": instruction }).to_string()),
        },
        approver: row.get::<Option<String>, _>("approver").map(PrincipalId),
        revision: revision.clone(),
        target: Target::Agent { agent, instruction },
        scopes: scopes.clone(),
    })
}

/// 비활성 프로필의 Close만 판본 없이 받는다: 정말 비활성인지 보호 잠금 안에서 확인한다.
async fn guard(tx: &mut Tx<'_>, cmd: &AdvanceSchedule) -> R<()> {
    match &cmd.checked_revision {
        Some(_)
            if matches!(
                cmd.next,
                NextOccurrence::Close(CloseReason::ProfileInactive)
            ) =>
        {
            invalid()
        }
        Some(rev) => crate::active::guard_active(tx, &cmd.profile.0, rev).await,
        None if matches!(
            cmd.next,
            NextOccurrence::Close(CloseReason::ProfileInactive)
        ) =>
        {
            sqlx::query("select 1 from bot_active where singleton for share")
                .fetch_optional(&mut **tx)
                .await?;
            let active: Option<String> =
                sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
                    .bind(&cmd.profile.0)
                    .fetch_optional(&mut **tx)
                    .await?;
            if active.is_some() {
                return Err(Error::Stale.into());
            }
            Ok(())
        }
        None => invalid(),
    }
}

impl Store {
    /// 옛 집합에 묶인 핸들은 읽지 못한다(`Stale`): 판본 교체 뒤 구판 스케줄러가 같은 쪽을 계속 읽으며 도는 일을 막는다.
    /// `after`가 있으면 `(next_ms, profile, id)`가 그 자리 **뒤**인 행부터 읽는다(keyset): 스케줄러가 처리하지 못해 남겨 둔 예약(등록부 장애로 보류된 것 등)이
    /// 첫 쪽을 계속 채워 뒤의 다른 예약을 막지 않게 한다. 이 읽기는 행을 건드리지 않는다(발생 시각 · 커서 · 다음 확인 시각 그대로).
    /// 바퀴의 끝은 바퀴를 시작할 때(`after`가 없을 때) 지금 확인할 예약의 마지막 행으로 **고정**한다. 이 끝은 키(자리) 상한일 뿐 전체 행의 스냅숏이 아니다:
    /// 읽는 동안 새로 확인 대상이 된 예약은 자리가 끝 뒤에 놓이면 다음 바퀴에 보고, 이어 읽을 자리와 끝 사이에 놓이면 이번 바퀴에도 보일 수 있다.
    /// 발화로 다음 확인 시각이 앞으로 간 예약도 그 사이에 놓이면 같은 바퀴의 뒤쪽 쪽에 다시 나타난다 — 바퀴 하나에서 예약마다 한 번씩만 차례를 주는 것은 부르는 쪽 몫이다.
    /// 계속 새 예약이 들어와도 바퀴는 끝나고(끝 뒤의 행은 이번 바퀴가 읽지 않는다), 앞쪽에 남겨 둔 예약이 다시 보인다. 끝 행까지 읽었으면 `next`는 None이다.
    /// 저장된 시각 정의(cron)를 읽을 수 없는 행은 `unreadable`로 따로 돌려준다(저장소 장애가 아니라 저장된 값이 읽히지 않는 경우만): 한 행의 손상이 쪽 전체를 막지 않는다.
    /// `limit`과 `next`는 정상 + 손상 행을 합친 쪽을 기준으로 하므로 손상 행만 있는 쪽도 앞으로 간다. 이 읽기는 닫지 않는다 — 닫는 것은 스케줄러의 `advance(Close)`다.
    pub(crate) async fn schedule_due(&self, after: Option<DueKey>, limit: u32) -> R<DuePage> {
        let limit = limit.clamp(1, 500);
        let mut tx = self.begin_authorized().await?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let end = match &after {
            Some(key) => key.end.clone(),
            None => {
                let last = sqlx::query(
                    "select next_ms, profile, id from bot_user_schedules where state = 'active' and next_ms <= $1
                     order by next_ms desc, profile desc, id desc limit 1",
                )
                .bind(now)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(last) = last else {
                    tx.rollback().await?;
                    return Ok(DuePage {
                        now: ms_to_time(now),
                        schedules: Vec::new(),
                        unreadable: Vec::new(),
                        next: None,
                    });
                };
                pos_of(&last)
            }
        };
        let from = if after.is_some() {
            "and (next_ms, profile, id) > ($6, $7, $8)"
        } else {
            ""
        };
        let sql = format!(
            "select {} from bot_user_schedules where state = 'active' and next_ms <= $1
               and (next_ms, profile, id) <= ($3, $4, $5) {from} order by next_ms, profile, id limit $2", crate::cols::USER_SCHEDULES
        );
        // 한 행을 더 읽어 «이 바퀴에 읽을 것이 더 남았는지» 본다(끝 행이 쪽의 마지막이면 빈 쪽을 따로 읽지 않는다)
        let mut query = sqlx::query(&sql)
            .bind(now)
            .bind(i64::from(limit) + 1)
            .bind(time_to_ms(end.at))
            .bind(&end.profile.0)
            .bind(&end.id.0);
        if let Some(key) = &after {
            query = query
                .bind(time_to_ms(key.after.at))
                .bind(&key.after.profile.0)
                .bind(&key.after.id.0);
        }
        let mut rows = query.fetch_all(&mut *tx).await?;
        tx.rollback().await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = rows.last().filter(|_| more).map(|last| DueKey {
            after: pos_of(last),
            end,
        });
        // 행마다 해석한다: 값을 읽지 못한 행은 따로 돌려주고(읽기 전용 — 닫는 것은 스케줄러의 advance), 나머지는 그대로 한 쪽에 담는다
        let (mut schedules, mut unreadable) = (Vec::new(), Vec::new());
        for row in &rows {
            match due_of(row)? {
                Ok(ready) => schedules.push(ready),
                Err(reason) => unreadable.push(UnreadableSchedule {
                    profile: ProfileId(row.get("profile")),
                    id: ScheduleId(row.get("id")),
                    cursor: ms_to_time(row.get("cursor_ms")),
                    at: ms_to_time(row.get("next_ms")),
                    reason,
                }),
            }
        }
        Ok(DuePage {
            now: ms_to_time(now),
            schedules,
            unreadable,
            next,
        })
    }

    pub(crate) async fn schedule_advance(&self, cmd: AdvanceSchedule) -> R<Option<RunId>> {
        let (profile, id) = (&cmd.profile.0, &cmd.id.0);
        ids_ok(&[profile, id])?;
        let expected = time_to_ms(cmd.expected_cursor);
        let mut tx = self.begin_authorized().await?;
        guard(&mut tx, &cmd).await?;
        // 안내를 남길 종료는 안내가 들어갈 run의 잠금을 예약 행보다 먼저 잡는다(run → 예약 순서, 일반 회신과 같은 run 잠금 아래에서 순번을 정한다)
        if let NextOccurrence::Close(closing) = &cmd.next
            && closing.notice().is_some()
        {
            crate::user_notice::lock_origin_run(&mut tx, profile, id).await?;
        }
        let row = sqlx::query(&format!(
            "select {} from bot_user_schedules where profile = $1 and id = $2 for update",
            crate::cols::USER_SCHEDULES
        ))
        .bind(profile)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let state: String = row.get("state");
        let cursor: i64 = row.get("cursor_ms");
        // 닫힌 예약: 닫는 요청은 이미 이뤄진 것이고, 발화·대기는 더는 할 수 없다(취소와 접수는 이 잠금으로 직렬화된다)
        if state != "active" {
            tx.rollback().await?;
            return match cmd.next {
                NextOccurrence::Close(_) => Ok(None),
                NextOccurrence::Fire { at, .. } => {
                    // 취소 전에 이미 접수한 같은 발생이면 그 run으로 수렴한다
                    self.occurrence_run(profile, id, time_to_ms(at))
                        .await?
                        .map(Some)
                        .ok_or(Error::Stale.into())
                }
                NextOccurrence::WaitUntil(_) => Err(Error::Stale.into()),
            };
        }
        match cmd.next {
            NextOccurrence::Close(closing) => {
                // 닫는 결정도 읽은 커서를 기준으로 한다: 그 사이 다른 스케줄러가 움직였다면 낡은 결정이니 감사·안내 없이 거절한다(다시 읽고 다시 정한다)
                if cursor != expected {
                    return Err(Error::Stale.into());
                }
                let reason = closing.code();
                close(&mut tx, profile, id, reason).await?;
                // 수신 경로가 없어진 예약은 원래 대화에 고정 안내 한 건을 같은 커밋으로 남긴다
                if let Some(text) = closing.notice() {
                    crate::user_notice::push(&mut tx, &row, text).await?;
                }
                audit::append(
                    &mut tx,
                    profile,
                    &Entry::new("system", "schedule.close", id, reason),
                )
                .await?;
                tx.commit().await?;
                Ok(None)
            }
            NextOccurrence::WaitUntil(until) => {
                let until = time_to_ms(until);
                if cursor != expected {
                    return Err(Error::Stale.into());
                }
                if until <= cursor {
                    return invalid();
                }
                sqlx::query(
                    "update bot_user_schedules set next_ms = $3, updated_ms = bot_now_ms() where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(id)
                .bind(until)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(None)
            }
            NextOccurrence::Fire { at, next, scopes } => {
                let (at_ms, next_ms) = (time_to_ms(at), next.map(time_to_ms));
                if cursor != expected {
                    // 다른 스케줄러가 먼저 움직였다: 같은 발생이면 그 run으로 수렴하고, 아니면 이 입력은 낡았다
                    tx.rollback().await?;
                    return self
                        .occurrence_run(profile, id, at_ms)
                        .await?
                        .map(Some)
                        .ok_or(Error::Stale.into());
                }
                let now: i64 = sqlx::query_scalar("select bot_now_ms()")
                    .fetch_one(&mut *tx)
                    .await?;
                let ceiling: Vec<String> = row.get("ceiling");
                let timed_at: Option<i64> = row.get("at_ms");
                // 시각 규칙: 아직 오지 않았거나(스케줄러 호스트 시계가 빠르다) 커서 뒤로 가는 발생, 한 번짜리의 다른 시각·다음 발생은 받지 않는다
                if at_ms > now
                    || at_ms <= cursor
                    || next_ms.is_some_and(|n| n <= at_ms)
                    || timed_at.is_some_and(|t| t != at_ms || next_ms.is_some())
                    || !scopes.iter().all(|s| ceiling.contains(s))
                {
                    return invalid();
                }
                let revision = cmd.checked_revision.clone().ok_or(Error::Invalid)?;
                // 원래 대화의 발송 책임은 예약을 만든 실행에 고정돼 있다.
                // 푸시 대화인데 발송 키가 비어 있는 구판 실행은 업그레이드 때 매핑을 빠뜨린 경우에만 생긴다.
                // 자동으로 닫으면 운영자 누락으로 사용자 예약이 되돌릴 수 없이 사라지므로, 발화만 거절해 두고 키를 채우면 다음 점검에 다시 발화하게 둔다.
                let origin_transport: Option<String> = sqlx::query_scalar(
                    "select reply_transport from bot_runs where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(row.get::<String, _>("origin_run"))
                .fetch_one(&mut *tx)
                .await?;
                let origin_kind: String = row.get("origin_kind");
                if matches!(origin_kind.as_str(), "direct" | "channel")
                    && origin_transport.is_none()
                {
                    return invalid();
                }
                let reply_transport = origin_transport.map(PushTransportId::new).transpose()?;
                let accept = accept_of(&row, &revision, at_ms, &scopes, reply_transport)?;
                let checked = check(&accept)?;
                let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
                    .fetch_one(&mut *tx)
                    .await?;
                insert_run(
                    &mut tx,
                    &accept,
                    &checked,
                    &run,
                    bot_core::state::RunOrigin::from_recorded(row.get("fire_origin")),
                    Some(&format!("user:{id}")),
                    None,
                )
                .await?;
                sqlx::query(
                    "update bot_runs set quiet = $3, wake_depth = $4, private_memory_read = $5 where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(&run)
                .bind(row.get::<bool, _>("quiet"))
                .bind(row.get::<i16, _>("wake_depth"))
                .bind(row.get::<bool, _>("origin_private_read"))
                .execute(&mut *tx)
                .await?;
                // 명시 목적지가 있으면 발화를 접수한 같은 커밋에서 목적지를 이 run에 고정한다(예약을 나중에 취소해도 바뀌지 않는다)
                if row.get::<Option<String>, _>("dest_binding").is_some() {
                    sqlx::query(
                        "insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id, dest_thread)
                         select $1, $3, dest_binding, dest_kind, dest_id, dest_thread from bot_user_schedules
                         where profile = $1 and id = $2",
                    )
                    .bind(profile)
                    .bind(id)
                    .bind(&run)
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query(
                    "insert into bot_user_occurrences (profile, schedule, at_ms, run) values ($1, $2, $3, $4)",
                )
                .bind(profile)
                .bind(id)
                .bind(at_ms)
                .bind(&run)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "update bot_user_schedules
                     set cursor_ms = $3, next_ms = $4, state = case when $4::bigint is null then 'closed' else state end,
                         close_reason = case when $4::bigint is null then $5 else close_reason end, updated_ms = bot_now_ms()
                     where profile = $1 and id = $2",
                )
                .bind(profile)
                .bind(id)
                .bind(at_ms)
                .bind(next_ms)
                .bind(CloseReason::Finished.code())
                .execute(&mut *tx)
                .await?;
                audit::append(
                    &mut tx,
                    profile,
                    &Entry::new(&accept.event.principal.0, "schedule.fire", &run, "accepted"),
                )
                .await?;
                // 마지막 발생을 접수하면 같은 커밋에서 정상 종료로 닫힌다(감사 한 줄, 안내 없음)
                if next_ms.is_none() {
                    audit::append(
                        &mut tx,
                        profile,
                        &Entry::new("system", "schedule.close", id, CloseReason::Finished.code()),
                    )
                    .await?;
                }
                tx.commit().await?;
                Ok(Some(RunId(run)))
            }
        }
    }

    /// 이미 접수한 발생의 run.
    async fn occurrence_run(&self, profile: &str, schedule: &str, at_ms: i64) -> R<Option<RunId>> {
        let run: Option<String> = sqlx::query_scalar(
            "select run from bot_user_occurrences where profile = $1 and schedule = $2 and at_ms = $3",
        )
        .bind(profile)
        .bind(schedule)
        .bind(at_ms)
        .fetch_optional(&self.pool)
        .await?;
        Ok(run.map(RunId))
    }
}

async fn close(tx: &mut Tx<'_>, profile: &str, id: &str, reason: &str) -> R<()> {
    sqlx::query(
        "update bot_user_schedules set state = 'closed', close_reason = $3, next_ms = null, updated_ms = bot_now_ms()
         where profile = $1 and id = $2",
    )
    .bind(profile)
    .bind(id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

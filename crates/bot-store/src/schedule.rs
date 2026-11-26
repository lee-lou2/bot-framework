//! 예약 저장. cron 계산은 runtime이 하고 저장소는 구간·커서·중복 방지·beat 합치기를 맡는다.
//! - `ActiveRevisions::replace`: 판본 활성화 때 트리거 정의를 구간으로 나눈다. 구간은 (from, until]이고 새 트리거의 커서는 효력 시각이다.
//! - `snapshot`: DB 시각과 커서 이후에 걸친 구간만 돌려준다. 스케줄러 호스트의 시계는 쓰지 않는다.
//! - `accept_occurrence`: 발생 기록·run 생성·커서 이동을 한 커밋으로. 같은 (profile, agent, trigger, 시각)은 기존 run으로 수렴한다.

use crate::{
    accept::{check, insert_run},
    audit::{self, Entry},
    codec::*,
    db::{R, Store, ms_to_time, time_to_ms},
};
use bot_core::{command::*, id::*, port::Error, state::ConversationKind};
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};
use std::collections::{BTreeMap, BTreeSet};

fn spec_from(row: &PgRow) -> R<TriggerSpec> {
    let target: Value = row.get("target");
    let scopes: Vec<String> = row.get("scopes");
    Ok(TriggerSpec {
        approver: row.get::<Option<String>, _>("approver").map(PrincipalId),
        trigger: row.get("trigger"),
        cron: row.get("cron"),
        timezone: row.get("timezone"),
        beat: row.get("beat"),
        principal: PrincipalId(row.get("principal")),
        scopes: scopes.into_iter().collect::<BTreeSet<_>>(),
        target: target_from(&target)?,
    })
}

/// 저장된 구간이 새 정의와 같은가(판본까지).
fn same(row: &PgRow, revision: &Revision, spec: &TriggerSpec) -> R<bool> {
    Ok(row.get::<String, _>("revision") == revision.0 && spec_from(row)? == *spec)
}

fn spec_ok(spec: &TriggerSpec) -> R<()> {
    if bot_core::cron::Cron::parse(&spec.cron).is_err()
        || !id_ok(&spec.timezone)
        || !id_ok(&spec.trigger)
        || !id_ok(&spec.principal.0)
        || spec.scopes.iter().any(|s| !id_ok(s))
        || spec.approver.as_ref().is_some_and(|p| !id_ok(&p.0))
    {
        return invalid();
    }
    target_json(&spec.target).map(|_| ())
}

impl Store {
    pub(crate) async fn schedule_snapshot(&self) -> R<ScheduleSnapshot> {
        let now: i64 = self.now_ms().await?;
        let rows = sqlx::query(
            "select c.profile, c.agent, c.trigger as trig, c.cursor_ms, s.from_ms, s.until_ms, s.revision,
                    s.cron, s.timezone, s.beat, s.principal, s.approver, s.scopes, s.target, s.trigger
             from bot_schedule_cursors c
             join bot_schedule_segments s using (profile, agent, trigger)
             where s.until_ms is null or s.until_ms > c.cursor_ms
             order by c.profile, c.agent, c.trigger, s.from_ms",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut grouped: Vec<ScheduledTrigger> = Vec::new();
        for row in rows {
            let (profile, agent, trigger): (String, String, String) =
                (row.get("profile"), row.get("agent"), row.get("trig"));
            let segment = Segment {
                revision: Revision(row.get("revision")),
                from: ms_to_time(row.get("from_ms")),
                until: row.get::<Option<i64>, _>("until_ms").map(ms_to_time),
                spec: spec_from(&row)?,
            };
            match grouped.last_mut() {
                Some(last)
                    if last.profile.0 == profile
                        && last.agent.0 == agent
                        && last.trigger == trigger =>
                {
                    last.segments.push(segment);
                }
                _ => grouped.push(ScheduledTrigger {
                    profile: ProfileId(profile),
                    agent: AgentId(agent),
                    trigger,
                    cursor: ms_to_time(row.get("cursor_ms")),
                    segments: vec![segment],
                }),
            }
        }
        Ok(ScheduleSnapshot {
            now: ms_to_time(now),
            triggers: grouped,
        })
    }

    /// 발생 한 건을 접수한다. 커서·발생 기록·run이 한 커밋이다. beat는 이미 기다리는 run에 합쳐진다.
    pub(crate) async fn accept_occurrence_run(&self, occ: Occurrence, cmd: Accept) -> R<RunId> {
        let c = check(&cmd)?;
        let (profile, agent, trigger) = (&occ.profile.0, &occ.agent.0, &occ.trigger);
        if occ.profile != cmd.event.profile
            || occ.agent != c.agent
            || cmd.event.conversation.kind != ConversationKind::Scheduled
        {
            return invalid();
        }
        let at = time_to_ms(occ.scheduled_at);
        let mut tx = self.begin_authorized().await?;
        let cursor: i64 = sqlx::query_scalar(
            "select cursor_ms from bot_schedule_cursors where profile = $1 and agent = $2 and trigger = $3 for update",
        )
        .bind(profile)
        .bind(agent)
        .bind(trigger)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let existing: Option<String> = sqlx::query_scalar(
            "select run from bot_occurrences where profile = $1 and agent = $2 and trigger = $3 and at_ms = $4",
        )
        .bind(profile)
        .bind(agent)
        .bind(trigger)
        .bind(at)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(run) = existing {
            tx.rollback().await?;
            return Ok(RunId(run));
        }
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        if at > now {
            return invalid(); // 아직 오지 않은 시각(스케줄러 호스트 시계가 빠르다)
        }
        if at <= cursor {
            return Err(Error::Stale.into()); // 커서 뒤로는 되돌리지 않는다
        }
        let segment = sqlx::query(
            &format!("select {} from bot_schedule_segments where profile = $1 and agent = $2 and trigger = $3
               and from_ms < $4 and (until_ms is null or $4 <= until_ms)", crate::cols::SEGMENTS),
        )
        .bind(profile)
        .bind(agent)
        .bind(trigger)
        .bind(at)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        // 저장된 정의와 다른 명령은 받지 않는다(호출자가 정의를 바꿔 끼울 수 없다)
        let spec = spec_from(&segment)?;
        let stored_target = target_json(&spec.target)?;
        if segment.get::<String, _>("revision") != cmd.revision.0
            || spec.principal != cmd.event.principal
            || spec.approver != cmd.approver
            || spec.scopes != cmd.scopes
            || stored_target != c.target
        {
            return Err(Error::Denied.into());
        }
        // 사전 승인: 이 트리거에 활성 승인이 있으면 정지 상태를 확인하고 새 run을 그 승인과 정지 세대에 묶는다.
        // 정지 중이면 이 발생은 건너뛴다(커서만 앞으로 가고 run은 없다 — 재개 뒤 새 발생부터 받는다).
        let grant = crate::grants::binding_in(&mut tx, profile, agent, trigger, at).await?;
        if matches!(grant, crate::grants::Binding::Paused) {
            sqlx::query("update bot_schedule_cursors set cursor_ms = greatest(cursor_ms, $4) where profile = $1 and agent = $2 and trigger = $3")
                .bind(profile)
                .bind(agent)
                .bind(trigger)
                .bind(at)
                .execute(&mut *tx)
                .await?;
            audit::append(
                &mut tx,
                profile,
                &Entry::new(
                    "system",
                    "schedule.occurrence",
                    &format!("{agent}/{trigger}@{at}"),
                    "skip:trigger_paused",
                ),
            )
            .await?;
            tx.commit().await?;
            return Err(Error::Stale.into());
        }
        let serial = spec.beat.then(|| format!("beat:{agent}:{trigger}"));
        let pending: Option<String> = match &serial {
            Some(key) => {
                sqlx::query_scalar(
                    "select id from bot_runs where profile = $1 and serial_key = $2 and state in ('ready', 'retry')
                       and revision = $3 order by created_ms limit 1 for update",
                )
                .bind(profile)
                .bind(key)
                .bind(&cmd.revision.0)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
        let (run, verdict) = match pending {
            Some(run) => (run, "coalesced"),
            None => {
                let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
                    .fetch_one(&mut *tx)
                    .await?;
                insert_run(
                    &mut tx,
                    &cmd,
                    &c,
                    &run,
                    bot_core::state::RunOrigin::Schedule,
                    Some(trigger),
                    serial.as_deref(),
                )
                .await?;
                if let crate::grants::Binding::Bound { grant, epoch } = &grant {
                    crate::grants::bind_run(&mut tx, profile, &run, grant, *epoch).await?;
                }
                (run, "accepted")
            }
        };
        sqlx::query(
            "insert into bot_occurrences (profile, agent, trigger, at_ms, run, coalesced) values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(profile)
        .bind(agent)
        .bind(trigger)
        .bind(at)
        .bind(&run)
        .bind(verdict == "coalesced")
        .execute(&mut *tx)
        .await?;
        sqlx::query("update bot_schedule_cursors set cursor_ms = greatest(cursor_ms, $4) where profile = $1 and agent = $2 and trigger = $3")
            .bind(profile)
            .bind(agent)
            .bind(trigger)
            .bind(at)
            .execute(&mut *tx)
            .await?;
        audit::append(
            &mut tx,
            profile,
            &Entry::new(&cmd.event.principal.0, "schedule.occurrence", &run, verdict),
        )
        .await?;
        tx.commit().await?;
        Ok(RunId(run))
    }
}

/// 판본 정의 한 벌의 모양을 검사한다(잠금 전).
pub(crate) fn validate_set(set: &ScheduleSet) -> R<()> {
    ids_ok(&[&set.profile.0, &set.revision.0])?;
    let mut keys = BTreeSet::new();
    for spec in &set.triggers {
        spec_ok(spec)?;
        if !keys.insert((agent_of(&spec.target).0.clone(), spec.trigger.clone())) {
            return invalid();
        }
    }
    Ok(())
}

/// 한 프로필의 정적 예약 구간을 새 정의에 맞춘다(호출자가 트랜잭션을 쥔다). 감사는 쓰지 않는다 — 호출자가 감사 head 잠금 순서에 맞춰 쓴다.
/// 새 구간은 이 게시 트랜잭션의 DB 시각부터 시작한다.
pub(crate) async fn sync_in(
    tx: &mut crate::db::Tx<'_>,
    set: &ScheduleSet,
    effective: i64,
) -> R<()> {
    let profile = &set.profile.0;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('bot.schedule.sync:' || $1, 0))")
        .bind(profile)
        .execute(&mut **tx)
        .await?;
    // 아직 시작하지 않은 예정 구간(효력 시각이 미래)은 없던 일로 하고 앞 구간을 다시 연다
    let pending = sqlx::query(
        "delete from bot_schedule_segments where profile = $1 and until_ms is null and from_ms > $2
         returning agent, trigger, from_ms",
    )
    .bind(profile)
    .bind(effective)
    .fetch_all(&mut **tx)
    .await?;
    for gone in pending {
        sqlx::query(
            "update bot_schedule_segments set until_ms = null
             where profile = $1 and agent = $2 and trigger = $3 and until_ms = $4",
        )
        .bind(profile)
        .bind(gone.get::<String, _>("agent"))
        .bind(gone.get::<String, _>("trigger"))
        .bind(gone.get::<i64, _>("from_ms"))
        .execute(&mut **tx)
        .await?;
    }
    let open = sqlx::query(&format!(
        "select {} from bot_schedule_segments where profile = $1 and until_ms is null for update",
        crate::cols::SEGMENTS
    ))
    .bind(profile)
    .fetch_all(&mut **tx)
    .await?;
    let mut open: BTreeMap<(String, String), PgRow> = open
        .into_iter()
        .map(|r| ((r.get("agent"), r.get("trigger")), r))
        .collect();
    for spec in &set.triggers {
        let agent = agent_of(&spec.target).0.clone();
        let target = target_json(&spec.target)?;
        if let Some(row) = open.remove(&(agent.clone(), spec.trigger.clone())) {
            if same(&row, &set.revision, spec)? {
                continue;
            }
            close(tx, profile, &row, effective).await?;
        }
        let scopes: Vec<String> = spec.scopes.iter().cloned().collect();
        sqlx::query(
            "insert into bot_schedule_segments
                (profile, agent, trigger, from_ms, revision, cron, timezone, beat, principal, approver, scopes, target)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(profile)
        .bind(&agent)
        .bind(&spec.trigger)
        .bind(effective)
        .bind(&set.revision.0)
        .bind(&spec.cron)
        .bind(&spec.timezone)
        .bind(spec.beat)
        .bind(&spec.principal.0)
        .bind(spec.approver.as_ref().map(|p| p.0.as_str()))
        .bind(&scopes)
        .bind(&target)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "insert into bot_schedule_cursors (profile, agent, trigger, cursor_ms) values ($1, $2, $3, $4)
             on conflict do nothing",
        )
        .bind(profile)
        .bind(&agent)
        .bind(&spec.trigger)
        .bind(effective)
        .execute(&mut **tx)
        .await?;
    }
    for row in open.values() {
        close(tx, profile, row, effective).await?;
    }
    Ok(())
}

async fn close(tx: &mut crate::db::Tx<'_>, profile: &str, row: &PgRow, at: i64) -> R<()> {
    let from: i64 = row.get("from_ms");
    if at < from {
        return Err(Error::Stale.into());
    }
    // 같은 밀리초에 다시 게시하면 (from, at]은 비어 있다. 그 행을 남기면 새 구간의 시작 키와 충돌한다.
    let sql = if at == from {
        "delete from bot_schedule_segments where profile = $1 and agent = $2 and trigger = $3 and from_ms = $4 and until_ms is null"
    } else {
        "update bot_schedule_segments set until_ms = $4 where profile = $1 and agent = $2 and trigger = $3 and until_ms is null"
    };
    sqlx::query(sql)
        .bind(profile)
        .bind(row.get::<String, _>("agent"))
        .bind(row.get::<String, _>("trigger"))
        .bind(at)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

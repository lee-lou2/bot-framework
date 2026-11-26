//! 사용자 예약(`UserSchedules`)의 생성·목록·취소·후속 확인. 정의와 신원은 저장소가 만든 run에서 복원해 적는다(호출자가 소유자·대화·권한을 고르지 못한다).
//! - 생성·취소는 `CallLedger::start`와 같은 검사(활성 판본·리스·승인 소비)를 하고 호출 완료·감사와 **한 커밋**이다.
//! - 활성 예약은 프로필·소유자당 `MAX_ACTIVE_PER_OWNER`개(모든 에이전트 합산)이고 한도는 소유자별 어드바이저리 잠금으로 직렬화한다.
//! - 후속 확인(`finish_with_wake`)은 신뢰 도구의 결과와 (run, 호출)당 하나뿐인 `at` 예약을 한 커밋에 남기고, 연쇄는 `MAX_WAKE_DEPTH`에서 멈춘다.
//!
//! 발화(`due`·`advance`)는 `user_due.rs`, 목록과 후속 확인은 `user_wake.rs`다.

use crate::{
    audit::{self, Entry},
    calls::{fence, lock_call, lock_root_shared, phase_of, risk_str, tree_flagged},
    codec::*,
    db::{R, Store, Tx, ms_to_time, time_to_ms},
    finish::RunRow,
};
use bot_core::{
    id::*,
    ledger::{CallPhase, StartCall},
    port::Error,
    schedule::*,
    state::Conversation,
};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};

/// 대화에서 예약을 만드는 내장 도구의 ID(브로커의 `schedule.create`·`schedule.cancel`과 같다).
pub(crate) use bot_core::builtin::{CANCEL_TOOL, CREATE_TOOL};

use bot_core::builtin::MAX_SCHEDULE_AHEAD_MS as MAX_AHEAD_MS;

/// 호출 완료 결과에 적는 예약 ID.
pub(crate) fn result_of(id: &str) -> String {
    json!({ "schedule": id }).to_string()
}

/// 취소 호출의 봉인된 요청 지문: 브로커가 인자 `{"id": …}`를 정규화해(JCS) SHA-256한 값.
pub(crate) fn cancel_hash(id: &ScheduleId) -> [u8; 32] {
    let args = json!({ "id": id.0 });
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&args).unwrap_or_default())
}

/// 일반 `start`로는 시작하지 못하는 내장 도구. 승인 소비·실행·호출 완료·감사가 한 커밋인 전용 연산만 쓴다.
pub(crate) fn dedicated_tool(tool: &str) -> bool {
    matches!(
        tool,
        crate::share::SHARE_TOOL | CREATE_TOOL | CANCEL_TOOL | DELIVER_TOOL
    )
}

pub(crate) fn id_in(result: &str) -> Option<String> {
    serde_json::from_str::<Value>(result)
        .ok()?
        .get("schedule")?
        .as_str()
        .map(str::to_owned)
}

/// 검증을 마친 시각 정의: (at_ms, cron, timezone, 다음 확인 시각).
pub(crate) struct Timed {
    at_ms: Option<i64>,
    cron: Option<String>,
    timezone: Option<String>,
    next_ms: i64,
}

pub(crate) fn timed(timing: &Timing, now: i64) -> R<Timed> {
    match timing {
        Timing::At(at) => {
            let ms = time_to_ms(*at);
            if ms <= now || ms > now + MAX_AHEAD_MS {
                return invalid();
            }
            Ok(Timed {
                at_ms: Some(ms),
                cron: None,
                timezone: None,
                next_ms: ms,
            })
        }
        Timing::Cron {
            expression,
            timezone,
        } => {
            if !id_ok(timezone) || timezone.parse::<chrono_tz::Tz>().is_err() {
                return invalid();
            }
            Ok(Timed {
                at_ms: None,
                cron: Some(expression.to_string()),
                timezone: Some(timezone.clone()),
                // 첫 발생은 스케줄러가 계산한다: 곧바로 확인 대상이 된다
                next_ms: now,
            })
        }
    }
}

pub(crate) fn instruction_ok(text: &str) -> R<()> {
    if text.trim().is_empty() || text.len() > MAX_INSTRUCTION_BYTES || text.contains('\0') {
        return invalid();
    }
    Ok(())
}

/// 만든 run에서 복원한 신원.
pub(crate) struct Origin {
    pub(crate) principal: String,
    pub(crate) agent: String,
    pub(crate) binding: String,
    pub(crate) kind: String,
    pub(crate) conv_id: String,
    pub(crate) thread: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) approver: Option<String>,
    pub(crate) tainted: bool,
    /// 루트의 자동 후속 연쇄 깊이(예약으로 발화한 루트는 그 예약의 깊이)
    pub(crate) depth: i16,
    /// 루트가 예약(정적 트리거·사용자 예약·후속 확인)이 발화한 실행인가. 직접 사용자 요청이 아니다.
    pub(crate) scheduled: bool,
}

pub(crate) async fn origin_of(
    tx: &mut Tx<'_>,
    at: &Attempt,
    run: &RunRow,
    shared_flag: Option<bool>,
) -> R<Origin> {
    let me = sqlx::query(
        "select principal, agent, binding, conv_kind, conv_id, thread, scopes, approver, root from bot_runs
         where profile = $1 and id = $2",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .fetch_one(&mut **tx)
    .await?;
    let kind: String = me.get("conv_kind");
    // HTTP 호출자에게는 나중에 보낼 길이 없다
    if kind == "http" {
        return Err(Error::Denied.into());
    }
    let root: String = me.get("root");
    let (depth, scheduled): (i16, bool) = sqlx::query_as(
        "select wake_depth, trigger is not null from bot_runs where profile = $1 and id = $2",
    )
    .bind(&at.profile.0)
    .bind(&root)
    .fetch_one(&mut **tx)
    .await?;
    Ok(Origin {
        principal: me.get("principal"),
        agent: me.get("agent"),
        binding: me.get("binding"),
        kind,
        conv_id: me.get("conv_id"),
        thread: me.get("thread"),
        scopes: me.get("scopes"),
        approver: me.get("approver"),
        tainted: tree_flagged(tx, run, shared_flag).await?,
        depth,
        scheduled,
    })
}

/// 시작 단계의 결과.
pub(crate) enum Claimed {
    /// 이미 끝난 호출의 재접수: 기록된 결과
    Done(String),
    Fresh {
        run: RunRow,
        shared_flag: Option<bool>,
        approval: Option<String>,
    },
}

/// `CallLedger::start`와 같은 검사(활성 판본 → 루트 run → run → 호출 → private 표지 → 승인 소비). 잠금 순서도 같다.
/// `sealed`는 이 연산이 만든 봉인 지문이다: 호출에 기록된 지문(승인한 내용)과 다르면 승인을 소비하기 전에 `Conflict`로 멈춘다.
/// 이미 끝난 호출도 같은 지문일 때만 기록된 결과를 돌려준다(다른 명세로 재접수해 결과만 얻지 못한다).
pub(crate) async fn claim(
    tx: &mut Tx<'_>,
    s: &StartCall,
    tool: &str,
    sealed: &[u8; 32],
) -> R<Claimed> {
    let at = &s.attempt;
    crate::active::guard_active(tx, &at.profile.0, &s.checked_revision).await?;
    let shared_flag = lock_root_shared(tx, at).await?;
    let run = fence(tx, at).await?;
    let call = lock_call(tx, at, &s.call).await?;
    // 원장에 기록된 호출이 이 도구의 것이어야 한다(다른 도구의 호출이나 승인으로 예약을 바꾸지 못한다)
    if call.get::<String, _>("tool") != tool {
        return Err(Error::Denied.into());
    }
    if call.get::<Vec<u8>, _>("args_sha256").as_slice() != sealed.as_slice() {
        return Err(Error::Conflict.into());
    }
    let pointer = match phase_of(&call)? {
        CallPhase::Done(result) => return Ok(Claimed::Done(result.0)),
        CallPhase::Open { approval } => approval,
        _ => return Err(Error::Conflict.into()),
    };
    if tree_flagged(tx, &run, shared_flag).await? && !s.private_recall {
        return Err(Error::Conflict.into()); // 판정한 뒤 다른 실행이 private 기억을 조회했다
    }
    crate::approvals::require_presented(tx, pointer.as_ref(), s.approval.is_some()).await?;
    let approval = match &s.approval {
        // 승인이 있으면 호출이 가리키는 승인·도구·봉인 지문과 맞는지 소비 전에 대조한다(`consume`)
        Some((expected, reasons)) => {
            crate::approvals::consume(tx, at, &s.call, expected, reasons).await?;
            Some(expected.nonce.0.clone())
        }
        None => None,
    };
    Ok(Claimed::Fresh {
        run,
        shared_flag,
        approval,
    })
}

/// 호출을 한 번에 시작해 완료한다(결과는 `{"schedule": <ID>}`).
pub(crate) async fn finish_call(
    tx: &mut Tx<'_>,
    s: &StartCall,
    result: &str,
    approval: Option<&str>,
) -> R<()> {
    let at = &s.attempt;
    sqlx::query(
        "update bot_calls set state = 'done', result = $4, started_generation = $5, owner = $6, started_ms = bot_now_ms(),
                finished_ms = bot_now_ms(), updated_ms = bot_now_ms(), risk = $7, tool_revision = $8,
                approval = coalesce($9, approval)
         where profile = $1 and run = $2 and call = $3",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&s.call.0)
    .bind(result)
    .bind(at.generation.0 as i64)
    .bind(&s.owner)
    .bind(risk_str(s.risk))
    .bind(&s.tool_revision.0)
    .bind(approval)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn audit_done(
    tx: &mut Tx<'_>,
    profile: &str,
    actor: &str,
    call: &str,
    hash: Vec<u8>,
) -> R<()> {
    let mut done = Entry::new(actor, "call.done", call, "done");
    done.args = Some(to32(hash)?);
    audit::append(tx, profile, &done).await?;
    Ok(())
}

/// 소유자별 한도를 직렬화하고 활성 예약 수를 센다.
pub(crate) async fn within_limit(tx: &mut Tx<'_>, profile: &str, owner: &str) -> R<()> {
    sqlx::query(
        "select pg_advisory_xact_lock(hashtextextended('bot.userschedule:' || $1 || ':' || $2, 0))",
    )
    .bind(profile)
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    let active: i64 = sqlx::query_scalar(
        "select count(*) from bot_user_schedules where profile = $1 and owner = $2 and state = 'active'",
    )
    .bind(profile)
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?;
    if active as usize >= MAX_ACTIVE_PER_OWNER {
        return Err(Error::Denied.into());
    }
    Ok(())
}

/// 새 예약 한 줄.
pub(crate) struct New<'a> {
    pub(crate) origin: &'a Origin,
    pub(crate) fire_origin: bot_core::state::RunOrigin,
    pub(crate) call: (&'a str, &'a str),
    pub(crate) destination: Option<&'a DeliveryTarget>,
    pub(crate) timed: Timed,
    pub(crate) instruction: &'a str,
    pub(crate) quiet: bool,
    pub(crate) depth: i16,
    pub(crate) now: i64,
}

pub(crate) async fn insert(tx: &mut Tx<'_>, profile: &str, n: &New<'_>) -> R<String> {
    let id: String = sqlx::query_scalar("select gen_random_uuid()::text")
        .fetch_one(&mut **tx)
        .await?;
    let dest = n.destination;
    sqlx::query(
        "insert into bot_user_schedules
            (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id, origin_thread,
             origin_private_read, approver, ceiling, dest_binding, dest_kind, dest_id, dest_thread,
             at_ms, cron, timezone, instruction, quiet, wake_depth, cursor_ms, next_ms, fire_origin)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)",
    )
    .bind(profile)
    .bind(&id)
    .bind(&n.origin.principal)
    .bind(&n.origin.agent)
    .bind(n.call.0)
    .bind(n.call.1)
    .bind(&n.origin.binding)
    .bind(&n.origin.kind)
    .bind(&n.origin.conv_id)
    .bind(&n.origin.thread)
    .bind(n.origin.tainted)
    .bind(&n.origin.approver)
    .bind(&n.origin.scopes)
    .bind(dest.map(|d| d.binding.0.as_str()))
    .bind(dest.map(|d| kind_str(&d.conversation.kind)))
    .bind(dest.map(|d| d.conversation.id.as_str()))
    .bind(dest.and_then(|d| d.conversation.thread.as_deref()))
    .bind(n.timed.at_ms)
    .bind(&n.timed.cron)
    .bind(&n.timed.timezone)
    .bind(n.instruction)
    .bind(n.quiet)
    .bind(n.depth)
    .bind(n.now)
    .bind(n.timed.next_ms)
    .bind(n.fire_origin.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// 명시 목적지는 같은 프로필의 푸시 대화(direct·channel)만 받는다.
/// 현재 호스트 등록부(`bot-gateway/src/schedule_routes.rs`)와 목적지 적재(`delivery::push_to`)는 Slack으로 한정돼 있다.
/// 다른 푸시 어댑터로 명시 목적지를 열려면 목적지 발송 키(`dest_transport`)도 함께 저장해야 한다.
pub(crate) fn destination_ok(dest: &DeliveryTarget) -> R<()> {
    ids_ok(&[&dest.binding.0, &dest.conversation.id])?;
    if dest
        .conversation
        .thread
        .as_deref()
        .is_some_and(|t| !id_ok(t))
    {
        return invalid();
    }
    match dest.conversation.kind {
        bot_core::state::ConversationKind::Direct | bot_core::state::ConversationKind::Channel => {
            Ok(())
        }
        bot_core::state::ConversationKind::Cli
        | bot_core::state::ConversationKind::Http
        | bot_core::state::ConversationKind::Scheduled => invalid(),
    }
}

pub(crate) fn summary_of(row: &PgRow) -> R<ScheduleSummary> {
    let timing = match (
        row.get::<Option<i64>, _>("at_ms"),
        row.get::<Option<String>, _>("cron"),
    ) {
        (Some(at), None) => Timing::At(ms_to_time(at)),
        (None, Some(cron)) => Timing::Cron {
            expression: bot_core::cron::Cron::parse(&cron).map_err(|_| Error::Invalid)?,
            timezone: row.get("timezone"),
        },
        _ => return invalid(),
    };
    Ok(ScheduleSummary {
        id: ScheduleId(row.get("id")),
        timing,
        quiet: row.get("quiet"),
    })
}

impl Store {
    pub(crate) async fn schedule_create(&self, s: StartCall, spec: ScheduleSpec) -> R<ScheduleId> {
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0])?;
        instruction_ok(&spec.instruction)?;
        // 승인 카드가 보여 준 바로 그 명세만 만든다: 봉인 지문을 만들 수 없는 명세(없는 시간대·표현할 수 없는 시각)는 DB를 만지기 전에 거절한다
        let sealed = spec.seal_create()?.sha256;
        // 다른 수신자로 나가는 답은 발화마다 동결한 초안을 소유자가 승인한 뒤 PG가 보낸다(`ScheduledReplies`): 어떤 경로로도 임의 발송하거나 원래 대화로 바꿔 보내지 않는다.
        if let Some(dest) = &spec.destination {
            destination_ok(dest)?;
        }
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        let (run, shared_flag, approval) = match claim(&mut tx, &s, CREATE_TOOL, &sealed).await? {
            Claimed::Done(result) => {
                tx.rollback().await?;
                return id_in(&result).map(ScheduleId).ok_or(Error::Conflict.into());
            }
            Claimed::Fresh {
                run,
                shared_flag,
                approval,
            } => (run, shared_flag, approval),
        };
        // 다른 수신자에게 가는 예약은 수신자·지시문을 승인한 호출이어야 한다
        if spec.destination.is_some() && approval.is_none() {
            return Err(Error::Denied.into());
        }
        let origin = origin_of(&mut tx, at, &run, shared_flag).await?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let timed = timed(&spec.timing, now)?;
        // 직접 사용자 요청이 만든 예약만 깊이 0이다. 예약 실행(그 위임 자손 포함)이 만든 예약은 루트 깊이 + 1이고 후속 확인과 같은 상한을 쓴다
        let depth = if origin.scheduled {
            origin.depth + 1
        } else {
            0
        };
        if depth > i16::from(MAX_WAKE_DEPTH) {
            return Err(Error::Denied.into());
        }
        within_limit(&mut tx, &at.profile.0, &origin.principal).await?;
        let id = insert(
            &mut tx,
            &at.profile.0,
            &New {
                origin: &origin,
                fire_origin: bot_core::state::RunOrigin::Schedule,
                call: (&at.run.0, &s.call.0),
                destination: spec.destination.as_ref(),
                timed,
                instruction: &spec.instruction,
                quiet: spec.quiet,
                depth,
                now,
            },
        )
        .await?;
        finish_call(&mut tx, &s, &result_of(&id), approval.as_deref()).await?;
        let kind = if matches!(spec.timing, Timing::At(_)) {
            "at"
        } else {
            "cron"
        };
        audit::append(
            &mut tx,
            &at.profile.0,
            &Entry::new(&origin.principal, "schedule.create", &id, kind),
        )
        .await?;
        let hash: Vec<u8> = sqlx::query_scalar(
            "select args_sha256 from bot_calls where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&s.call.0)
        .fetch_one(&mut *tx)
        .await?;
        audit_done(&mut tx, &at.profile.0, &origin.principal, &s.call.0, hash).await?;
        tx.commit().await?;
        Ok(ScheduleId(id))
    }

    pub(crate) async fn schedule_cancel(&self, s: StartCall, id: ScheduleId) -> R<()> {
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0, &id.0])?;
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        let (run, shared_flag, approval) =
            match claim(&mut tx, &s, CANCEL_TOOL, &cancel_hash(&id)).await? {
                Claimed::Done(result) => {
                    tx.rollback().await?;
                    return if id_in(&result).as_deref() == Some(id.0.as_str()) {
                        Ok(())
                    } else {
                        Err(Error::Conflict.into())
                    };
                }
                Claimed::Fresh {
                    run,
                    shared_flag,
                    approval,
                } => (run, shared_flag, approval),
            };
        let origin = origin_of(&mut tx, at, &run, shared_flag).await?;
        // 소유자만: 없는 예약과 남의 예약은 같은 `Missing`이다(존재 여부를 알리지 않는다)
        let row = sqlx::query(
            "select owner, state from bot_user_schedules where profile = $1 and id = $2 for update",
        )
        .bind(&at.profile.0)
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await?
        .filter(|r| r.get::<String, _>("owner") == origin.principal)
        .ok_or(Error::Missing)?;
        let state: String = row.get("state");
        if state == "active" {
            sqlx::query(
                "update bot_user_schedules set state = 'canceled', close_reason = 'canceled', next_ms = null, updated_ms = bot_now_ms()
                 where profile = $1 and id = $2",
            )
            .bind(&at.profile.0)
            .bind(&id.0)
            .execute(&mut *tx)
            .await?;
        }
        finish_call(&mut tx, &s, &result_of(&id.0), approval.as_deref()).await?;
        let verdict = if state == "active" {
            "canceled"
        } else {
            "already_closed"
        };
        audit::append(
            &mut tx,
            &at.profile.0,
            &Entry::new(&origin.principal, "schedule.cancel", &id.0, verdict),
        )
        .await?;
        let hash: Vec<u8> = sqlx::query_scalar(
            "select args_sha256 from bot_calls where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&s.call.0)
        .fetch_one(&mut *tx)
        .await?;
        audit_done(&mut tx, &at.profile.0, &origin.principal, &s.call.0, hash).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// 원래 대화(`DeliveryTarget`)로 옮긴다.
pub(crate) fn origin_target(row: &PgRow) -> R<DeliveryTarget> {
    Ok(DeliveryTarget {
        binding: BindingId(row.get("origin_binding")),
        conversation: Conversation {
            kind: kind_from(&row.get::<String, _>("origin_kind"))?,
            id: row.get("origin_id"),
            thread: row.get("origin_thread"),
        },
    })
}

pub(crate) fn destination_target(row: &PgRow) -> R<Option<DeliveryTarget>> {
    let Some(binding) = row.get::<Option<String>, _>("dest_binding") else {
        return Ok(None);
    };
    Ok(Some(DeliveryTarget {
        binding: BindingId(binding),
        conversation: Conversation {
            kind: kind_from(
                &row.get::<Option<String>, _>("dest_kind")
                    .ok_or(Error::Invalid)?,
            )?,
            id: row
                .get::<Option<String>, _>("dest_id")
                .ok_or(Error::Invalid)?,
            thread: row.get("dest_thread"),
        },
    }))
}

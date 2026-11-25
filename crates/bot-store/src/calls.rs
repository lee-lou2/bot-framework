//! 호출 원장(CallLedger의 호출 쪽): context·begin·start·finish.
//! 모든 전이는 같은 트랜잭션에서 현재 소유자(세대·리스·취소 요청 없음)를 대조하고 감사 한 줄을 잇는다.
//! 보류(held)가 되면 run은 같은 커밋으로 보류 대기로 넘어가 워커 리스를 놓는다(시도 횟수는 쓰지 않는다).

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::*,
    queue,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::*,
    port::Error,
    state::{Risk, RunContext},
};
use sqlx::{Row, postgres::PgRow};

pub(crate) fn risk_str(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Write => "write",
        Risk::Irreversible => "irreversible",
    }
}

pub(crate) fn risk_from(s: &str) -> R<Risk> {
    Ok(match s {
        "read" => Risk::Read,
        "write" => Risk::Write,
        "irreversible" => Risk::Irreversible,
        _ => return invalid(),
    })
}

/// 호출 원장의 소유자 확인. 없는 run은 `Missing`, 다른 프로필에만 있는 run은 `Denied`,
/// 옛 세대·리스 만료·취소 요청은 `Stale`이다. run ID는 프로필마다 따로 쓰이므로(같은 ID가 두 프로필에 있을 수 있다) **자기 프로필의 run을 먼저** 본다:
/// 자기 프로필에 그 run이 있으면 어느 쪽 프로필의 행을 먼저 읽었는지와 무관하게 `Stale`이다. 민감 DM 접수 기록(`private`)은 일반 호출 원장으로 접근할 수 없다(`Denied`):
/// 전용 원장(`PrivateLedger`)만 그 상태를 바꾼다.
pub(crate) async fn fence(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    match lock_leased(tx, at).await {
        Ok(run) if run.private => Err(Error::Denied.into()),
        Ok(run) if run.cancel_requested => Err(Error::Stale.into()),
        Ok(run) => Ok(run),
        Err(Fail::Core(Error::Stale)) => {
            // (자기 프로필에 있는 수, 어느 프로필에든 있는 수): 자기 것이 하나라도 있으면 Stale, 남의 것뿐이면 Denied, 아무 데도 없으면 Missing
            let (mine, any): (i64, i64) = sqlx::query_as(
                "select count(*) filter (where profile = $2), count(*) from bot_runs where id = $1",
            )
            .bind(&at.run.0)
            .bind(&at.profile.0)
            .fetch_one(&mut **tx)
            .await?;
            Err(match (mine, any) {
                (0, 0) => Error::Missing,
                (0, _) => Error::Denied,
                _ => Error::Stale,
            }
            .into())
        }
        Err(other) => Err(other),
    }
}

/// 결과를 기록하는 경로(호출 완료·보류, 단계 정산)의 소유자 확인. 취소 요청이 서 있어도 받는다:
/// 취소는 **새 효과 시작**을 막을 뿐, 이미 시작한 호출의 영수증·보류 기록을 버리면 안 된다(결과 유실·이중 전송).
/// 세대·claim 토큰·leased·리스 만료는 그대로 대조한다.
pub(crate) async fn fence_settle(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    match fence(tx, at).await {
        Err(Fail::Core(Error::Stale)) => {
            // 취소 요청 때문에 거절된 것이면 취소 표시를 무시하고 다시 대조한다
            lock_leased_ignoring_cancel(tx, at).await
        }
        other => other,
    }
}

async fn lock_leased_ignoring_cancel(tx: &mut Tx<'_>, at: &Attempt) -> R<RunRow> {
    let run = lock_leased(tx, at).await?;
    if run.private {
        return Err(Error::Denied.into());
    }
    Ok(run)
}

pub(crate) fn phase_of(row: &PgRow) -> R<CallPhase> {
    let text = |col: &str| row.get::<Option<String>, _>(col);
    Ok(match row.get::<String, _>("state").as_str() {
        "open" => CallPhase::Open {
            approval: text("approval").map(ApprovalId),
        },
        "started" => CallPhase::Started {
            generation: Generation(
                row.get::<Option<i64>, _>("started_generation").unwrap_or(0) as u64
            ),
            owner: text("owner").unwrap_or_default(),
        },
        "done" => CallPhase::Done(JsonPayload(text("result").unwrap_or_default())),
        "denied" => CallPhase::Denied,
        "held" => CallPhase::Held {
            reason: text("reason").unwrap_or_default(),
            resolution: match text("resolution").as_deref() {
                Some("executed") => Some(Resolution::Executed {
                    receipt: text("receipt").unwrap_or_default(),
                }),
                Some("not_executed") => Some(Resolution::NotExecuted),
                _ => None,
            },
        },
        _ => return invalid(),
    })
}

pub(crate) fn record_of(row: &PgRow) -> R<CallRecord> {
    Ok(CallRecord {
        call: CallId(row.get("call")),
        tool: ToolId(row.get("tool")),
        args_sha256: to32(row.get("args_sha256"))?,
        phase: phase_of(row)?,
    })
}

/// 위임한 자식이면 루트 run을 FOR SHARE로 잡고 그 시점의 private 플래그를 돌려준다(플래그를 바꾸는 쪽은 같은 행을 갱신하므로 서로 기다린다).
/// 이 run이 루트면 None이다(이후 fence의 행 잠금이 같은 역할을 한다 — 같은 행을 공유→배타로 올리면 교착하기 때문이다).
pub(crate) async fn lock_root_shared(tx: &mut Tx<'_>, at: &Attempt) -> R<Option<bool>> {
    let root: Option<String> =
        sqlx::query_scalar("select root from bot_runs where profile = $1 and id = $2")
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(root) = root.filter(|r| *r != at.run.0) else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar(
        "select private_memory_read from bot_runs where profile = $1 and id = $2 for share",
    )
    .bind(&at.profile.0)
    .bind(&root)
    .fetch_optional(&mut **tx)
    .await?)
}

/// 요청 트리의 private 기억 조회 플래그. 위임 자식은 루트를 공유 잠금으로 읽은 값(`shared_flag`)을, 루트는 자기 행의 지금 값을 쓴다.
pub(crate) async fn tree_flagged(
    tx: &mut Tx<'_>,
    run: &RunRow,
    shared_flag: Option<bool>,
) -> R<bool> {
    Ok(match shared_flag {
        Some(flag) => flag,
        // 이 run이 루트다: 앞에서 행 잠금을 쥐었으니 지금 값이 곧 최신이다
        None => {
            sqlx::query_scalar(
                "select private_memory_read from bot_runs where profile = $1 and id = $2",
            )
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_one(&mut **tx)
            .await?
        }
    })
}

/// 호출 행을 잠가 읽는다. 없으면 `Missing`.
pub(crate) async fn lock_call(tx: &mut Tx<'_>, at: &Attempt, call: &CallId) -> R<PgRow> {
    sqlx::query(&format!(
        "select {} from bot_calls where profile = $1 and run = $2 and call = $3 for update",
        crate::cols::CALLS
    ))
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&call.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Missing.into())
}

impl Store {
    /// 인증된 실행 맥락. 모델·도구 호출 판정은 호출자가 넘긴 값이 아니라 이 값으로 한다.
    pub(crate) async fn call_context(&self, at: &Attempt) -> R<RunContext> {
        let mut tx = self.pool.begin().await?;
        self.check_pinned(&mut tx).await?;
        fence(&mut tx, at).await?;
        let context = queue::load(&mut tx, &at.profile.0, &at.run.0).await?;
        tx.rollback().await?;
        Ok(context)
    }

    /// 이 run의 기존 호출 한 건을 읽는다(없으면 `None`). 현재 일반 실행의 claim·세대·리스·취소를 확인하고, 호출 생성·승인 소비·감사는 하지 않는다.
    pub(crate) async fn call_lookup(&self, at: &Attempt, call: &CallId) -> R<Option<CallRecord>> {
        ids_ok(&[&call.0])?;
        let mut tx = self.pool.begin().await?;
        self.check_pinned(&mut tx).await?;
        fence(&mut tx, at).await?;
        let row = sqlx::query(&format!(
            "select {} from bot_calls where profile = $1 and run = $2 and call = $3",
            crate::cols::CALLS
        ))
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        row.as_ref().map(record_of).transpose()
    }

    /// 실행 자격과 (profile, run, key) 한 행을 같은 시점에 읽는다.
    pub(crate) async fn call_step(&self, at: &Attempt, key: &str) -> R<Option<JsonPayload>> {
        ids_ok(&[key])?;
        let row = sqlx::query(
            "with owner as (
                 select private, coalesce(state = 'leased' and not cancel_requested
                     and generation = $3 and claim_epoch = $4 and lease_until_ms >= bot_now_ms()
                     and ($6::text is null or exists (
                         select 1 from bot_active where singleton and revision = $6 and revision <> ''
                     )), false) as live
                 from bot_runs where profile = $1 and id = $2
             )
             select owner.private, owner.live, s.body from owner left join bot_steps s
             on owner.live and not owner.private and s.profile = $1 and s.run = $2 and s.key = $5",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(at.generation.0 as i64)
        .bind(at.claim.map(|c| c.0 as i64))
        .bind(key)
        .bind(self.activation.as_ref().map(|revision| &revision.0))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Stale)?;
        if row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        if !row.get::<bool, _>("live") {
            return Err(Error::Stale.into());
        }
        Ok(row.get::<Option<String>, _>("body").map(JsonPayload))
    }

    /// 결과를 돌려주기 직전의 확인: 이 시도의 현재 일반 claim과 그 프로필의 활성 판본을 **한 문장**(같은 DB 시점)으로 읽는다. 잠금은 없다.
    /// 프로필이 활성 집합에 없으면 `None`, 인계·만료·취소·알 수 없는 run은 `Stale`, 민감 DM 접수 기록은 `Denied`다.
    pub(crate) async fn call_active_revision(&self, at: &Attempt) -> R<Option<Revision>> {
        let row = sqlx::query(
            "select r.private, r.cancel_requested,
                    (r.state = 'leased' and r.generation = $3 and r.claim_epoch = $4
                     and r.lease_until_ms >= bot_now_ms()) as live,
                    (select p.revision from bot_active_profiles p where p.profile = $1) as active
             from bot_runs r where r.profile = $1 and r.id = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(at.generation.0 as i64)
        .bind(at.claim.map_or(-1, |c| c.0 as i64))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Stale)?;
        if row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        if !row.get::<bool, _>("live") || row.get::<bool, _>("cancel_requested") {
            return Err(Error::Stale.into());
        }
        Ok(row.get::<Option<String>, _>("active").map(Revision))
    }

    /// 호출을 기록한다((run, call) 유일·멱등). 같은 호출에 도구·인자 해시가 다르면 `Conflict`.
    /// 이미 보류(판정 전)인 호출을 다시 만나면 run을 보류 대기로 넘긴다.
    pub(crate) async fn call_begin(
        &self,
        at: &Attempt,
        call: &CallId,
        tool: &ToolId,
        args: [u8; 32],
    ) -> R<CallRecord> {
        ids_ok(&[&call.0, &tool.0])?;
        // `reply:`·`private:` 접두사는 회신 발송·민감 DM 보류의 합성 ID 자리다: 도구 호출 ID로는 쓸 수 없다(판정 경로가 섞이지 않게)
        if call.0.starts_with("reply:") || call.0.starts_with("private:") {
            return invalid();
        }
        // 예약 회신 호출 ID는 호스트가 소유한다: 그 도구만 쓰고, 그 도구는 그 ID로만 기록한다
        let delivering = tool.0 == bot_core::schedule::DELIVER_TOOL;
        if delivering != (call.0 == bot_core::schedule::DELIVER_CALL) {
            return invalid();
        }
        let mut tx = self.pool.begin().await?;
        let run = fence(&mut tx, at).await?;
        // 동결한 초안이 있고 그 승인 지문과 같은 호출만 기록한다(초안 없이·다른 내용으로 회신 호출을 열지 못한다)
        if delivering {
            crate::delivery::check_begin(&mut tx, &at.profile.0, &at.run.0, &args).await?;
        }
        let fresh = sqlx::query(
            "insert into bot_calls (profile, run, call, tool, args_sha256) values ($1, $2, $3, $4, $5)
             on conflict do nothing",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .bind(&tool.0)
        .bind(args.as_slice())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        let row = lock_call(&mut tx, at, call).await?;
        let record = record_of(&row)?;
        if !fresh && (record.tool != *tool || record.args_sha256 != args) {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        }
        if matches!(
            record.phase,
            CallPhase::Held {
                resolution: None,
                ..
            }
        ) {
            park(&mut tx, &run, &Wait::Hold).await?;
            audit::append(
                &mut tx,
                &run.profile,
                &Entry::new("system", "run.wait", &run.id, &format!("hold:{}", call.0)),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(record)
    }

    /// 현재 세대·취소·private 플래그와 승인을 잠금 안에서 대조하고 단 한 번 시작한다.
    pub(crate) async fn call_start(&self, s: StartCall) -> R<()> {
        let at = &s.attempt;
        ids_ok(&[&s.owner, &s.checked_revision.0])?;
        let mut tx = self.begin_authorized().await?;
        // 잠금 순서: 활성 판본 → 루트 run(공유) → 이 run → 호출 → 승인 → 감사.
        // 권한 검사에 쓴 판본이 지금도 활성이고(교체와 직렬화), 루트의 private 플래그가 판정한 값과 같은 채로 시작을 커밋한다.
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let call = lock_call(&mut tx, at, &s.call).await?;
        // 기억 공유·예약 생성/취소·예약 회신은 승인 소비·변경·호출 완료가 한 커밋인 전용 연산으로만 한다(시작과 변경 사이에 본문이 바뀔 수 있다)
        if crate::user_schedule::dedicated_tool(&call.get::<String, _>("tool")) {
            return Err(Error::Denied.into());
        }
        let CallPhase::Open { approval: pointer } = phase_of(&call)? else {
            return Err(Error::Conflict.into());
        };
        let flagged = tree_flagged(&mut tx, &run, shared_flag).await?;
        if flagged && !s.private_recall {
            return Err(Error::Conflict.into()); // 판정한 뒤 다른 실행이 private 기억을 조회했다
        }
        // 호출에 승인이 연결돼 있으면 그 승인을 내야 시작한다(인자를 비워 승인 소비를 건너뛰지 못한다)
        crate::approvals::require_presented(&mut tx, pointer.as_ref(), s.approval.is_some())
            .await?;
        let mut verdict = "started".to_owned();
        let mut approved = None;
        if let Some((expected, reasons)) = &s.approval {
            crate::approvals::consume(&mut tx, at, &s.call, expected, reasons).await?;
            approved = Some(expected.nonce.0.clone());
            verdict = "started:approved".into();
        }
        sqlx::query(
            "update bot_calls set state = 'started', started_generation = $4, owner = $5, started_ms = bot_now_ms(),
                    updated_ms = bot_now_ms(), risk = $6, tool_revision = $7, approval = coalesce($8, approval)
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&s.call.0)
        .bind(at.generation.0 as i64)
        .bind(&s.owner)
        .bind(risk_str(s.risk))
        .bind(&s.tool_revision.0)
        .bind(approved)
        .execute(&mut *tx)
        .await?;
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let mut entry = Entry::new(&principal, "call.start", &s.call.0, &verdict);
        entry.args = Some(to32(call.get("args_sha256"))?);
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 호출을 닫는다. 열린 호출은 거절(Blocked)로만, 시작한 호출(같은 세대 또는 옛 세대)은 어느 결과로나,
    /// 판정이 있는 보류는 영수증이면 완료·미실행이면 되돌림(또는 거절)으로만 닫는다.
    pub(crate) async fn call_finish(&self, f: FinishCall) -> R<()> {
        self.call_finish_with(f, None).await
    }

    /// `call_finish`와 같되 거절(`Blocked`)을 승인 경로 때문이라고 저장한다: 호출 기록의 `reason`에 닫힌 낱말을 남겨 조회(`ApiRuns::lookup`)가 도구가
    /// 실행되지 않은 이유를 저장된 판정에서 읽게 한다. `Blocked`가 아닌 결과에는 쓸 수 없다(`Conflict`).
    pub(crate) async fn call_finish_with(
        &self,
        f: FinishCall,
        block: Option<bot_core::api::ApprovalBlock>,
    ) -> R<()> {
        if block.is_some() && !matches!(f.outcome, Outcome::Blocked) {
            return Err(Error::Conflict.into());
        }
        let at = &f.attempt;
        let result = match &f.outcome {
            Outcome::Done(payload) => Some(canonical(payload)?.1),
            Outcome::Held(reason) if !id_ok(reason) || reason.len() > 64 => return invalid(),
            _ => None,
        };
        let mut tx = self.pool.begin().await?;
        let run = fence_settle(&mut tx, at).await?;
        let row = lock_call(&mut tx, at, &f.call).await?;
        let phase = phase_of(&row)?;
        let allowed = match (&phase, &f.outcome) {
            (CallPhase::Open { .. }, Outcome::Blocked) => true,
            (CallPhase::Started { generation, .. }, _) => generation.0 <= at.generation.0,
            (
                CallPhase::Held {
                    resolution: Some(Resolution::Executed { .. }),
                    ..
                },
                Outcome::Done(_),
            ) => true,
            (
                CallPhase::Held {
                    resolution: Some(Resolution::NotExecuted),
                    ..
                },
                Outcome::Released | Outcome::Blocked,
            ) => true,
            _ => false,
        };
        if !allowed {
            return Err(Error::Conflict.into());
        }
        let (state, reason, label, verdict) = match &f.outcome {
            Outcome::Done(_) => ("done", None, "call.done", "done".to_owned()),
            Outcome::Released => ("open", None, "call.released", "released".to_owned()),
            Outcome::Held(reason) => (
                "held",
                Some(reason.as_str()),
                "call.held",
                format!("held:{reason}"),
            ),
            Outcome::Blocked => (
                "denied",
                block.map(bot_core::api::ApprovalBlock::code),
                "call.blocked",
                block.map_or_else(|| "blocked".to_owned(), |b| format!("blocked:{}", b.code())),
            ),
        };
        // 풀려 Open으로 돌아가면 판정·시작 표지를 비워 새 시작을 받을 수 있게 한다. 보류가 되면 시작 표지는 복구용으로 남긴다
        sqlx::query(
            "update bot_calls set state = $4, result = $5, reason = $6, updated_ms = bot_now_ms(),
                    finished_ms = case when $4 in ('done', 'denied') then bot_now_ms() else null end,
                    approval = case when $4 = 'open' then null else approval end,
                    started_generation = case when $4 = 'open' then null else started_generation end,
                    owner = case when $4 = 'open' then null else owner end,
                    resolution = case when $4 in ('open', 'held') and $7 then null else resolution end,
                    receipt = case when $4 in ('open', 'held') and $7 then null else receipt end
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&f.call.0)
        .bind(state)
        .bind(result)
        .bind(reason)
        .bind(!matches!(phase, CallPhase::Held { .. }) || state == "open")
        .execute(&mut *tx)
        .await?;
        if state == "held" {
            // 호출을 보류하면 결과가 불명인 단계(시작했지만 영수증이 없다)도 같은 커밋에서 판정 가능한 보류로 옮긴다.
            // 그래야 운영자 보류 목록에 단계별로 나타나고(없으면 run만 기다리고 목록은 비어 있다) 미실행·영수증으로 하나씩 판정할 수 있다.
            sqlx::query(
                "update bot_effects set state = 'held', reason = coalesce(reason, $4), updated_ms = bot_now_ms()
                 where profile = $1 and run = $2 and call = $3 and state = 'started'",
            )
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .bind(&f.call.0)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        }
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let mut entry = Entry::new(&principal, label, &f.call.0, &verdict);
        entry.args = Some(to32(row.get("args_sha256"))?);
        audit::append(&mut tx, &run.profile, &entry).await?;
        if state == "held" {
            park(&mut tx, &run, &Wait::Hold).await?;
            audit::append(
                &mut tx,
                &run.profile,
                &Entry::new("system", "run.wait", &run.id, &format!("hold:{}", f.call.0)),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

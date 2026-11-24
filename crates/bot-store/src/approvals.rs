//! 승인 원장: 요청·결정·대체·소비. 브로커가 판정하고 저장소가 세대 검사·전이·감사·대기 해제를 한 커밋으로 한다.
//! - 요청(`open`)은 run을 승인 대기로 넘기고 워커 리스를 놓는다. 푸시 대화(Slack)에는 카드 회신 행을 같은 커밋으로 쌓는다.
//! - 결정(`decide`)은 승인자 본인만, 승인 세대가 현재일 때만 받는다(옛 버튼 거절). 기다리는 승인이 모두 결정되면 run을 같은 세대로 깨운다.
//! - 승인 필드는 재개해도 바뀌지 않는다. 인계(리스 만료)로 세대가 오르면 옛 승인은 무효(Superseded)다.
//!
//! 잠금 순서: run → 호출 → 승인 → 감사 head(늘 마지막). 만료 청소(`expire_approvals`)도 run마다 이 순서다(승인을 먼저 잠그거나 감사 head를 쥔 채 다음 run을 잠그지 않는다).

use crate::{
    audit::{self, Entry},
    calls::{lock_call, risk_from},
    codec::*,
    db::{R, Store, Tx, ms_to_time},
};
use bot_core::{
    id::*,
    ledger::*,
    port::Error,
    state::{Approval, RunContext},
};
use sqlx::{Row, postgres::PgRow};

fn state_str(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Pending => "pending",
        ApprovalState::Approved => "approved",
        ApprovalState::Consumed => "consumed",
        ApprovalState::Denied => "denied",
        ApprovalState::Expired => "expired",
        ApprovalState::Superseded => "superseded",
        ApprovalState::Revoked => "revoked",
    }
}

pub(crate) fn state_of(s: &str) -> R<ApprovalState> {
    Ok(match s {
        "pending" => ApprovalState::Pending,
        "approved" => ApprovalState::Approved,
        "consumed" => ApprovalState::Consumed,
        "denied" => ApprovalState::Denied,
        "expired" => ApprovalState::Expired,
        "superseded" => ApprovalState::Superseded,
        "revoked" => ApprovalState::Revoked,
        _ => return invalid(),
    })
}

pub(crate) fn approval_of(row: &PgRow) -> R<Approval> {
    Ok(Approval {
        nonce: ApprovalId(row.get("nonce")),
        attempt: Attempt {
            profile: ProfileId(row.get("profile")),
            run: RunId(row.get("run")),
            claim: None,
            generation: Generation(row.get::<i64, _>("generation") as u64),
        },
        call: CallId(row.get("call")),
        requester: PrincipalId(row.get("requester")),
        approver: PrincipalId(row.get("approver")),
        risk: risk_from(&row.get::<String, _>("risk"))?,
        policy_revision: Revision(row.get("policy_revision")),
        tool: ToolId(row.get("tool")),
        tool_revision: Revision(row.get("tool_revision")),
        args_sha256: to32(row.get("args_sha256"))?,
        expires_at: ms_to_time(row.get("expires_ms")),
    })
}

/// 저장된 승인이 지금 다시 계산한 결합값과 같은가. nonce와 만료는 조회 키·시각 검사이므로 비교하지 않는다.
fn same_binding(stored: &Approval, expected: &Approval) -> bool {
    let (a, b) = (stored, expected);
    // claim 토큰은 비교하지 않는다(정상 재개마다 바뀌고, 승인에는 저장하지 않는다)
    (&a.attempt.profile, &a.attempt.run, a.attempt.generation)
        == (&b.attempt.profile, &b.attempt.run, b.attempt.generation)
        && a.call == b.call
        && a.requester == b.requester
        && a.approver == b.approver
        && a.risk == b.risk
        && a.policy_revision == b.policy_revision
        && a.tool == b.tool
        && a.tool_revision == b.tool_revision
        && a.args_sha256 == b.args_sha256
}

/// 기다리던 승인이 하나도 남지 않았으면 승인 대기 중인 run을 깨운다(같은 세대로 재개).
pub(crate) async fn wake_waiters(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<bool> {
    let woke = sqlx::query(
        "update bot_runs set state = 'ready', wait_kind = null, keep_generation = true, not_before_ms = 0, updated_ms = bot_now_ms()
         where profile = $1 and id = $2 and state = 'waiting' and wait_kind = 'approval'
           and not exists (select 1 from bot_approvals a where a.profile = $1 and a.run = $2 and a.state = 'pending')",
    )
    .bind(profile)
    .bind(run)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(woke == 1)
}

/// 승인 행의 run을 먼저 잠가 순서 규칙(run → 승인)을 지키고, 승인 세대가 현재인지 대조한다.
/// 승인 세대와 run 세대가 다르거나 취소됐으면 `Stale`(옛 버튼). run 상태는 대기·재개 어느 쪽이어도 된다.
async fn lock_run_for(tx: &mut Tx<'_>, approval: &PgRow) -> R<()> {
    let run = sqlx::query(
        "select generation, state, cancel_requested from bot_runs where profile = $1 and id = $2 for update",
    )
    .bind(approval.get::<String, _>("profile"))
    .bind(approval.get::<String, _>("run"))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Stale)?;
    let current = run.get::<i64, _>("generation") == approval.get::<i64, _>("generation");
    let live =
        !run.get::<bool, _>("cancel_requested") && run.get::<String, _>("state") != "cancelled";
    if current && live {
        Ok(())
    } else {
        Err(Error::Stale.into())
    }
}

async fn peek(tx: &mut Tx<'_>, nonce: &str) -> R<PgRow> {
    sqlx::query(&format!(
        "select {} from bot_approvals where nonce = $1",
        crate::cols::APPROVALS
    ))
    .bind(nonce)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Missing.into())
}

pub(crate) async fn lock_approval(tx: &mut Tx<'_>, nonce: &str) -> R<PgRow> {
    sqlx::query(&format!(
        "select {} from bot_approvals where nonce = $1 for update",
        crate::cols::APPROVALS
    ))
    .bind(nonce)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Missing.into())
}

/// 호출이 가리키는 승인이 아직 쓸 수 있는데(결정을 받았거나 기다리는 중) 내놓지 않은 채 시작하지 못한다. 대체된 승인만 건너뛴다(정책이 바뀌어 더는 필요 없다).
/// `presented`는 시작을 요청한 쪽이 승인을 냈는가다(낸 승인의 대조·소비는 `consume`이 한다).
pub(crate) async fn require_presented(
    tx: &mut Tx<'_>,
    pointer: Option<&ApprovalId>,
    presented: bool,
) -> R<()> {
    let Some(nonce) = pointer.filter(|_| !presented) else {
        return Ok(());
    };
    let state: Option<String> =
        sqlx::query_scalar("select state from bot_approvals where nonce = $1")
            .bind(&nonce.0)
            .fetch_optional(&mut **tx)
            .await?;
    if state.as_deref() == Some("superseded") {
        Ok(())
    } else {
        Err(Error::Denied.into())
    }
}

/// `start`가 승인을 소비한다(같은 트랜잭션·같은 잠금 안). 소비·시작·감사가 한 커밋이다.
pub(crate) async fn consume(
    tx: &mut Tx<'_>,
    at: &Attempt,
    call: &CallId,
    expected: &Approval,
    reasons: &[String],
) -> R<()> {
    let row = lock_approval(tx, &expected.nonce.0).await?;
    if row.get::<String, _>("profile") != at.profile.0
        || row.get::<String, _>("run") != at.run.0
        || row.get::<String, _>("call") != call.0
    {
        return Err(Error::Denied.into());
    }
    match state_of(&row.get::<String, _>("state"))? {
        ApprovalState::Approved => {}
        ApprovalState::Consumed => return Err(Error::Conflict.into()),
        ApprovalState::Pending | ApprovalState::Denied | ApprovalState::Revoked => {
            return Err(Error::Denied.into());
        }
        ApprovalState::Expired | ApprovalState::Superseded => return Err(Error::Stale.into()),
    }
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?;
    let stored_reasons: Vec<String> = row.get("reasons");
    if now >= row.get::<i64, _>("expires_ms")
        || !same_binding(&approval_of(&row)?, expected)
        || stored_reasons != reasons
    {
        return Err(Error::Stale.into());
    }
    // 저장된 승인이 이 호출의 승인이어야 한다: 호출이 가리키는 승인과 같고, 승인한 도구·봉인 지문이 호출 행에 기록된 것과 같아야 한다.
    // 호출자가 넘긴 값이 아니라 두 저장 행을 대조한다(어긋나면 승인을 소비하지 않는다)
    let recorded = sqlx::query(
        "select tool, args_sha256, approval from bot_calls where profile = $1 and run = $2 and call = $3",
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&call.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Missing)?;
    if recorded.get::<Option<String>, _>("approval").as_deref() != Some(expected.nonce.0.as_str()) {
        return Err(Error::Denied.into());
    }
    if recorded.get::<String, _>("tool") != row.get::<String, _>("tool")
        || recorded.get::<Vec<u8>, _>("args_sha256") != row.get::<Vec<u8>, _>("args_sha256")
    {
        return Err(Error::Conflict.into());
    }
    sqlx::query("update bot_approvals set state = 'consumed' where nonce = $1")
        .bind(&expected.nonce.0)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Store {
    /// 승인 화면·클릭 경로의 읽기 전용 맥락. 대기 중에도 읽는다. 승인자 본인만, 승인 세대가 현재일 때만(옛 버튼은 `Stale`),
    /// claim 토큰 없이 돌려준다(토큰은 실행기만 쥔다).
    pub(crate) async fn approval_context(
        &self,
        nonce: &ApprovalId,
        actor: &PrincipalId,
    ) -> R<RunContext> {
        let mut tx = self.pool.begin().await?;
        let seen = peek(&mut tx, &nonce.0).await?;
        if seen.get::<String, _>("approver") != actor.0 {
            return Err(Error::Denied.into());
        }
        let (profile, run): (String, String) = (seen.get("profile"), seen.get("run"));
        let row = sqlx::query("select generation, state, cancel_requested from bot_runs where profile = $1 and id = $2")
            .bind(&profile)
            .bind(&run)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Stale)?;
        let current = row.get::<i64, _>("generation") == seen.get::<i64, _>("generation");
        let live =
            !row.get::<bool, _>("cancel_requested") && row.get::<String, _>("state") != "cancelled";
        if !(current && live) {
            return Err(Error::Stale.into());
        }
        let mut context = crate::queue::load(&mut tx, &profile, &run).await?;
        context.attempt.claim = None;
        tx.rollback().await?;
        Ok(context)
    }

    pub(crate) async fn approval_get(&self, nonce: &ApprovalId) -> R<StoredApproval> {
        let row = sqlx::query(&format!(
            "select {} from bot_approvals where nonce = $1",
            crate::cols::APPROVALS
        ))
        .bind(&nonce.0)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Missing)?;
        Ok(StoredApproval {
            approval: approval_of(&row)?,
            reasons: row.get("reasons"),
            card: bot_core::command::JsonPayload(row.get("card")),
            state: state_of(&row.get::<String, _>("state"))?,
            // 소유자 DM으로 전달한 카드는 게시를 확인한 뒤에만 영수증이 있다(클릭 대조용)
            delivery: self.cards_receipt(nonce).await?,
        })
    }

    /// 지금 호출에 연결된 승인만 닫는다(승인 화면 뒤 조건이 바뀐 경우). 이미 닫혔으면 새 깨움 없이 Ok, 소비된 것은 `Conflict`.
    pub(crate) async fn approval_supersede(
        &self,
        nonce: &ApprovalId,
        actor: &PrincipalId,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let seen = peek(&mut tx, &nonce.0).await?;
        if seen.get::<String, _>("approver") != actor.0 {
            return Err(Error::Denied.into());
        }
        if state_of(&seen.get::<String, _>("state"))? == ApprovalState::Superseded {
            tx.rollback().await?;
            return Ok(());
        }
        lock_run_for(&mut tx, &seen).await?;
        let row = lock_approval(&mut tx, &nonce.0).await?;
        let call_at = Attempt {
            profile: ProfileId(row.get("profile")),
            run: RunId(row.get("run")),
            claim: None,
            generation: Generation(row.get::<i64, _>("generation") as u64),
        };
        let call = lock_call(&mut tx, &call_at, &CallId(row.get("call"))).await?;
        let pointed = matches!(crate::calls::record_of(&call)?.phase,
            bot_core::ledger::CallPhase::Open { approval: Some(ref n) } if n == nonce);
        match state_of(&row.get::<String, _>("state"))? {
            ApprovalState::Superseded => {
                tx.rollback().await?;
                return Ok(());
            }
            ApprovalState::Pending | ApprovalState::Approved if pointed => {}
            _ => return Err(Error::Conflict.into()),
        }
        sqlx::query("update bot_approvals set state = 'superseded' where nonce = $1")
            .bind(&nonce.0)
            .execute(&mut *tx)
            .await?;
        let profile: String = row.get("profile");
        wake_waiters(&mut tx, &profile, &row.get::<String, _>("run")).await?;
        audit::append(
            &mut tx,
            &profile,
            &Entry::new(&actor.0, "approval.supersede", &nonce.0, "superseded"),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 승인자의 결정. Pending→Approved|Denied, Approved+거절→Revoked, 그 밖은 `Conflict`.
    /// 만료(DB 시각)·옛 세대·취소된 run은 `Stale`. 결정과 대기 run 깨움·감사가 한 커밋이다.
    pub(crate) async fn approval_decide(&self, d: Decision) -> R<ApprovalState> {
        let mut tx = self.begin_authorized().await?;
        let seen = peek(&mut tx, &d.nonce.0).await?;
        if seen.get::<String, _>("approver") != d.actor.0 {
            return Err(Error::Denied.into());
        }
        lock_run_for(&mut tx, &seen).await?;
        let row = lock_approval(&mut tx, &d.nonce.0).await?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        if now >= row.get::<i64, _>("expires_ms") {
            return Err(Error::Stale.into());
        }
        // 소유자 DM으로 전달하는 카드는 게시를 확인하기 전에는 열리지 않은 카드다: 누르거나 API로 결정해도 승인을 소비하지 않는다
        let job: Option<String> =
            sqlx::query_scalar("select state from bot_card_jobs where nonce = $1 for share")
                .bind(&d.nonce.0)
                .fetch_optional(&mut *tx)
                .await?;
        if job.is_some_and(|state| state != "posted") {
            return Err(Error::Conflict.into());
        }
        let next = match (state_of(&row.get::<String, _>("state"))?, d.allow) {
            (ApprovalState::Pending, true) => ApprovalState::Approved,
            (ApprovalState::Pending, false) => ApprovalState::Denied,
            (ApprovalState::Approved, false) => ApprovalState::Revoked,
            _ => return Err(Error::Conflict.into()),
        };
        sqlx::query("update bot_approvals set state = $2, decided_by = $3, decided_ms = bot_now_ms() where nonce = $1")
            .bind(&d.nonce.0)
            .bind(state_str(next))
            .bind(&d.actor.0)
            .execute(&mut *tx)
            .await?;
        let profile: String = row.get("profile");
        wake_waiters(&mut tx, &profile, &row.get::<String, _>("run")).await?;
        let mut entry = Entry::new(&d.actor.0, "approval.decide", &d.nonce.0, state_str(next));
        entry.args = Some(to32(row.get("args_sha256"))?);
        audit::append(&mut tx, &profile, &entry).await?;
        tx.commit().await?;
        Ok(next)
    }

    /// 만료된 승인을 닫고 기다리던 run을 깨운다. 다시 들어온 호출은 만료를 알고 새 승인을 연다. 워커가 집기 전에 부른다.
    ///
    /// **run마다 한 트랜잭션**이다(승인을 한꺼번에 잠그지 않는다). 소유자의 결정·취소·시작과 같은 순서 `run → 승인 → … → 감사 head`를 지킨다:
    /// 승인 행을 먼저 잠그고 run을 기다리거나, 감사 head를 쥔 채 다음 run을 잠그면 같은 run을 반대 순서로 잠그는 쪽(소유자의 클릭)과 PG 교착이 난다.
    /// 만료한 run은 잠금 없이 읽어 모으고, 각 run은 **잠근 뒤에 다시 판정**한다(그 사이 결정·대체·소비된 승인은 건드리지 않는다).
    /// 한 run이 실패하면 거기서 멈추고 오류를 돌려준다(앞선 run은 이미 커밋됐다). 다음 호출이 이어서 한다.
    pub(crate) async fn expire_approvals(&self) -> R<()> {
        let due = sqlx::query(
            "select profile, run from bot_approvals
             where state in ('pending', 'approved') and expires_ms <= (select bot_now_ms())
             group by profile, run order by profile, run",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in &due {
            let (profile, run): (String, String) = (row.get("profile"), row.get("run"));
            self.expire_run_approvals(&profile, &run).await?;
        }
        Ok(())
    }

    /// 한 run의 만료한 승인을 닫는다: `run FOR UPDATE → 그 run의 만료 승인 UPDATE → 깨움 → 감사(늘 마지막)`.
    async fn expire_run_approvals(&self, profile: &str, run: &str) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let locked =
            sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update")
                .bind(profile)
                .bind(run)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            return Ok(()); // run이 이미 없다(승인도 함께 지워졌다)
        }
        let mut gone: Vec<String> = sqlx::query_scalar(
            "update bot_approvals set state = 'expired'
             where profile = $1 and run = $2 and state in ('pending', 'approved') and expires_ms <= (select bot_now_ms())
             returning nonce",
        )
        .bind(profile)
        .bind(run)
        .fetch_all(&mut *tx)
        .await?;
        if gone.is_empty() {
            return Ok(()); // 잠그는 사이 결정·대체·소비됐다
        }
        gone.sort();
        wake_waiters(&mut tx, profile, run).await?;
        for nonce in &gone {
            audit::append(
                &mut tx,
                profile,
                &Entry::new("system", "approval.expire", nonce, "expired"),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// 리스 만료 인계: 옛 세대가 연 승인은 모두 무효다(옛 버튼 거절). 호출자가 run을 잠갔다.
pub(crate) async fn supersede_generation(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    generation: i64,
) -> R<()> {
    let gone = sqlx::query(
        "update bot_approvals set state = 'superseded'
         where profile = $1 and run = $2 and generation <= $3 and state in ('pending', 'approved') returning nonce",
    )
    .bind(profile)
    .bind(run)
    .bind(generation)
    .fetch_all(&mut **tx)
    .await?;
    for row in gone {
        audit::append(
            tx,
            profile,
            &Entry::new(
                "system",
                "approval.supersede",
                &row.get::<String, _>("nonce"),
                "owner_lost",
            ),
        )
        .await?;
    }
    Ok(())
}

/// 끝난 run의 열린 승인을 닫는다(취소·dead·성공). 호출자가 run을 잠갔다.
pub(crate) async fn close_for_run(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<()> {
    sqlx::query(
        "update bot_approvals set state = 'superseded' where profile = $1 and run = $2 and state in ('pending', 'approved')",
    )
    .bind(profile)
    .bind(run)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

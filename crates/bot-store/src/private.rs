//! 민감 DM 접수 원장(PrivateLedger). 본문·인자는 저장하지 않는다: 접수 키와 프로필별 HMAC 지문만 남는다.
//! 접수 기록은 워커가 집지 않는 `private` run이다(목록·이력에 내용이 없다). 일반 큐·호출·기억·효과·회신 원장으로는
//! 접근할 수 없고(`Denied`) 이 원장만 상태를 바꾼다.
//!
//! 상태: 예약(leased, 시작 표시 없음) → 시작(leased + `private_started_ms`) → 완료(succeeded) | 거절(dead) | 보류(waiting·hold).
//! - 실행기가 명시적으로 반납한 행(`RetryBeforeStart`: 시작 전, `NotSent`: 시작 뒤 전송 전 실패 확정)만 ready가 되고,
//!   같은 입력의 재전달이 새 claim 토큰(세대 유지)으로 다시 점유한다.
//! - **리스가 끝난 leased는 시작 표시와 관계없이 내구 보류다**(재전달·청소 어느 쪽이든). 자동으로 다시 실행하거나 거절·미전송으로
//!   바꾸지 않는다: 그 뒤는 운영자 증거 판정뿐이다. 늦게 온 성공(`Completed`)은 시작이 실제로 확인된 행만 받는다.
//!   결과 기록(`finish`)이 재전달보다 먼저 와도 같다: 행 잠금 안에서 DB 시각으로 만료를 보고 보류·감사를 커밋한 뒤 판정한다.
//! - 결과 기록은 세대·claim 토큰을 대조한다. 시작은 살아 있는 리스가 필요하다. 영수증·오류·인자는 저장하지 않는다.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store, Tx},
};
use bot_core::{
    id::*,
    ledger::{PrivateOutcome, PrivatePermit, PrivateRequest, PrivateStart},
    port::Error,
    state::ConversationKind,
};
use sqlx::{Row, postgres::PgRow};

fn grant(
    profile: &ProfileId,
    run: &str,
    generation: i64,
    epoch: i64,
    allow_pass: bool,
) -> PrivatePermit {
    PrivatePermit::Start {
        attempt: Attempt {
            profile: profile.clone(),
            run: RunId(run.to_owned()),
            generation: Generation(generation as u64),
            claim: Some(ClaimEpoch(epoch as u64)),
        },
        allow_pass,
    }
}

/// 보냈는지 모르는 접수를 내구 보류로 옮긴다(운영자가 판정할 때까지 다시 실행하지 않는다).
async fn to_hold(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<()> {
    sqlx::query(
        "update bot_runs set state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null,
                updated_ms = bot_now_ms() where profile = $1 and id = $2",
    )
    .bind(profile)
    .bind(run)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 잠근 행의 세대·claim 토큰이 permit과 같은지 본다(리스는 보지 않는다).
fn owned(row: &PgRow, at: &Attempt) -> bool {
    row.get::<i64, _>("generation") as u64 == at.generation.0
        && at.claim == Some(ClaimEpoch(row.get::<i64, _>("claim_epoch") as u64))
}

impl Store {
    pub(crate) async fn private_begin(&self, r: PrivateRequest) -> R<PrivatePermit> {
        let k = &r.key;
        ids_ok(&[
            &k.profile.0,
            &k.binding.0,
            &k.principal.0,
            &k.request.0,
            &r.revision.0,
            &r.agent.0,
            &r.tool.0,
            &r.conversation.id,
        ])?;
        // 민감 입력은 개인 DM으로만 받는다
        if r.conversation.kind != ConversationKind::Direct
            || r.conversation.thread.as_deref().is_some_and(|t| !id_ok(t))
            || r.scopes.iter().any(|s| !id_ok(s))
        {
            return invalid();
        }
        let scopes: Vec<String> = r.scopes.iter().cloned().collect();
        let mut tx = self.begin_authorized().await?;
        let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        let fingerprint = r.pass_fingerprint.as_ref().unwrap_or(&r.fingerprint);
        let fresh = sqlx::query(
            "insert into bot_requests (profile, binding, principal, request, input_sha256, run)
             values ($1, $2, $3, $4, $5, $6) on conflict do nothing",
        )
        .bind(&k.profile.0)
        .bind(&k.binding.0)
        .bind(&k.principal.0)
        .bind(&k.request.0)
        .bind(fingerprint.as_slice())
        .bind(&run)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !fresh {
            return self.private_replay(tx, &r).await;
        }
        // 새 접수만 활성 판본과 대조한다(같은 잠금 안에서). 어긋나면 접수 행도 남지 않는다
        crate::active::guard_active(&mut tx, &k.profile.0, &r.revision).await?;
        let target = target_json(&bot_core::command::Target::Tool {
            agent: r.agent.clone(),
            tool: r.tool.clone(),
            args: bot_core::command::JsonPayload("{}".into()),
        })?;
        sqlx::query(
            "insert into bot_runs (profile, id, root, agent, principal, scopes, revision, binding, conv_kind, conv_id, thread,
                                   target, input, private, state, generation, claim_epoch, lease_owner, lease_until_ms, private_pass)
             values ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'null', true, 'leased', 1, 1, $12,
                     bot_now_ms() + $13, $14)",
        )
        .bind(&k.profile.0)
        .bind(&run)
        .bind(&r.agent.0)
        .bind(&k.principal.0)
        .bind(&scopes)
        .bind(&r.revision.0)
        .bind(&k.binding.0)
        .bind(kind_str(&r.conversation.kind))
        .bind(&r.conversation.id)
        .bind(&r.conversation.thread)
        .bind(&target)
        .bind(&self.cfg.worker)
        .bind(self.cfg.delivery_lease.as_millis() as i64)
        .bind(r.pass_fingerprint.map(|_| "bound_v2"))
        .execute(&mut *tx)
        .await?;
        let mut entry = Entry::new(&k.principal.0, "private.begin", &run, "started");
        entry.args = Some(*fingerprint);
        audit::append(&mut tx, &k.profile.0, &entry).await?;
        tx.commit().await?;
        Ok(grant(&k.profile, &run, 1, 1, r.pass_fingerprint.is_some()))
    }

    /// 같은 접수 키의 재전달: 지문·에이전트·도구·대화가 하나라도 다르면 충돌, 같으면 이미 정해진 판정만 돌려준다.
    /// 실행기가 반납한 접수(ready)만 다시 점유시키고, 리스가 끝난 접수는 보류한다(다시 실행하지 않는다).
    async fn private_replay(&self, mut tx: Tx<'_>, r: &PrivateRequest) -> R<PrivatePermit> {
        let k = &r.key;
        let row = sqlx::query(
            "select q.input_sha256, u.private_pass, u.id, u.state, u.private, u.agent, u.conv_kind, u.conv_id, u.thread,
                    u.target->>'tool' as tool, u.lease_until_ms
             from bot_requests q join bot_runs u on u.profile = q.profile and u.id = q.run
             where q.profile = $1 and q.binding = $2 and q.principal = $3 and q.request = $4
             for update of u",
        )
        .bind(&k.profile.0)
        .bind(&k.binding.0)
        .bind(&k.principal.0)
        .bind(&k.request.0)
        .fetch_one(&mut *tx)
        .await?;
        // 지문·에이전트·도구·대화(종류·ID·스레드)가 모두 같은 재전달만 받는다. 회신 목적지를 바꾸지 못한다
        let pass: Option<String> = row.get("private_pass");
        let fingerprint = match pass.as_deref() {
            None => r.fingerprint,
            Some("bound_v2" | "passed_v2") => r.pass_fingerprint.ok_or(Error::Conflict)?,
            Some(_) => return invalid(),
        };
        let same = row.get::<Vec<u8>, _>("input_sha256") == fingerprint
            && row.get::<bool, _>("private")
            && text(&row, "agent") == r.agent.0
            && row.get::<Option<String>, _>("tool").as_deref() == Some(r.tool.0.as_str())
            && text(&row, "conv_kind") == kind_str(&r.conversation.kind)
            && text(&row, "conv_id") == r.conversation.id
            && row.get::<Option<String>, _>("thread") == r.conversation.thread;
        if !same {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        }
        // 시각은 행 잠금을 얻은 다음 문장에서 읽는다
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let run: String = row.get("id");
        let live = row
            .get::<Option<i64>, _>("lease_until_ms")
            .is_some_and(|until| until >= now);
        match (text(&row, "state").as_str(), live) {
            ("succeeded", _) if pass.as_deref() == Some("passed_v2") => {
                finish_read(tx, PrivatePermit::Passed).await
            }
            ("succeeded", _) => finish_read(tx, PrivatePermit::Completed).await,
            ("dead" | "cancelled", _) => finish_read(tx, PrivatePermit::Rejected).await,
            ("waiting", _) => finish_read(tx, PrivatePermit::Held).await,
            ("leased", true) => finish_read(tx, PrivatePermit::InProgress).await,
            ("leased", false) => {
                // 리스가 끝났다(시작 여부와 관계없이): 보낸 것이 없다고 단정할 수 없으므로 보류하고 다시 실행하지 않는다
                to_hold(&mut tx, &k.profile.0, &run).await?;
                let entry = Entry::new("system", "private.held", &run, "lease_expired");
                audit::append(&mut tx, &k.profile.0, &entry).await?;
                tx.commit().await?;
                Ok(PrivatePermit::Held)
            }
            ("ready", _) => {
                // 실행기가 반납한 행: 새 claim 토큰(세대 유지)으로 다시 점유한다
                let claimed = sqlx::query(
                    "update bot_runs set state = 'leased', claim_epoch = claim_epoch + 1,
                            lease_owner = $3, lease_until_ms = bot_now_ms() + $4, updated_ms = bot_now_ms()
                     where profile = $1 and id = $2 returning generation, claim_epoch",
                )
                .bind(&k.profile.0)
                .bind(&run)
                .bind(&self.cfg.worker)
                .bind(self.cfg.delivery_lease.as_millis() as i64)
                .fetch_one(&mut *tx)
                .await?;
                let entry = Entry::new(&k.principal.0, "private.begin", &run, "reclaimed");
                audit::append(&mut tx, &k.profile.0, &entry).await?;
                tx.commit().await?;
                Ok(grant(
                    &k.profile,
                    &run,
                    claimed.get("generation"),
                    claimed.get("claim_epoch"),
                    pass.is_some(),
                ))
            }
            _ => finish_read(tx, PrivatePermit::InProgress).await,
        }
    }

    /// 리스가 끝난 민감 DM 접수(시작 여부와 관계없이)를 재전달을 기다리지 않고 내구 보류로 옮긴다.
    /// 보냈는지 모르므로 자동으로 다시 실행하지 않는다. 운영자 보류 목록에서 판정한다. 워커가 집기 전과 보류 조회 전에 부른다.
    pub(crate) async fn sweep_private(&self) -> R<()> {
        let mut tx = self.pool.begin().await?;
        // 다른 청소와 같은 행을 서로 기다리지 않도록 잠긴 행은 건너뛴다
        let mut lost = sqlx::query(
            "with lost as (
                 select profile, id from bot_runs
                 where private and state = 'leased' and lease_until_ms < bot_now_ms()
                 order by profile, id for update skip locked)
             update bot_runs r set state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null,
                    updated_ms = bot_now_ms()
             from lost where r.profile = lost.profile and r.id = lost.id
             returning r.profile, r.id",
        )
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| (row.get::<String, _>("profile"), row.get::<String, _>("id")))
        .collect::<Vec<_>>();
        if lost.is_empty() {
            tx.rollback().await?;
            return Ok(());
        }
        lost.sort();
        for (profile, run) in &lost {
            let entry = Entry::new("system", "private.held", run, "lease_expired");
            audit::append(&mut tx, profile, &entry).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 지정 도구를 실제로 보내기 직전에 한 번만 커밋한다: 활성 판본·리스·claim 토큰·지정 도구·지문을 같은 잠금 안에서 대조한다.
    /// 이 기록이 남으면 결과를 못 받아도 다시 보내지 않는다.
    pub(crate) async fn private_start(&self, s: PrivateStart) -> R<()> {
        ids_ok(&[&s.tool.0, &s.checked_revision.0])?;
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        // 잠금 순서: bot_active → run
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let row = sqlx::query(
            "select r.private, r.state, r.generation, r.claim_epoch, r.private_started_ms, r.lease_until_ms,
                    r.principal, r.target->>'tool' as tool,
                    (select q.input_sha256 from bot_requests q where q.profile = r.profile and q.run = r.id) as fingerprint
             from bot_runs r where r.profile = $1 and r.id = $2 for update of r",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if !row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let live = text(&row, "state") == "leased"
            && owned(&row, at)
            && row
                .get::<Option<i64>, _>("lease_until_ms")
                .is_some_and(|until| until >= now);
        if !live {
            return Err(Error::Stale.into());
        }
        if row.get::<Option<i64>, _>("private_started_ms").is_some() {
            return Err(Error::Conflict.into()); // 한 접수는 한 번만 시작한다
        }
        if row.get::<Option<String>, _>("tool").as_deref() != Some(s.tool.0.as_str())
            || row.get::<Option<Vec<u8>>, _>("fingerprint").as_deref()
                != Some(s.fingerprint.as_slice())
        {
            return Err(Error::Denied.into());
        }
        sqlx::query(
            "update bot_runs set private_started_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .execute(&mut *tx)
        .await?;
        let principal: String = row.get("principal");
        let entry = Entry::new(&principal, "private.start", &at.run.0, "started");
        audit::append(&mut tx, &at.profile.0, &entry).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 결과 기록. 전이표 밖은 `Conflict`다(같은 결과의 반복도 포함). 보류 뒤에는 시작이 확인된 행의 늦은 성공만 받는다:
    /// 보류에서 거절·미전송으로 바꾸거나 다시 점유시키는 길은 운영자 증거 판정(`AdminAct`)뿐이다.
    pub(crate) async fn private_finish(&self, permit: &Attempt, outcome: PrivateOutcome) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "select state, generation, claim_epoch, private, private_started_ms, private_pass, principal, lease_until_ms
             from bot_runs where profile = $1 and id = $2 for update",
        )
        .bind(&permit.profile.0)
        .bind(&permit.run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if !row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        if !owned(&row, permit) {
            return Err(Error::Stale.into());
        }
        let state = text(&row, "state");
        let started = row.get::<Option<i64>, _>("private_started_ms").is_some();
        // 시각은 행 잠금을 얻은 다음 문장에서 읽는다
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let expired = state == "leased"
            && row
                .get::<Option<i64>, _>("lease_until_ms")
                .is_none_or(|until| until < now);
        if expired
            && !matches!(
                (&outcome, started),
                (PrivateOutcome::Completed | PrivateOutcome::Passed, true)
            )
        {
            // 리스가 끝난 접수는 결과가 먼저 오든 재전달이 먼저 오든 내구 보류다: 반납·거절로 되돌리지 않는다.
            // 보류와 감사를 커밋한 뒤에 판정한다(시작이 확인된 늦은 성공만 아래에서 받는다).
            to_hold(&mut tx, &permit.profile.0, &permit.run.0).await?;
            let entry = Entry::new("system", "private.held", &permit.run.0, "lease_expired");
            audit::append(&mut tx, &permit.profile.0, &entry).await?;
            tx.commit().await?;
            return match (&outcome, started) {
                (PrivateOutcome::Held, true) => Ok(()),
                _ => Err(Error::Conflict.into()),
            };
        }
        // (갱신 SQL, 감사 판정): 시작 전에는 반납·거절만, 시작 뒤에는 미전송 반납·완료·보류·거절
        let (sql, verdict) = match (&outcome, state.as_str(), started) {
            (PrivateOutcome::RetryBeforeStart, "leased", false) => (RELEASE, "retry_before_start"),
            (PrivateOutcome::NotSent, "leased", true) => (RELEASE, "not_sent"),
            (PrivateOutcome::Completed, "leased" | "waiting", true) => (FINISH_OK, "completed"),
            (PrivateOutcome::Passed, "leased" | "waiting", true) => {
                if row.get::<Option<String>, _>("private_pass").as_deref() != Some("bound_v2") {
                    return Err(Error::Denied.into());
                }
                (FINISH_OK, "passed")
            }
            (PrivateOutcome::Held, "leased", true) => (HOLD, "held"),
            (PrivateOutcome::Rejected, "leased", _) => (FINISH_REJECTED, "rejected"),
            _ => return Err(Error::Conflict.into()),
        };
        let update = sqlx::query(sql).bind(&permit.profile.0).bind(&permit.run.0);
        let update = match outcome {
            PrivateOutcome::Passed | PrivateOutcome::Completed => {
                update.bind(matches!(outcome, PrivateOutcome::Passed))
            }
            _ => update,
        };
        update.execute(&mut *tx).await?;
        let principal: String = row.get("principal");
        let entry = Entry::new(&principal, "private.finish", &permit.run.0, verdict);
        audit::append(&mut tx, &permit.profile.0, &entry).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// 리스를 반납하고 시작 표시를 지운다(보낸 것이 없음이 확정됐다). 다음 재전달이 새 claim 토큰(세대 유지)으로 다시 점유한다.
const RELEASE: &str = "update bot_runs set state = 'ready', wait_kind = null, lease_until_ms = null, lease_owner = null,
        private_started_ms = null, updated_ms = bot_now_ms() where profile = $1 and id = $2";
const HOLD: &str = "update bot_runs set state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null,
        updated_ms = bot_now_ms() where profile = $1 and id = $2";
const FINISH_OK: &str = "update bot_runs set state = 'succeeded', private_pass = case when $3 then 'passed_v2' else private_pass end,
        error = null, wait_kind = null, lease_until_ms = null, lease_owner = null,
        finished_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2";
const FINISH_REJECTED: &str = "update bot_runs set state = 'dead', error = 'rejected', wait_kind = null, lease_until_ms = null, lease_owner = null,
        finished_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2";

/// 읽기만 한 재전달의 판정을 돌려준다(변경 없음: 열린 트랜잭션을 닫는다).
async fn finish_read(tx: Tx<'_>, permit: PrivatePermit) -> R<PrivatePermit> {
    tx.rollback().await?;
    Ok(permit)
}

use crate::db::text;

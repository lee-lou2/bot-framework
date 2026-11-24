//! 운영자 판정(AdminAct): 보류된 호출·단계·회신 발송·민감 DM 요청을 닫는다. 판정과 감사는 한 커밋이다.
//! - 호출 단위 보류(단일 쓰기): 영수증이면 실행됨, 미실행 증거면 같은 호출 ID로 재개한다. 판정이 서면 run을 깨운다.
//! - 다단계 보류: 어느 단계인지 지정해야 한다(`Invalid`). 요청 단계가 그 호출의 보류 단계일 때만 받고,
//!   미결 보류 단계가 0개가 될 때만 run을 재개한다. 단계가 모두 정해지면 호출은 "미실행(이어서 실행)" 판정으로 닫힌다 —
//!   이미 확정된 단계는 단계 원장이 영수증으로 되돌려주므로 다시 보내지 않는다.
//! - 회신 발송(`reply:<run>:<seq>`)과 민감 DM(`private:<run>`)의 보류도 같은 입구로 판정한다.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store, Tx},
    finish::wake,
};
use bot_core::{
    admin::{HoldDecision, HoldRef, HoldVerdict},
    id::*,
    port::Error,
};
use sha2::{Digest, Sha256};
use sqlx::Row;

fn verdict_ok(verdict: &HoldVerdict) -> R<()> {
    match verdict {
        HoldVerdict::Confirmed { receipt } if id_ok(receipt) && receipt.len() <= 512 => Ok(()),
        HoldVerdict::NotExecuted { evidence }
            if !evidence.trim().is_empty() && evidence.len() <= 1024 =>
        {
            Ok(())
        }
        _ => invalid(),
    }
}

/// 감사에 남길 판정 표지. 민감 DM(`private`) 보류는 영수증·증거 원문을 저장하지 않고 고정 어휘만 남긴다.
fn label(verdict: &HoldVerdict, private: bool) -> String {
    match verdict {
        HoldVerdict::Confirmed { .. } => "confirmed".into(),
        HoldVerdict::NotExecuted { .. } if private => "not_executed".into(),
        HoldVerdict::NotExecuted { evidence } => format!(
            "not_executed:{}",
            evidence.chars().take(200).collect::<String>()
        ),
    }
}

/// 판정 종류와 영수증/증거의 지문. 원문은 저장하지 않는다(민감 DM의 증거도 같다).
fn fingerprint(verdict: &HoldVerdict) -> (&'static str, Vec<u8>) {
    let (kind, text) = match verdict {
        HoldVerdict::Confirmed { receipt } => ("confirmed", receipt),
        HoldVerdict::NotExecuted { evidence } => ("not_executed", evidence),
    };
    (kind, Sha256::digest(text.as_bytes()).to_vec())
}

impl Store {
    /// 발생 ID와 대상을 함께 대조하고 판정·감사를 한 커밋으로 낸다.
    /// - 모르는 ID는 Missing, 다른 run·호출·단계를 가리키면 Conflict다.
    /// - 이미 판정으로 닫힌 발생은 같은 운영자·같은 판정·같은 영수증/증거 지문이면 Repeated(쓰기 없음), 아니면 Conflict다.
    ///   그 뒤 같은 대상이 다시 보류됐어도(새 발생) 옛 ID의 재전송은 새 발생을 건드리지 않는다.
    /// - 잠금 순서: run → 호출·단계 / 회신 행 → 발생 → 감사. 정산·전이 쪽(대상 행 → 발생 트리거)과 같은 순서다.
    pub(crate) async fn resolve_hold_run(
        &self,
        actor: &PrincipalId,
        hold: &HoldRef,
        verdict: &HoldVerdict,
    ) -> R<HoldDecision> {
        ids_ok(&[
            &actor.0,
            &hold.hold_id.0,
            &hold.profile.0,
            &hold.run.0,
            &hold.call.0,
        ])?;
        if let Some(effect) = &hold.effect {
            ids_ok(&[&effect.0])?;
        }
        verdict_ok(verdict)?;
        let (decided, digest) = fingerprint(verdict);
        let mut tx = self.begin_authorized().await?;
        // 발생의 profile·run은 바뀌지 않으므로 먼저 읽어 run을 잠근다
        let owner: (String, String) =
            sqlx::query_as("select profile, run from bot_holds where id = $1")
                .bind(&hold.hold_id.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Missing)?;
        if owner.0 != hold.profile.0 || owner.1 != hold.run.0 {
            return Err(Error::Conflict.into());
        }
        // 호출·회신을 추가하는 쪽과 같은 run 잠금 안에서 판정 대상을 찾는다.
        let (private, waiting_on_hold): (bool, bool) = sqlx::query_as(
            "select private, state = 'waiting' and wait_kind is not distinct from 'hold'
             from bot_runs where profile = $1 and id = $2 for update",
        )
        .bind(&hold.profile.0)
        .bind(&hold.run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let row = sqlx::query(
            "select call, effect, kind, closed_ms, operator, verdict, evidence_sha256 from bot_holds where id = $1",
        )
        .bind(&hold.hold_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("call") != hold.call.0
            || row.get::<Option<String>, _>("effect") != hold.effect.as_ref().map(|e| e.0.clone())
        {
            return Err(Error::Conflict.into());
        }
        if row.get::<Option<i64>, _>("closed_ms").is_some() {
            let same = row.get::<Option<String>, _>("operator").as_deref() == Some(&actor.0)
                && row.get::<Option<String>, _>("verdict").as_deref() == Some(decided)
                && row.get::<Option<Vec<u8>>, _>("evidence_sha256").as_deref()
                    == Some(digest.as_slice());
            return if same {
                Ok(HoldDecision::Repeated)
            } else {
                Err(Error::Conflict.into())
            };
        }
        let seq = lock_target(&mut tx, hold, &row.get::<String, _>("kind"), private).await?;
        let claimed = sqlx::query(
            "update bot_holds set closed_ms = bot_now_ms(), operator = $2, verdict = $3, evidence_sha256 = $4
             where id = $1 and closed_ms is null",
        )
        .bind(&hold.hold_id.0)
        .bind(&actor.0)
        .bind(decided)
        .bind(&digest)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            return Err(Error::Conflict.into());
        }
        let target = match seq {
            Some(seq) => self.resolve_reply(&mut tx, hold, seq, verdict).await?,
            None if private => {
                self.resolve_private(&mut tx, hold, waiting_on_hold, verdict)
                    .await?
            }
            None => {
                self.resolve_call(&mut tx, hold, waiting_on_hold, verdict)
                    .await?
            }
        };
        audit::append(
            &mut tx,
            &hold.profile.0,
            &Entry::new(&actor.0, "hold.resolve", &target, &label(verdict, private)),
        )
        .await?;
        tx.commit().await?;
        Ok(HoldDecision::Applied)
    }

    async fn resolve_call(
        &self,
        tx: &mut Tx<'_>,
        hold: &HoldRef,
        waiting_on_hold: bool,
        verdict: &HoldVerdict,
    ) -> R<String> {
        let (profile, run, call) = (&hold.profile.0, &hold.run.0, &hold.call.0);
        // 잠금 순서: run → 호출 → 단계
        let call_row = sqlx::query("select state, resolution from bot_calls where profile = $1 and run = $2 and call = $3 for update")
            .bind(profile)
            .bind(run)
            .bind(call)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Missing)?;
        if call_row.get::<String, _>("state") != "held" {
            return Err(Error::Conflict.into());
        }
        let steps: i64 = sqlx::query_scalar(
            "select count(*) from bot_effects where profile = $1 and run = $2 and call = $3",
        )
        .bind(profile)
        .bind(run)
        .bind(call)
        .fetch_one(&mut **tx)
        .await?;
        let target = match &hold.effect {
            None if steps > 0 => return invalid(), // 다단계 보류는 어느 단계인지 지정해야 한다
            None => {
                if call_row.get::<Option<String>, _>("resolution").is_some() {
                    return Err(Error::Conflict.into());
                }
                let (resolution, receipt) = match verdict {
                    HoldVerdict::Confirmed { receipt } => ("executed", Some(receipt.as_str())),
                    HoldVerdict::NotExecuted { .. } => ("not_executed", None),
                };
                set_call_resolution(tx, hold, resolution, receipt).await?;
                call.clone()
            }
            Some(effect) => {
                let step = sqlx::query("select state, resolution from bot_effects where profile = $1 and id = $2 and run = $3 and call = $4 for update")
                    .bind(profile)
                    .bind(&effect.0)
                    .bind(run)
                    .bind(call)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::Missing)?;
                if step.get::<String, _>("state") != "held"
                    || step.get::<Option<String>, _>("resolution").is_some()
                {
                    return Err(Error::Conflict.into());
                }
                match verdict {
                    HoldVerdict::Confirmed { receipt } => {
                        sqlx::query("update bot_effects set state = 'confirmed', receipt = $3, reason = null, updated_ms = bot_now_ms() where profile = $1 and id = $2")
                            .bind(profile)
                            .bind(&effect.0)
                            .bind(receipt)
                            .execute(&mut **tx)
                            .await?;
                    }
                    HoldVerdict::NotExecuted { .. } => {
                        sqlx::query("update bot_effects set resolution = 'not_executed', updated_ms = bot_now_ms() where profile = $1 and id = $2")
                            .bind(profile)
                            .bind(&effect.0)
                            .execute(&mut **tx)
                            .await?;
                    }
                }
                let open: i64 = sqlx::query_scalar(
                    "select count(*) from bot_effects where profile = $1 and run = $2 and call = $3 and state = 'held' and resolution is null",
                )
                .bind(profile)
                .bind(run)
                .bind(call)
                .fetch_one(&mut **tx)
                .await?;
                if open > 0 {
                    return Ok(format!("{call}:{}", effect.0)); // 보류 단계가 남았다: run은 계속 기다린다
                }
                set_call_resolution(tx, hold, "not_executed", None).await?;
                format!("{call}:{}", effect.0)
            }
        };
        if waiting_on_hold {
            wake(tx, profile, run, "hold", None).await?;
        }
        Ok(target)
    }

    async fn resolve_reply(
        &self,
        tx: &mut Tx<'_>,
        hold: &HoldRef,
        seq: i32,
        verdict: &HoldVerdict,
    ) -> R<String> {
        let run = &hold.run.0;
        if hold.effect.is_some() {
            return invalid();
        }
        let state: String = sqlx::query_scalar(
            "select state from bot_outbox where profile = $1 and run = $2 and seq = $3 for update",
        )
        .bind(&hold.profile.0)
        .bind(run)
        .bind(seq)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Missing)?;
        if state != "held" {
            return Err(Error::Conflict.into());
        }
        let sql = match verdict {
            HoldVerdict::Confirmed { .. } => {
                "update bot_outbox set state = 'confirmed', receipt = $4, reason = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"
            }
            // 미실행이 입증됐다: 처음부터 다시 보낼 수 있게 되돌린다(발송 세대는 이어서 오른다)
            HoldVerdict::NotExecuted { .. } => {
                "update bot_outbox set state = 'pending', attempts = 0, not_before_ms = 0, lease_until_ms = null, reason = null,
                        updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3 and $4::text is null"
            }
        };
        let receipt = match verdict {
            HoldVerdict::Confirmed { receipt } => Some(receipt.as_str()),
            HoldVerdict::NotExecuted { .. } => None,
        };
        sqlx::query(sql)
            .bind(&hold.profile.0)
            .bind(run)
            .bind(seq)
            .bind(receipt)
            .execute(&mut **tx)
            .await?;
        Ok(hold.call.0.clone())
    }

    async fn resolve_private(
        &self,
        tx: &mut Tx<'_>,
        hold: &HoldRef,
        waiting_on_hold: bool,
        verdict: &HoldVerdict,
    ) -> R<String> {
        if hold.effect.is_some() {
            return invalid();
        }
        if !waiting_on_hold {
            return Err(Error::Conflict.into());
        }
        // 본문이 없어 다시 실행할 수 없다: 실행됨이면 완료, 미실행이면 거절로 끝내고 사용자가 새로 요청한다
        let (state, error) = match verdict {
            HoldVerdict::Confirmed { .. } => ("succeeded", None),
            HoldVerdict::NotExecuted { .. } => ("dead", Some("not_executed")),
        };
        sqlx::query(
            "update bot_runs set state = $3, error = $4, wait_kind = null, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&hold.profile.0)
        .bind(&hold.run.0)
        .bind(state)
        .bind(error)
        .execute(&mut **tx)
        .await?;
        Ok(hold.call.0.clone())
    }
}

async fn set_call_resolution(
    tx: &mut Tx<'_>,
    hold: &HoldRef,
    resolution: &str,
    receipt: Option<&str>,
) -> R<()> {
    sqlx::query(
        "update bot_calls set resolution = $4, receipt = $5, updated_ms = bot_now_ms() where profile = $1 and run = $2 and call = $3",
    )
    .bind(&hold.profile.0)
    .bind(&hold.run.0)
    .bind(&hold.call.0)
    .bind(resolution)
    .bind(receipt)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 발생이 가리키는 대상 행을 잠그고(호출 → 단계, 또는 회신 행) 회신이면 순번을 돌려준다. 발생의 종류와 run의 성격이 어긋나면 Conflict다.
async fn lock_target(
    tx: &mut Tx<'_>,
    hold: &HoldRef,
    kind: &str,
    private_run: bool,
) -> R<Option<i32>> {
    let (profile, run, call) = (&hold.profile.0, &hold.run.0, &hold.call.0);
    match kind {
        "tool" if !private_run => {
            sqlx::query(
                "select 1 from bot_calls where profile = $1 and run = $2 and call = $3 for update",
            )
            .bind(profile)
            .bind(run)
            .bind(call)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Missing)?;
            if let Some(effect) = &hold.effect {
                sqlx::query("select 1 from bot_effects where profile = $1 and id = $2 and run = $3 and call = $4 for update")
                    .bind(profile)
                    .bind(&effect.0)
                    .bind(run)
                    .bind(call)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::Missing)?;
            }
            Ok(None)
        }
        "reply" if !private_run && hold.effect.is_none() => {
            let seqs: Vec<i32> = sqlx::query_scalar(
                "select seq from bot_outbox where profile = $1 and run = $2 and call = $3 for update",
            )
            .bind(profile)
            .bind(run)
            .bind(call)
            .fetch_all(&mut **tx)
            .await?;
            match seqs.as_slice() {
                [] => Err(Error::Missing.into()),
                [seq] => Ok(Some(*seq)),
                _ => Err(Error::Conflict.into()),
            }
        }
        "private" if private_run && hold.effect.is_none() && *call == format!("private:{run}") => {
            Ok(None)
        }
        _ => Err(Error::Conflict.into()),
    }
}

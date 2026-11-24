//! 단계 원장(EffectJournal): 여러 번 쓰는 도구의 단계별 intent → confirmed | rejected | held.
//! 최초 요청에서 기록을 커밋한 뒤에만 `send=true`를 준다. 같은 키·다른 인자는 `Conflict`, 기존 confirmed는 영수증,
//! 전송 여부가 불명인 started는 held를 돌려주며 둘 다 `send=false`다. 같은 영수증을 다시 보고해도 결과는 같다.
//! 호출이 보류되면 운영자 판정(미실행 입증)이 있을 때만 같은 단계를 다시 보낼 수 있다.

use crate::{
    audit::{self, Entry},
    calls::{fence, fence_settle, lock_call, record_of},
    codec::*,
    db::{R, Store},
};
use bot_core::{
    command::{EffectRequest, EffectStatus, JsonPayload},
    id::*,
    ledger::CallPhase,
    port::{EffectPermit, Error},
    state::{EffectResolution, EffectState},
};
use sqlx::{Row, postgres::PgRow};

pub(crate) fn state_str(state: EffectState) -> &'static str {
    match state {
        EffectState::Started => "started",
        EffectState::Confirmed => "confirmed",
        EffectState::Rejected => "rejected",
        EffectState::Held => "held",
    }
}

pub(crate) fn state_of(s: &str) -> R<EffectState> {
    Ok(match s {
        "started" => EffectState::Started,
        "confirmed" => EffectState::Confirmed,
        "rejected" => EffectState::Rejected,
        "held" => EffectState::Held,
        _ => return invalid(),
    })
}

/// 단계 인자의 지문. 요청과 조회가 같은 정규 JSON에서 만든다(키 순서·공백이 달라도 같다).
pub(crate) fn args_digest(args: &JsonPayload) -> R<[u8; 32]> {
    Ok(sha256_text(&canonical(args)?.1))
}

pub(crate) fn status_of(row: &PgRow) -> R<EffectStatus> {
    let state = state_of(&row.get::<String, _>("state"))?;
    let resolution = (state == EffectState::Held
        && row.get::<Option<String>, _>("resolution").as_deref() == Some("not_executed"))
    .then_some(EffectResolution::NotExecuted);
    Ok(EffectStatus {
        effect: EffectId(row.get("id")),
        state,
        resolution,
        receipt: row.get("receipt"),
    })
}

async fn lock_effect(
    tx: &mut crate::db::Tx<'_>,
    at: &Attempt,
    call: &CallId,
    step: &str,
) -> R<Option<PgRow>> {
    Ok(sqlx::query(
        &format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 and step = $4 for update", crate::cols::EFFECTS),
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&call.0)
    .bind(step)
    .fetch_optional(&mut **tx)
    .await?)
}

impl Store {
    pub(crate) async fn effect_request(&self, r: EffectRequest) -> R<EffectPermit> {
        ids_ok(&[&r.call.0, &r.step])?;
        let digest = args_digest(&r.args)?;
        let at = &r.attempt;
        let mut tx = self.pool.begin().await?;
        let run = fence(&mut tx, at).await?;
        // 단계는 지금 실행 중인(이 세대가 시작한) 호출 안에서만 만들 수 있다
        let call = lock_call(&mut tx, at, &r.call).await?;
        match record_of(&call)?.phase {
            CallPhase::Started { generation, .. } if generation == at.generation => {}
            _ => return Err(Error::Conflict.into()),
        }
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let permit = match lock_effect(&mut tx, at, &r.call, &r.step).await? {
            None => {
                let id: String = sqlx::query_scalar(
                    "insert into bot_effects (profile, id, run, call, step, args_sha256, generation)
                     values ($1, 'e-' || gen_random_uuid()::text, $2, $3, $4, $5, $6) returning id",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(&r.call.0)
                .bind(&r.step)
                .bind(digest.as_slice())
                .bind(at.generation.0 as i64)
                .fetch_one(&mut *tx)
                .await?;
                let mut entry = Entry::new(&principal, "effect.start", &id, &r.step);
                entry.args = Some(digest);
                audit::append(&mut tx, &run.profile, &entry).await?;
                EffectPermit {
                    status: EffectStatus {
                        effect: EffectId(id),
                        state: EffectState::Started,
                        resolution: None,
                        receipt: None,
                    },
                    send: true,
                }
            }
            Some(row) => {
                if row.get::<Vec<u8>, _>("args_sha256") != digest {
                    return Err(Error::Conflict.into());
                }
                let id: String = row.get("id");
                let (state, held_before) = (
                    state_of(&row.get::<String, _>("state"))?,
                    row.get::<String, _>("state") == "held",
                );
                match state {
                    EffectState::Started => {
                        // 이전 시도가 보냈는지 알 수 없다: 보류로 바꾸고 다시 보내지 않는다(같은 세대의 중복 요청도 보내지 않는다)
                        if row.get::<i64, _>("generation") < at.generation.0 as i64 {
                            sqlx::query("update bot_effects set state = 'held', reason = 'owner_lost', updated_ms = bot_now_ms() where profile = $1 and id = $2")
                                .bind(&run.profile)
                                .bind(&id)
                                .execute(&mut *tx)
                                .await?;
                            audit::append(
                                &mut tx,
                                &run.profile,
                                &Entry::new("system", "effect.held", &id, "owner_lost"),
                            )
                            .await?;
                        }
                        EffectPermit {
                            status: EffectStatus {
                                effect: EffectId(id),
                                state: EffectState::Held,
                                resolution: None,
                                receipt: None,
                            },
                            send: false,
                        }
                    }
                    EffectState::Held
                        if held_before
                            && row.get::<Option<String>, _>("resolution").as_deref()
                                == Some("not_executed") =>
                    {
                        // 운영자가 미실행을 입증했다: 같은 호출·같은 단계를 현재 세대로 다시 보낸다
                        sqlx::query(
                            "update bot_effects set state = 'started', generation = $3, reason = null, resolution = null, updated_ms = bot_now_ms()
                             where profile = $1 and id = $2",
                        )
                        .bind(&run.profile)
                        .bind(&id)
                        .bind(at.generation.0 as i64)
                        .execute(&mut *tx)
                        .await?;
                        audit::append(
                            &mut tx,
                            &run.profile,
                            &Entry::new(&principal, "effect.restart", &id, &r.step),
                        )
                        .await?;
                        EffectPermit {
                            status: EffectStatus {
                                effect: EffectId(id),
                                state: EffectState::Started,
                                resolution: None,
                                receipt: None,
                            },
                            send: true,
                        }
                    }
                    _ => EffectPermit {
                        status: status_of(&row)?,
                        send: false,
                    },
                }
            }
        };
        tx.commit().await?;
        Ok(permit)
    }

    pub(crate) async fn effect_status(
        &self,
        at: &Attempt,
        call: &CallId,
        effect: &EffectId,
    ) -> R<EffectStatus> {
        let mut tx = self.pool.begin().await?;
        fence_settle(&mut tx, at).await?;
        let row = sqlx::query(&format!(
            "select {} from bot_effects where profile = $1 and id = $2 and run = $3 and call = $4",
            crate::cols::EFFECTS
        ))
        .bind(&at.profile.0)
        .bind(&effect.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        tx.rollback().await?;
        status_of(&row)
    }

    /// 아직 확정되지 않은(started·held) 단계들. 하나라도 있으면 도구의 성공 응답만으로 호출을 끝내지 않는다.
    pub(crate) async fn effect_unresolved(
        &self,
        at: &Attempt,
        call: &CallId,
    ) -> R<Vec<EffectStatus>> {
        let mut tx = self.pool.begin().await?;
        fence_settle(&mut tx, at).await?;
        let rows = sqlx::query(
            &format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 and state in ('started', 'held')
               and not (state = 'held' and resolution is not distinct from 'not_executed') order by created_ms, id", crate::cols::EFFECTS),
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .fetch_all(&mut *tx)
        .await?;
        tx.rollback().await?;
        rows.iter().map(status_of).collect()
    }

    /// 단계를 마무리한다. confirmed에는 영수증, rejected는 확정 거절, held에는 불명 사유 코드(`receipt` 칸)를 적는다.
    pub(crate) async fn effect_record(
        &self,
        at: &Attempt,
        call: &CallId,
        status: EffectStatus,
    ) -> R<()> {
        let text = status.receipt.as_deref();
        match (status.state, text) {
            (EffectState::Confirmed, Some(receipt)) if id_ok(receipt) && receipt.len() <= 512 => {}
            (EffectState::Held, Some(code)) if id_ok(code) && code.len() <= 64 => {}
            (EffectState::Rejected, None) => {}
            _ => return invalid(),
        }
        let mut tx = self.pool.begin().await?;
        let run = fence_settle(&mut tx, at).await?;
        let row = sqlx::query(&format!(
            "select {} from bot_effects where profile = $1 and id = $2 for update",
            crate::cols::EFFECTS
        ))
        .bind(&at.profile.0)
        .bind(&status.effect.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("run") != at.run.0 || row.get::<String, _>("call") != call.0 {
            return Err(Error::Denied.into()); // 다른 호출의 단계다
        }
        let current = state_of(&row.get::<String, _>("state"))?;
        if current == status.state
            && row.get::<Option<String>, _>("receipt").as_deref()
                == text.filter(|_| status.state == EffectState::Confirmed)
        {
            tx.rollback().await?;
            return Ok(()); // 같은 보고의 반복
        }
        match (current, status.state) {
            (
                EffectState::Started | EffectState::Held,
                EffectState::Confirmed | EffectState::Rejected,
            ) => {}
            (EffectState::Started, EffectState::Held) => {}
            _ => return Err(Error::Conflict.into()),
        }
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let (receipt, reason) = match status.state {
            EffectState::Confirmed => (text, None),
            EffectState::Held => (None, text),
            _ => (None, None),
        };
        sqlx::query(
            "update bot_effects set state = $3, receipt = $4, reason = $5, resolution = null, updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&status.effect.0)
        .bind(state_str(status.state))
        .bind(receipt)
        .bind(reason)
        .execute(&mut *tx)
        .await?;
        let verdict = reason.map_or_else(
            || state_str(status.state).to_owned(),
            |code| format!("held:{code}"),
        );
        let mut entry = Entry::new(&principal, "effect.record", &status.effect.0, &verdict);
        entry.args = Some(to32(row.get("args_sha256"))?);
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(())
    }
}

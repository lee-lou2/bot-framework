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

/// 사전 승인 호출의 새 쓰기 허가를 확인한다. 거절 사유(고정 낱말)가 있으면 돌려준다. 승인 없는 호출은 늘 통과한다.
async fn write_refused(
    tx: &mut crate::db::Tx<'_>,
    granted: &Option<(String, i64)>,
    args: &JsonPayload,
) -> R<Option<crate::authority::Refused>> {
    let Some((grant, epoch)) = granted else {
        return Ok(None);
    };
    Ok(crate::authority::permit_write(tx, grant, *epoch, args)
        .await?
        .err())
}

/// 감사 판정: 사전 승인 호출이면 승인 ID를 덧붙인다(영수증과 같은 줄에서 짝을 지어 볼 수 있다).
fn granted_verdict(step: &str, granted: &Option<(String, i64)>) -> String {
    granted
        .as_ref()
        .map_or_else(|| step.to_owned(), |(id, _)| format!("{step};grant={id}"))
}

/// 새 쓰기 허가를 거절한다: 감사 한 줄만 커밋하고(사용량·효과 행 0) 호출자에게는 wire 분류 오류다
/// (대상 밖·한도·정지 = denied, 철회·교체·옛 세대 = stale, 형식 오류 = invalid).
async fn refuse(
    mut tx: crate::db::Tx<'_>,
    profile: &str,
    principal: &str,
    step: &str,
    digest: [u8; 32],
    why: crate::authority::Refused,
) -> R<EffectPermit> {
    let mut entry = Entry::new(principal, "effect.refuse", step, why.reason());
    entry.args = Some(digest);
    audit::append(&mut tx, profile, &entry).await?;
    tx.commit().await?;
    Err(why.error().into())
}

impl Store {
    pub(crate) async fn effect_request(&self, r: EffectRequest) -> R<EffectPermit> {
        ids_ok(&[&r.call.0, &r.step])?;
        let digest = args_digest(&r.args)?;
        let at = &r.attempt;
        let mut tx = self.pool.begin().await?;
        // 사전 승인(F1) 호출이면 활성 집합을 먼저 공유 잠금한다(잠금 순서: 활성 집합 → run → 호출 → 효과 → 승인 → 정지 → 사용 → 감사).
        // 승인 ID는 호출을 시작할 때 한 번 기록되고 바뀌지 않는다.
        let granted: Option<(String, i64)> = sqlx::query(
            "select grant_id, grant_epoch from bot_calls where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&r.call.0)
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|row| row.get::<Option<String>, _>("grant_id").zip(row.get::<Option<i64>, _>("grant_epoch")));
        if granted.is_some() {
            sqlx::query("select 1 from bot_active where singleton for share")
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Stale)?;
        }
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
                // 새 외부 쓰기 허가: 사전 승인 호출이면 승인·정지·허용 쌍·하루 한도를 같은 커밋에서 확인하고 사용량을 올린다
                if let Some(refused) = write_refused(&mut tx, &granted, &r.args).await? {
                    return refuse(tx, &run.profile, &principal, &r.step, digest, refused).await;
                }
                let id: String = sqlx::query_scalar(
                    "insert into bot_effects (profile, id, run, call, step, args_sha256, generation, grant_id)
                     values ($1, 'e-' || gen_random_uuid()::text, $2, $3, $4, $5, $6, $7) returning id",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(&r.call.0)
                .bind(&r.step)
                .bind(digest.as_slice())
                .bind(at.generation.0 as i64)
                .bind(granted.as_ref().map(|(id, _)| id.as_str()))
                .fetch_one(&mut *tx)
                .await?;
                let verdict = granted_verdict(&r.step, &granted);
                let mut entry = Entry::new(&principal, "effect.start", &id, &verdict);
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
                    // 같은 유효한 승인 안에서만 충돌이다: 철회·교체·옛 세대의 승인은 먼저 낡았다고 알린다
                    if let Some((grant, epoch)) = &granted
                        && crate::authority::stale(&mut tx, grant, *epoch).await?
                    {
                        let stale = crate::authority::Refused::Stale;
                        return refuse(tx, &run.profile, &principal, &r.step, digest, stale).await;
                    }
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
                        // 운영자가 미실행을 입증했다: 같은 호출·같은 단계를 현재 세대로 다시 보낸다(사전 승인 호출이면 새 허가로 센다)
                        if let Some(refused) = write_refused(&mut tx, &granted, &r.args).await? {
                            return refuse(tx, &run.profile, &principal, &r.step, digest, refused)
                                .await;
                        }
                        sqlx::query(
                            "update bot_effects set state = 'started', generation = $3, reason = null, resolution = null, updated_ms = bot_now_ms(),
                                    grant_id = coalesce($4, grant_id)
                             where profile = $1 and id = $2",
                        )
                        .bind(&run.profile)
                        .bind(&id)
                        .bind(at.generation.0 as i64)
                        .bind(granted.as_ref().map(|(id, _)| id.as_str()))
                        .execute(&mut *tx)
                        .await?;
                        let verdict = granted_verdict(&r.step, &granted);
                        audit::append(
                            &mut tx,
                            &run.profile,
                            &Entry::new(&principal, "effect.restart", &id, &verdict),
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

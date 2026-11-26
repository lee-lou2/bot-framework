//! 넘김(`bot/pass`): 넘김 사슬을 접수와 함께 남기고, PASS 때 호출 결론·커서 전진·감사를 **한 커밋**으로 기록한다.
//! 사슬은 루트 run 한 행의 `pass_chain`(후보 목록과 기본 대화)과 `pass_pos`(지금 차례)다. 커서는 앞으로만 가고, 옛 시도의 쓰기는 리스 펜싱(`fence`)이 막는다.
//! 후보마다 호출 ID가 달라(`direct_call_id`) 한 후보는 한 번만 방문한다. 쓰기 단계·보류·승인·회신이 이미 있으면 넘기지 않는다(`pass_after_effect`).

use crate::{
    audit::{self, Entry},
    calls::{fence, lock_call, phase_of},
    codec::*,
    db::{R, Store, Tx},
    finish::{End, end_run},
};
use bot_core::{
    command::{Accept, Target},
    id::{AgentId, RunId},
    ledger::{CallPhase, FinishCall, Outcome},
    pass::{Candidate, PASS_AFTER_EFFECT, PassChain, PassNext, direct_call_id},
    port::Error,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeSet;

/// 한 접수가 가질 수 있는 후보 수의 상한(이보다 많이 맞아도 뒤는 도달하지 못한다)
pub const MAX_CANDIDATES: usize = 32;

/// 사슬을 검증해 JSON 문서로 만든다. 첫 후보는 접수의 대상과 같아야 하고, 후보는 모두 직접 도구 호출, 기본 대화는 에이전트 대상이어야 한다.
fn chain_doc(a: &Accept, chain: &PassChain) -> R<Value> {
    let Some(first) = chain.candidates.first() else {
        return invalid();
    };
    if chain.candidates.len() > MAX_CANDIDATES || first.target != a.target {
        return invalid();
    }
    let mut candidates = Vec::new();
    for c in &chain.candidates {
        if !id_ok(&c.trigger) || !matches!(c.target, Target::Tool { .. }) {
            return invalid();
        }
        ids_ok(&[&agent_of(&c.target).0])?;
        candidates.push(json!({"t": c.trigger, "target": target_json(&c.target)?}));
    }
    let default = match &chain.default {
        Some(target @ Target::Agent { .. }) => {
            ids_ok(&[&agent_of(target).0])?;
            Some(target_json(target)?)
        }
        Some(_) => return invalid(),
        None => None,
    };
    Ok(json!({"c": candidates, "d": default}))
}

/// 접수한 run에 사슬과 커서(0)를 남긴다. 접수 트랜잭션 안에서 부른다.
pub(crate) async fn store_chain(
    tx: &mut Tx<'_>,
    a: &Accept,
    run: &str,
    chain: &PassChain,
) -> R<()> {
    let doc = chain_doc(a, chain)?;
    sqlx::query("update bot_runs set pass_chain = $3, pass_pos = 0 where profile = $1 and id = $2")
        .bind(&a.event.profile.0)
        .bind(run)
        .bind(&doc)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 사슬 문서를 후보 목록과 기본 대화로 읽는다.
fn read_chain(doc: &Value) -> R<PassChain> {
    let mut candidates = Vec::new();
    for c in doc
        .get("c")
        .and_then(Value::as_array)
        .ok_or(Error::Invalid)?
    {
        let trigger = c.get("t").and_then(Value::as_str).ok_or(Error::Invalid)?;
        candidates.push(Candidate {
            trigger: trigger.to_owned(),
            target: target_from(c.get("target").ok_or(Error::Invalid)?)?,
        });
    }
    let default = match doc.get("d") {
        None | Some(Value::Null) => None,
        Some(target) => Some(target_from(target)?),
    };
    Ok(PassChain {
        candidates,
        default,
    })
}

/// 이 호출 때문에 생긴 쓰기 흔적이 있는가: 효과 단계(어떤 상태든)·회신(outbox)·승인·예약된 전송 초안. 있으면 판별용 읽기만 한 것이 아니다.
async fn effect_traces(tx: &mut Tx<'_>, profile: &str, run: &str, call: &str) -> R<bool> {
    let found: bool = sqlx::query_scalar(
        "select exists (select 1 from bot_effects where profile = $1 and run = $2 and call = $3)
             or exists (select 1 from bot_outbox where profile = $1 and run = $2)
             or exists (select 1 from bot_approvals where profile = $1 and run = $2)
             or exists (select 1 from bot_deliveries where profile = $1 and run = $2)
             or exists (select 1 from bot_calls where profile = $1 and run = $2 and call = $3 and approval is not null)",
    )
    .bind(profile)
    .bind(run)
    .bind(call)
    .fetch_one(&mut **tx)
    .await?;
    Ok(found)
}

impl Store {
    /// `call_finish`의 «처리하지 않음» 판: 호출 결론(`Done`)·커서 전진·감사를 한 커밋으로 기록하고 run을 다음 후보(또는 기본 대화)로 되돌린다.
    /// 마지막 후보가 넘겼고 기본 대화가 없으면 run을 조용히 끝낸다. 호출이 쓰기 흔적을 남겼으면 아무것도 쓰지 않고 `Refused`다.
    /// 커서가 `pos`가 아니면(이미 넘겼다) `Stale`이다. 옛 시도는 리스 펜싱이 막는다.
    pub(crate) async fn call_finish_pass(
        &self,
        f: FinishCall,
        pos: u32,
        runnable: BTreeSet<AgentId>,
    ) -> R<PassNext> {
        let Outcome::Done(payload) = &f.outcome else {
            return invalid();
        };
        let result = canonical(payload)?.1;
        let at = &f.attempt;
        let mut tx = self.pool.begin().await?;
        let run = fence(&mut tx, at).await?;
        if run.parent.is_some() {
            return invalid();
        }
        let row = lock_call(&mut tx, at, &f.call).await?;
        match phase_of(&row)? {
            CallPhase::Started { generation, .. } if generation.0 <= at.generation.0 => {}
            _ => return Err(Error::Conflict.into()),
        }
        let cursor = sqlx::query(
            "select pass_chain, pass_pos, principal, scopes from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        let (Some(doc), Some(current)) = (
            cursor.get::<Option<Value>, _>("pass_chain"),
            cursor.get::<Option<i32>, _>("pass_pos"),
        ) else {
            return Err(Error::Stale.into());
        };
        // 이미 앞으로 간 커서·다른 후보의 호출은 이 호출의 넘김이 아니다
        if u32::try_from(current) != Ok(pos)
            || f.call != direct_call_id(&RunId(run.id.clone()), pos)
        {
            return Err(Error::Stale.into());
        }
        let PassChain {
            candidates,
            default,
        } = read_chain(&doc)?;
        let index = usize::try_from(pos).map_err(|_| Error::Invalid)?;
        let Some(Candidate { trigger, .. }) = candidates.get(index) else {
            return Err(Error::Stale.into());
        };
        if effect_traces(&mut tx, &run.profile, &run.id, &f.call.0).await? {
            tx.rollback().await?;
            return Ok(PassNext::Refused(PASS_AFTER_EFFECT));
        }
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, reason = null, updated_ms = bot_now_ms(), finished_ms = bot_now_ms()
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .bind(&f.call.0)
        .bind(&result)
        .execute(&mut *tx)
        .await?;
        let next_pos = pos + 1;
        let next_index = usize::try_from(next_pos).map_err(|_| Error::Invalid)?;
        // 기본 대화(에이전트)는 에이전트 실행에 요구하는 `run` 스코프가 있는 요청자만 시작한다. 접수는 첫 후보(도구)만 보고 이 스코프를 요구하지 않았으므로 넘기는 자리에서
        // 한 번 더 본다: 접수가 고정한 스코프에 `run`이 있어야 하고, **지금** 두 판본에서 요청자가 그 에이전트를 `run`할 수 있어야 한다(`runnable`: 브로커가 현재 판본으로 계산한다).
        // 아니면 기본 대화를 부르지 않고 run을 조용히 끝낸다(도구만 허용된 멤버, 접수 뒤에 `run`이 철회된 멤버가 에이전트를 실행하지 못한다).
        let granted = cursor
            .get::<Vec<String>, _>("scopes")
            .iter()
            .any(|scope| scope == bot_core::scope::RUN_SCOPE);
        let may_run = |target: &Target| granted && runnable.contains(agent_of(target));
        let (next, label) = match (candidates.get(next_index), &default) {
            (Some(next), _) => (
                PassNext::Candidate(next_pos),
                (
                    Some(next.target.clone()),
                    format!("candidate:{}@{next_pos}", next.trigger),
                ),
            ),
            (None, Some(target)) if may_run(target) => (
                PassNext::Default,
                (Some(target.clone()), format!("default@{next_pos}")),
            ),
            (None, Some(_)) => (PassNext::End, (None, format!("default_denied@{next_pos}"))),
            (None, None) => (PassNext::End, (None, format!("end@{next_pos}"))),
        };
        let (target, label) = label;
        match &target {
            Some(target) => {
                // 다음 후보(또는 기본 대화)로: 대상·에이전트·커서를 바꾸고 대기열로 되돌린다. 다음 점유가 세대와 claim 토큰을 올려 이 시도의 쓰기를 막는다.
                sqlx::query(
                    "update bot_runs set target = $3, agent = $4, pass_pos = $5, state = 'ready', lease_until_ms = null, lease_owner = null,
                            updated_ms = bot_now_ms()
                     where profile = $1 and id = $2",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(target_json(target)?)
                .bind(&agent_of(target).0)
                .bind(i32::try_from(next_pos).map_err(|_| Error::Invalid)?)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "update bot_runs set pass_pos = $3, updated_ms = bot_now_ms() where profile = $1 and id = $2",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(i32::try_from(next_pos).map_err(|_| Error::Invalid)?)
                .execute(&mut *tx)
                .await?;
                // 보낼 답이 없다: 일반 완료와 달리 outbox를 쌓지 않는다
                end_run(&mut tx, &run, End::Succeeded(&result), "system").await?;
            }
        }
        // 감사는 마지막에(감사 head 잠금을 짧게): 넘긴 후보와 다음 자리만 남긴다
        let principal: String = cursor.get("principal");
        let verdict = format!("{trigger}@{pos}");
        let mut entry = Entry::new(&principal, "call.pass", &f.call.0, &verdict);
        entry.args = Some(to32(row.get("args_sha256"))?);
        audit::append(&mut tx, &run.profile, &entry).await?;
        audit::append(
            &mut tx,
            &run.profile,
            &Entry::new("system", "trigger.next", &run.id, &label),
        )
        .await?;
        tx.commit().await?;
        Ok(next)
    }
}

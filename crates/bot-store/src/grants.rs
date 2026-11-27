//! 사전 승인의 저장: 활성화 동기화·접수 결박·관리 조회와 정지·재개.
//! 잠금 순서(전역 규칙의 연장): 활성 집합 → (run·호출·효과) → 승인 행 → 정지 행 → 사용 행 → 감사 head.
//! 철회된 승인 행은 남겨 같은 ID가 되살아나지 않는다. 바뀐 선언은 항상 새 ID를 받는다.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{R, Store, Tx},
};
use bot_core::{
    admin::Viewer,
    grant::{
        GrantCas, GrantId, GrantSpec, MAX_PAIRS, MAX_PER_DAY, PauseDecision, PauseOutcome,
        PauseState, PreapprovalRow, TriggerKey, decide_pause, piece_ok,
    },
    id::*,
    port::Error,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeMap;

/// 한 트리거를 가리키는 키.
type Key = (String, String, String);

/// 활성화가 같은 커밋에 남길 감사 한 줄(상태 변경 뒤 마지막에 쓴다).
pub(crate) struct Note {
    profile: String,
    action: &'static str,
    target: String,
    verdict: String,
    args: Option<[u8; 32]>,
}

impl Note {
    pub(crate) fn into_audit(self) -> OwnedEntry {
        let mut entry = OwnedEntry::new(
            self.profile,
            "system",
            self.action,
            self.target,
            self.verdict,
        );
        entry.args = self.args;
        entry
    }
}

fn spec_ok(spec: &GrantSpec) -> R<()> {
    ids_ok(&[&spec.profile.0, &spec.agent.0, &spec.trigger, &spec.tool.0])?;
    let sorted = spec.pairs.windows(2).all(|w| w[0] < w[1]);
    if spec.pairs.is_empty()
        || spec.pairs.len() > MAX_PAIRS
        || !sorted
        || spec
            .pairs
            .iter()
            .any(|(operation, target)| !piece_ok(operation) || !piece_ok(target))
        || !(1..=MAX_PER_DAY).contains(&spec.max_per_day)
        || spec.summary.is_empty()
        || spec.summary.len() > 8 * 1024
    {
        return invalid();
    }
    Ok(())
}

fn key_of(spec: &GrantSpec) -> Key {
    (
        spec.profile.0.clone(),
        spec.agent.0.clone(),
        spec.trigger.clone(),
    )
}

/// 활성 집합 교체와 같은 커밋에서 사전 승인을 맞춘다(호출자가 `bot_active`를 `FOR UPDATE`로 쥔 상태).
/// 지문이 같은 활성 승인은 ID를 그대로 두고, 지문이 달라졌거나 처음이면 새 ID를 발급하며(옛 승인은 철회),
/// 새 집합에 선언이 없는 활성 승인은 철회한다. 돌려주는 것은 이번에 발급한 승인과 감사 줄이다.
pub(crate) async fn sync_in(
    tx: &mut Tx<'_>,
    specs: &[GrantSpec],
    live: &BTreeMap<ProfileId, Revision>,
) -> R<(Vec<(GrantId, GrantSpec)>, Vec<Note>)> {
    let mut wanted: BTreeMap<Key, &GrantSpec> = BTreeMap::new();
    for spec in specs {
        spec_ok(spec)?;
        if !live.contains_key(&spec.profile) || wanted.insert(key_of(spec), spec).is_some() {
            return invalid();
        }
    }
    let rows = sqlx::query(
        "select id, profile, agent, trigger, fingerprint from bot_grants where state = 'active' order by id for update",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut existing: BTreeMap<Key, (String, Vec<u8>)> = rows
        .iter()
        .map(|r| {
            (
                (r.get("profile"), r.get("agent"), r.get("trigger")),
                (r.get("id"), r.get("fingerprint")),
            )
        })
        .collect();
    let (mut issued, mut notes) = (Vec::new(), Vec::new());
    for (key, spec) in &wanted {
        match existing.remove(key) {
            Some((_, fingerprint)) if fingerprint == spec.fingerprint => continue,
            Some((id, _)) => {
                revoke(tx, &id, "changed").await?;
                notes.push(revoked(key, &id, "changed"));
            }
            None => {}
        }
        let id = issue(tx, spec).await?;
        notes.push(Note {
            profile: spec.profile.0.clone(),
            action: "preapproval.issue",
            target: format!("{}/{}", spec.agent.0, spec.trigger),
            verdict: id.clone(),
            args: Some(spec.fingerprint),
        });
        issued.push((GrantId(id), (*spec).clone()));
    }
    for (key, (id, _)) in existing {
        revoke(tx, &id, "removed").await?;
        notes.push(revoked(&key, &id, "removed"));
    }
    Ok((issued, notes))
}

fn revoked(key: &Key, id: &str, why: &str) -> Note {
    Note {
        profile: key.0.clone(),
        action: "preapproval.revoke",
        target: format!("{}/{}", key.1, key.2),
        verdict: format!("{id}:{why}"),
        args: None,
    }
}

async fn revoke(tx: &mut Tx<'_>, id: &str, why: &str) -> R<()> {
    sqlx::query(
        "update bot_grants set state = 'revoked', revoked_ms = bot_now_ms(), revoked_reason = $2 where id = $1",
    )
    .bind(id)
    .bind(why)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn issue(tx: &mut Tx<'_>, spec: &GrantSpec) -> R<String> {
    let seq: i64 = sqlx::query_scalar(
        "select coalesce(max(seq), 0) + 1 from bot_grants where profile = $1 and agent = $2 and trigger = $3",
    )
    .bind(&spec.profile.0)
    .bind(&spec.agent.0)
    .bind(&spec.trigger)
    .fetch_one(&mut **tx)
    .await?;
    let pairs: Vec<Value> = spec.pairs.iter().map(|(o, t)| json!([o, t])).collect();
    let summary: Value = serde_json::from_str(&spec.summary).map_err(|_| Error::Invalid)?;
    let id: String = sqlx::query_scalar(
        "insert into bot_grants (id, profile, agent, trigger, seq, fingerprint, spec, tool, tool_revision,
                                 args_sha256, pairs, max_per_day)
         values ('g-' || gen_random_uuid()::text, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning id",
    )
    .bind(&spec.profile.0)
    .bind(&spec.agent.0)
    .bind(&spec.trigger)
    .bind(seq)
    .bind(spec.fingerprint.as_slice())
    .bind(&summary)
    .bind(&spec.tool.0)
    .bind(summary["tool_revision"].as_str().unwrap_or_default())
    .bind(hex_bytes(summary["args_sha256"].as_str().unwrap_or_default())?.as_slice())
    .bind(Value::Array(pairs))
    .bind(spec.max_per_day as i32)
    .fetch_one(&mut **tx)
    .await?;
    // 정지 상태는 트리거에 속한다: 승인이 교체돼도 이어진다(처음이면 풀린 상태로 만든다)
    sqlx::query(
        "insert into bot_trigger_pause (profile, agent, trigger) values ($1, $2, $3) on conflict do nothing",
    )
    .bind(&spec.profile.0)
    .bind(&spec.agent.0)
    .bind(&spec.trigger)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

fn hex_bytes(text: &str) -> R<Vec<u8>> {
    if text.len() != 64 || !text.is_ascii() {
        return invalid();
    }
    (0..32)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|_| Error::Invalid.into()))
        .collect()
}

/// 접수에서 본 이 트리거의 활성 승인과 정지 상태.
pub(crate) enum Binding {
    /// 승인이 없다(사전 승인 선언이 없는 트리거).
    None,
    Bound {
        grant: String,
        epoch: i64,
    },
    Paused,
}

/// 발생 접수 안에서 활성 승인과 정지 상태를 읽는다(정지 행을 `FOR SHARE`로 잠가 정지 커밋과 직렬화한다).
/// 정지 중이거나, 발생의 **예정 시각**(`at_ms`)이 마지막 정지·재개 전환보다 앞서면 건너뛴다: 정지 구간에 예정된 발생(또는 정지 전에 예정됐지만 밀려 접수되는 발생)을
/// 재개 뒤 따라잡기가 접수해 쓰게 하지 않는다. 재개 뒤에 예정된 새 발생부터 받는다.
pub(crate) async fn binding_in(
    tx: &mut Tx<'_>,
    profile: &str,
    agent: &str,
    trigger: &str,
    at_ms: i64,
) -> R<Binding> {
    let grant: Option<String> = sqlx::query_scalar(
        "select id from bot_grants where profile = $1 and agent = $2 and trigger = $3 and state = 'active'",
    )
    .bind(profile)
    .bind(agent)
    .bind(trigger)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(grant) = grant else {
        return Ok(Binding::None);
    };
    let pause = sqlx::query(
        "select epoch, paused, changed_ms from bot_trigger_pause where profile = $1 and agent = $2 and trigger = $3 for share",
    )
    .bind(profile)
    .bind(agent)
    .bind(trigger)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match pause {
        Some(row)
            if row.get::<bool, _>("paused")
                || row
                    .get::<Option<i64>, _>("changed_ms")
                    .is_some_and(|changed| at_ms < changed) =>
        {
            Binding::Paused
        }
        Some(row) => Binding::Bound {
            grant,
            epoch: row.get("epoch"),
        },
        None => Binding::Paused,
    })
}

/// 새 run을 승인에 묶는다(접수 커밋 안에서).
pub(crate) async fn bind_run(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    grant: &str,
    epoch: i64,
) -> R<()> {
    sqlx::query(
        "insert into bot_run_grants (profile, run, grant_id, epoch) values ($1, $2, $3, $4)",
    )
    .bind(profile)
    .bind(run)
    .bind(grant)
    .bind(epoch)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn row_of(row: &sqlx::postgres::PgRow) -> PreapprovalRow {
    PreapprovalRow {
        profile: row.get("profile"),
        agent: row.get("agent"),
        trigger: row.get("trigger"),
        grant_id: row.get("id"),
        epoch: row.get("epoch"),
        paused: row.get("paused"),
        used_today: row.get::<i32, _>("used") as i64,
        max_per_day: i64::from(row.get::<i32, _>("max_per_day")),
    }
}

const LIST: &str = "select g.id, g.profile, g.agent, g.trigger, g.max_per_day, p.epoch, p.paused,
                           coalesce(u.used, 0) as used
                    from bot_grants g
                    join bot_trigger_pause p
                      on p.profile = g.profile and p.agent = g.agent and p.trigger = g.trigger
                    left join bot_grant_usage u
                      on u.profile = g.profile and u.agent = g.agent and u.trigger = g.trigger
                     and u.day = bot_now_ms() / 86400000
                    where g.state = 'active'";

impl Store {
    /// 운영자 전용. 활성 승인마다 정지 세대·정지 여부·오늘(UTC) 사용량을 한 문장(한 스냅숏)으로 읽는다.
    pub(crate) async fn admin_preapprovals(&self, viewer: &Viewer) -> R<Vec<PreapprovalRow>> {
        if !viewer.operator {
            return Err(Error::Denied.into());
        }
        self.check_pinned_pool().await?;
        let rows = sqlx::query(&format!("{LIST} order by g.profile, g.agent, g.trigger"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_of).collect())
    }

    /// 읽기 전용 경로의 활성 집합 확인(고정한 핸들이면 지금도 현재인지).
    async fn check_pinned_pool(&self) -> R<()> {
        let mut tx = self.pool.begin().await?;
        self.check_pinned(&mut tx).await?;
        tx.rollback().await?;
        Ok(())
    }

    /// 정지(`pause`)·재개. 승인 ID와 세대를 현재와 대조해 전환과 감사를 한 커밋에 쓴다. 같은 세대에서 이미 그 상태이거나, 직전 성공 명령의 재전달
    /// (그 뒤 전환이 없었다)이면 `repeated`다(변경·감사 0). 승인이 교체됐거나 그 사이 전환이 있었으면 `Stale`(상태가 우연히 같아도 같다).
    pub(crate) async fn pause_run(
        &self,
        actor: &PrincipalId,
        key: &TriggerKey,
        cas: &GrantCas,
        pause: bool,
    ) -> R<PauseOutcome> {
        ids_ok(&[
            &actor.0,
            &key.profile.0,
            &key.agent.0,
            &key.trigger,
            &cas.grant_id,
        ])?;
        if cas.epoch < 0 {
            return invalid();
        }
        let mut tx = self.begin_authorized().await?;
        sqlx::query("select 1 from bot_active where singleton for share")
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Stale)?;
        let grant = sqlx::query(
            "select id from bot_grants
             where profile = $1 and agent = $2 and trigger = $3 and state = 'active' for share",
        )
        .bind(&key.profile.0)
        .bind(&key.agent.0)
        .bind(&key.trigger)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let grant: String = grant.get("id");
        if grant != cas.grant_id {
            return Err(Error::Stale.into());
        }
        let state = sqlx::query(
            "select epoch, paused, last_from, last_kind, last_grant from bot_trigger_pause
             where profile = $1 and agent = $2 and trigger = $3 for update",
        )
        .bind(&key.profile.0)
        .bind(&key.agent.0)
        .bind(&key.trigger)
        .fetch_one(&mut *tx)
        .await?;
        let current = PauseState {
            epoch: state.get("epoch"),
            paused: state.get("paused"),
            last: state
                .get::<Option<i64>, _>("last_from")
                .zip(state.get::<Option<String>, _>("last_kind"))
                .zip(state.get::<Option<String>, _>("last_grant"))
                .map(|((from, kind), grant)| (from, kind == "pause", grant)),
        };
        let kind = if pause { "pause" } else { "resume" };
        let repeated = match decide_pause(&grant, &current, cas, pause) {
            PauseDecision::Stale => return Err(Error::Stale.into()),
            PauseDecision::Repeated => true,
            PauseDecision::Apply => false,
        };
        if !repeated {
            sqlx::query(
                "update bot_trigger_pause set epoch = epoch + 1, paused = $4, last_from = epoch, last_kind = $5,
                        last_grant = $6, changed_ms = bot_now_ms(), updated_ms = bot_now_ms()
                 where profile = $1 and agent = $2 and trigger = $3",
            )
            .bind(&key.profile.0)
            .bind(&key.agent.0)
            .bind(&key.trigger)
            .bind(pause)
            .bind(kind)
            .bind(&grant)
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query(&format!(
            "{LIST} and g.profile = $1 and g.agent = $2 and g.trigger = $3"
        ))
        .bind(&key.profile.0)
        .bind(&key.agent.0)
        .bind(&key.trigger)
        .fetch_one(&mut *tx)
        .await?;
        let row = row_of(&row);
        if !repeated {
            let action = if pause {
                "preapproval.pause"
            } else {
                "preapproval.resume"
            };
            audit::append(
                &mut tx,
                &key.profile.0,
                &Entry::new(
                    &actor.0,
                    action,
                    &format!("{}/{}", key.agent.0, key.trigger),
                    &format!("{grant}:epoch={}", row.epoch),
                ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(PauseOutcome { row, repeated })
    }
}

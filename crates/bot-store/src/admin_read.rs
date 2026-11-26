//! 관리 조회(AdminRead). 모든 조회는 인증된 `Viewer`로 한정한다: 운영자는 호스트 전체, 그 밖의 주체는 자기 요청과
//! 자기에게 지정된(또는 자기가 요청한) 승인만 본다. 보이지 않는 대상은 `Missing`이다(존재 여부도 알리지 않는다).
//! 읽기 전용이며 인자·본문·비밀 원문을 싣지 않는다(해시와 브로커가 렌더링한 카드 줄만).

use crate::{
    audit,
    calls::risk_from,
    codec::*,
    db::{R, Store, ms_to_time, time_to_ms},
};
use bot_core::{admin::*, id::*, port::Error};
use sqlx::{PgExecutor, Row, postgres::PgRow};
use std::collections::BTreeMap;

const RUN_COLUMNS: &str = "r.*,
    (select x.private_memory_read from bot_runs x where x.profile = r.profile and x.id = r.root) as tree_private,
    (select c.call from bot_calls c where c.profile = r.profile and c.run = r.id and c.state = 'held' and c.resolution is null
      order by c.updated_ms limit 1) as held_call";

fn operator(viewer: &Viewer) -> R<()> {
    if viewer.operator {
        Ok(())
    } else {
        Err(Error::Denied.into())
    }
}

/// 실행 목록의 커서 `<갱신 시각 ms>:<프로필>:<run ID>`. 같은 run ID가 다른 프로필에 있을 수 있으므로 프로필이 정렬 키에 든다.
fn run_cursor(after: &Option<String>) -> Option<(i64, String, String)> {
    let mut parts = after.as_deref()?.splitn(3, ':');
    let ms = parts.next()?.parse().ok()?;
    let profile = parts.next()?.to_owned();
    Some((ms, profile, parts.next()?.to_owned()))
}

fn run_view(row: &PgRow) -> R<RunView> {
    let waiting = match row.get::<Option<String>, _>("wait_kind").as_deref() {
        Some("approval") => Some(Waiting::Approval),
        Some("child") => Some(Waiting::Child(RunId(
            row.get::<Option<String>, _>("wait_child")
                .unwrap_or_default(),
        ))),
        Some("hold") => Some(Waiting::Hold(CallId(
            row.get::<Option<String>, _>("held_call")
                .unwrap_or_default(),
        ))),
        _ => None,
    };
    Ok(RunView {
        profile: ProfileId(row.get("profile")),
        run: RunId(row.get("id")),
        root: RunId(row.get("root")),
        parent: row.get::<Option<String>, _>("parent").map(RunId),
        step: row.get("parent_step"),
        agent: AgentId(row.get("agent")),
        principal: PrincipalId(row.get("principal")),
        kind: kind_from(&row.get::<String, _>("conv_kind"))?,
        trigger: row.get("trigger"),
        state: state_from(&row.get::<String, _>("state"))?,
        generation: Generation(row.get::<i64, _>("generation") as u64),
        attempts: row.get::<i32, _>("attempts").max(0) as u32,
        waiting,
        private_memory_read: row.get("tree_private"),
        error: row.get("error"),
        created_at: ms_to_time(row.get("created_ms")),
        updated_at: ms_to_time(row.get("updated_ms")),
    })
}

/// 호출의 고정 실패 분류. 저장된 상태·사유 코드에서 기계적으로 옮기고 원문·추측은 쓰지 않는다.
/// 거절(`denied`)은 Denied. 보류(`held`)는 사유가 `timeout`이면 Timeout, 결과를 막은 `result_*`면 InvalidResult,
/// 그 밖에는 결과를 알 수 없어 멈춘 것이므로 OutcomeUnknown이다. 그 밖의 상태는 분류가 없다(None).
fn call_error(row: &PgRow) -> Option<CallError> {
    match row.get::<String, _>("state").as_str() {
        "denied" => Some(CallError::Denied),
        "held" => Some(match row.get::<Option<String>, _>("reason").as_deref() {
            Some("timeout") => CallError::Timeout,
            Some(code) if code.starts_with("result_") => CallError::InvalidResult,
            _ => CallError::OutcomeUnknown,
        }),
        _ => None,
    }
}

fn call_view(row: &PgRow) -> R<CallView> {
    let state = match (
        row.get::<String, _>("state").as_str(),
        row.get::<Option<String>, _>("approval"),
    ) {
        ("done", _) => CallState::Done,
        ("held", _) => CallState::Held,
        ("denied", _) => CallState::Denied,
        ("open", Some(_)) => CallState::Awaiting,
        _ => CallState::Running,
    };
    Ok(CallView {
        error: call_error(row),
        call: CallId(row.get("call")),
        run: RunId(row.get("run")),
        generation: Generation(row.get::<Option<i64>, _>("started_generation").unwrap_or(0) as u64),
        tool: ToolId(row.get("tool")),
        tool_revision: Revision(
            row.get::<Option<String>, _>("tool_revision")
                .unwrap_or_default(),
        ),
        risk: row
            .get::<Option<String>, _>("risk")
            .map_or(Ok(bot_core::state::Risk::Read), |r| risk_from(&r))?,
        state,
        args_sha256: to32(row.get("args_sha256"))?,
        started_at: ms_to_time(
            row.get::<Option<i64>, _>("started_ms")
                .unwrap_or_else(|| row.get("created_ms")),
        ),
        finished_at: row.get::<Option<i64>, _>("finished_ms").map(ms_to_time),
    })
}

/// 단계 한 줄. 운영자가 미실행으로 판정한 보류 단계(`resolution`이 있다)는 더 이상 보류가 아니다:
/// 다시 보낼 수 있는 시작 상태로 보이고 사유는 비운다(미결 단계만 held다).
fn effect_view(row: &PgRow, holds: &BTreeMap<String, HoldId>) -> R<EffectView> {
    let decided = row.get::<Option<String>, _>("resolution").is_some();
    let mut state = crate::effects::state_of(&row.get::<String, _>("state"))?;
    if decided && state == bot_core::state::EffectState::Held {
        state = bot_core::state::EffectState::Started;
    }
    Ok(EffectView {
        // 운영자가 미실행을 확인한 보류 단계만(원래 상태가 held이고 resolution이 not_executed일 때). 다시 시작한 뒤에는 표시하지 않는다
        resolution: (row.get::<String, _>("state") == "held"
            && row.get::<Option<String>, _>("resolution").as_deref() == Some("not_executed"))
        .then_some(EffectResolution::NotExecuted),
        // 지금 판정할 수 있는 보류(열린 발생)가 있는 단계만 ID를 가진다
        hold_id: holds.get(&row.get::<String, _>("id")).cloned(),
        effect: EffectId(row.get("id")),
        run: RunId(row.get("run")),
        call: CallId(row.get("call")),
        step: row.get("step"),
        state,
        receipt: row.get("receipt"),
        reason: if decided { None } else { row.get("reason") },
        args_sha256: to32(row.get("args_sha256"))?,
        generation: Generation(row.get::<i64, _>("generation") as u64),
        at: ms_to_time(row.get("updated_ms")),
    })
}

/// 열린 단계 보류의 발생 ID(단계 ID별). `call`이 있으면 그 호출의 것만.
async fn step_holds<'e>(
    ex: impl PgExecutor<'e>,
    profile: &str,
    run: &str,
    call: Option<&str>,
) -> R<BTreeMap<String, HoldId>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select effect, id from bot_holds
         where profile = $1 and run = $2 and ($3::text is null or call = $3) and kind = 'tool' and effect is not null and closed_ms is null",
    )
    .bind(profile)
    .bind(run)
    .bind(call)
    .fetch_all(ex)
    .await?;
    Ok(rows.into_iter().map(|(e, id)| (e, HoldId(id))).collect())
}

/// 호출 단위(단계 없는) 열린 발생 ID. 종류가 같은 호출 ID를 가른다(도구 호출·회신 발송).
async fn call_hold<'e>(
    ex: impl PgExecutor<'e>,
    profile: &str,
    run: &str,
    call: &str,
    kind: &str,
) -> R<Option<HoldId>> {
    let id: Option<String> = sqlx::query_scalar(
        "select id from bot_holds
         where profile = $1 and run = $2 and call = $3 and kind = $4 and effect is null and closed_ms is null",
    )
    .bind(profile)
    .bind(run)
    .bind(call)
    .bind(kind)
    .fetch_optional(ex)
    .await?;
    Ok(id.map(HoldId))
}

impl Store {
    pub(crate) async fn admin_runs(&self, viewer: &Viewer, q: RunQuery) -> R<Listing<RunView>> {
        let limit = i64::from(q.page.limit.clamp(1, 200));
        let after = run_cursor(&q.page.after);
        // 갱신 시각 최신순, 같으면 프로필·run ID 오름차순(바이트 순서: 조회 가짜와 같은 동률 순서)
        let rows = sqlx::query(&format!(
            "select {RUN_COLUMNS} from bot_runs r
             where ($1::text is null or r.profile = $1) and ($2::text is null or r.state = $2) and ($3::text is null or r.root = $3)
               and ($4 or r.principal = $5)
               and ($6::bigint is null or r.updated_ms < $6
                    or (r.updated_ms = $6 and (r.profile collate \"C\" > $7 collate \"C\"
                        or (r.profile = $7 and r.id collate \"C\" > $8 collate \"C\"))))
             order by r.updated_ms desc, r.profile collate \"C\" asc, r.id collate \"C\" asc limit $9"
        ))
        .bind(q.profile.as_ref().map(|p| p.0.as_str()))
        .bind(q.state.map(state_text))
        .bind(q.root.as_ref().map(|r| r.0.as_str()))
        .bind(viewer.operator)
        .bind(&viewer.principal.0)
        .bind(after.as_ref().map(|c| c.0))
        .bind(after.as_ref().map(|c| c.1.clone()).unwrap_or_default())
        .bind(after.as_ref().map(|c| c.2.clone()).unwrap_or_default())
        .bind(limit + 1)
        .fetch_all(&self.pool)
        .await?;
        let more = rows.len() as i64 > limit;
        let items = rows
            .iter()
            .take(limit as usize)
            .map(run_view)
            .collect::<R<Vec<_>>>()?;
        let next = more
            .then(|| {
                items
                    .last()
                    .map(|r| format!("{}:{}:{}", time_to_ms(r.updated_at), r.profile.0, r.run.0))
            })
            .flatten();
        Ok(Listing { items, next })
    }

    pub(crate) async fn admin_detail(
        &self,
        viewer: &Viewer,
        profile: &ProfileId,
        run: &RunId,
    ) -> R<RunDetail> {
        let mut tx = self.pool.begin().await?; // 같은 스냅숏에서 읽는다
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(&format!(
            "select {RUN_COLUMNS} from bot_runs r where r.profile = $1 and r.id = $2 and ($3 or r.principal = $4)"
        ))
        .bind(&profile.0)
        .bind(&run.0)
        .bind(viewer.operator)
        .bind(&viewer.principal.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let control = if viewer.operator
            && !row.get::<bool, _>("private")
            && row.get::<String, _>("root") == run.0
        {
            Some(RunControlView {
                claim_epoch: ClaimEpoch(row.get::<i64, _>("claim_epoch") as u64),
                cancel_requested: row.get("cancel_requested"),
                held: crate::admin::tree_has_open_hold(&mut tx, &profile.0, &run.0).await?,
            })
        } else {
            None
        };
        let lineage = sqlx::query(&format!(
            "with recursive up as (
                 select profile, id, parent, 0 as n from bot_runs where profile = $1 and id = $2
                 union all select p.profile, p.id, p.parent, up.n + 1 from bot_runs p join up on p.profile = up.profile and p.id = up.parent
             ) select {RUN_COLUMNS} from bot_runs r join up on up.profile = r.profile and up.id = r.id order by up.n desc"
        ))
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_all(&mut *tx)
        .await?;
        let children = sqlx::query(&format!(
            "select {RUN_COLUMNS} from bot_runs r where r.profile = $1 and r.parent = $2 order by r.created_ms, r.id"
        ))
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_all(&mut *tx)
        .await?;
        let calls = sqlx::query(&format!(
            "select {} from bot_calls where profile = $1 and run = $2 order by created_ms, call",
            crate::cols::CALLS
        ))
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_all(&mut *tx)
        .await?;
        let effects = sqlx::query(&format!(
            "select {} from bot_effects where profile = $1 and run = $2 order by created_ms, id",
            crate::cols::EFFECTS
        ))
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_all(&mut *tx)
        .await?;
        let holds = step_holds(&mut *tx, &profile.0, &run.0, None).await?;
        tx.rollback().await?;
        Ok(RunDetail {
            run: run_view(&row)?,
            control,
            lineage: lineage.iter().map(run_view).collect::<R<_>>()?,
            children: children.iter().map(run_view).collect::<R<_>>()?,
            calls: calls.iter().map(call_view).collect::<R<_>>()?,
            effects: effects
                .iter()
                .map(|e| effect_view(e, &holds))
                .collect::<R<_>>()?,
        })
    }

    /// 열린 보류: 호출 단위는 판정 전까지, 다단계는 미결 보류 단계가 남은 동안. 회신 발송·민감 DM의 보류도 함께 싣는다.
    pub(crate) async fn admin_holds(
        &self,
        viewer: &Viewer,
        profile: Option<&ProfileId>,
    ) -> R<Vec<HoldView>> {
        operator(viewer)?;
        self.sweep_private().await?; // 리스가 끝난 민감 DM 접수는 재전달 없이도 보류로 보인다
        let profile = profile.map(|p| p.0.as_str());
        // 보류 내용(사유·단계)과 발생 ID를 한 스냅숏에서 읽는다: 읽는 도중 같은 대상이 다시 보류돼도 한 카드에 옛 사유와 새 ID가 섞이지 않는다
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let calls = sqlx::query(
            &format!("select {} from bot_calls c
             where c.state = 'held' and ($1::text is null or c.profile = $1)
               and not exists (select 1 from bot_runs r where r.profile = c.profile and r.id = c.run and r.private)
               and c.resolution is null and exists (
                    select 1 from bot_holds h where h.profile = c.profile and h.run = c.run and h.call = c.call
                      and h.kind = 'tool' and h.closed_ms is null)
             order by c.updated_ms, c.call", crate::cols::prefixed(crate::cols::CALLS, "c")),
        )
        .bind(profile)
        .fetch_all(&mut *tx)
        .await?;
        let mut out = Vec::new();
        for c in &calls {
            let steps = sqlx::query(&format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 order by created_ms, id", crate::cols::EFFECTS))
                .bind(c.get::<String, _>("profile"))
                .bind(c.get::<String, _>("run"))
                .bind(c.get::<String, _>("call"))
                .fetch_all(&mut *tx)
                .await?;
            let (profile, run, call) = (
                c.get::<String, _>("profile"),
                c.get::<String, _>("run"),
                c.get::<String, _>("call"),
            );
            let holds = step_holds(&mut *tx, &profile, &run, Some(&call)).await?;
            let hold_id = call_hold(&mut *tx, &profile, &run, &call, "tool").await?;
            out.push(HoldView {
                kind: HoldKind::Tool,
                hold_id,
                profile: ProfileId(profile),
                run: RunId(run),
                call: CallId(call),
                tool: ToolId(c.get("tool")),
                reason: c.get::<Option<String>, _>("reason").unwrap_or_default(),
                since: ms_to_time(c.get("updated_ms")),
                steps: steps
                    .iter()
                    .map(|e| effect_view(e, &holds))
                    .collect::<R<_>>()?,
            });
        }
        let replies = sqlx::query(
            "select profile, run, call, coalesce(reason, 'unknown_outcome') as reason, updated_ms from bot_outbox
             where state = 'held' and ($1::text is null or profile = $1) order by updated_ms, run, seq",
        )
        .bind(profile)
        .fetch_all(&mut *tx)
        .await?;
        let privates = sqlx::query(
            "select profile, id, target->>'tool' as tool, updated_ms from bot_runs
             where private and state = 'waiting' and wait_kind = 'hold' and ($1::text is null or profile = $1) order by updated_ms, id",
        )
        .bind(profile)
        .fetch_all(&mut *tx)
        .await?;
        // 회신 발송·민감 DM은 단계가 없는 호출 단위 보류다: 열린 발생 ID가 곧 판정 ID다
        for r in &replies {
            let (profile, run, call): (String, String, String) =
                (r.get("profile"), r.get("run"), r.get("call"));
            out.push(HoldView {
                kind: HoldKind::Reply,
                hold_id: call_hold(&mut *tx, &profile, &run, &call, "reply").await?,
                profile: ProfileId(profile),
                run: RunId(run),
                call: CallId(call),
                tool: ToolId("reply".into()),
                reason: r.get("reason"),
                since: ms_to_time(r.get("updated_ms")),
                steps: vec![],
            });
        }
        for r in &privates {
            let (profile, run): (String, String) = (r.get("profile"), r.get("id"));
            let call = format!("private:{run}");
            out.push(HoldView {
                kind: HoldKind::Private,
                hold_id: call_hold(&mut *tx, &profile, &run, &call, "private").await?,
                profile: ProfileId(profile),
                run: RunId(run),
                call: CallId(call),
                tool: ToolId(r.get::<Option<String>, _>("tool").unwrap_or_default()),
                reason: "unknown_outcome".into(),
                since: ms_to_time(r.get("updated_ms")),
                steps: vec![],
            });
        }
        tx.rollback().await?;
        Ok(out)
    }

    pub(crate) async fn admin_audit(&self, viewer: &Viewer, q: AuditQuery) -> R<Vec<AuditEntry>> {
        operator(viewer)?;
        let rows = sqlx::query(
            &format!("select {} from bot_audit where ($1::text is null or profile = $1) and ($2::bigint is null or seq < $2)
             order by seq desc limit $3", crate::cols::AUDIT),
        )
        .bind(q.profile.as_ref().map(|p| p.0.as_str()))
        .bind(q.before.map(|b| b as i64))
        .bind(i64::from(q.limit.clamp(1, 201)))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AuditEntry {
                    seq: r.get::<i64, _>("seq") as u64,
                    at: ms_to_time(r.get("at_ms")),
                    prev: to32(r.get("prev_hash"))?,
                    hash: to32(r.get("hash"))?,
                    profile: r.get::<Option<String>, _>("profile").map(ProfileId),
                    actor: PrincipalId(r.get("actor")),
                    action: r.get("action"),
                    target: r.get("target"),
                    args_sha256: r
                        .get::<Option<Vec<u8>>, _>("args_sha256")
                        .map(to32)
                        .transpose()?,
                    verdict: r.get("verdict"),
                })
            })
            .collect()
    }

    pub(crate) async fn admin_verify(&self, viewer: &Viewer) -> R<AuditCheck> {
        operator(viewer)?;
        let report = audit::verify_chain(self).await?;
        Ok(AuditCheck {
            entries: report.entries,
            head: report.head,
            broken_at: report.first_bad,
        })
    }

    pub(crate) async fn admin_status(&self, viewer: &Viewer) -> R<Vec<ProfileStatus>> {
        operator(viewer)?;
        let holds = self.admin_holds(viewer, None).await?;
        let profiles: Vec<String> = sqlx::query_scalar(
            "select profile from bot_runs union select profile from bot_approvals order by 1",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for profile in profiles {
            let counts = sqlx::query("select state, count(*) as n from bot_runs where profile = $1 group by state order by state")
                .bind(&profile)
                .fetch_all(&self.pool)
                .await?;
            let oldest: Option<i64> = sqlx::query_scalar(
                "select (bot_now_ms() - min(greatest(created_ms, not_before_ms))) / 1000 from bot_runs
                 where profile = $1 and state in ('ready', 'retry') and not private and not_before_ms <= bot_now_ms()",
            )
            .bind(&profile)
            .fetch_one(&self.pool)
            .await?;
            let pending: i64 = sqlx::query_scalar(
                "select count(*) from bot_approvals a where a.profile = $1 and a.state = 'pending'
                   and not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted')",
            )
            .bind(&profile)
            .fetch_one(&self.pool)
            .await?;
            out.push(ProfileStatus {
                runs: counts
                    .iter()
                    .map(|r| {
                        Ok((
                            state_from(&r.get::<String, _>("state"))?,
                            r.get::<i64, _>("n") as u64,
                        ))
                    })
                    .collect::<R<_>>()?,
                oldest_ready_secs: oldest.map(|s| s.max(0) as u64),
                approvals_pending: pending as u64,
                holds: holds.iter().filter(|h| h.profile.0 == profile).count() as u64,
                profile: ProfileId(profile),
            });
        }
        Ok(out)
    }
}

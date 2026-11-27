//! 키로 이어지는 긴 요청 입구(OpenAI 호환 HTTP)의 원자 accept-or-replay와 범위가 정해진 조회(`bot_core::api`).
//!
//! 기존 접수 트랜잭션(`accept.rs`: 접수 행 `bot_requests` + `bot_runs` + 감사)을 그대로 쓴다. 별도 원장이 아니다. 한 트랜잭션에서
//! `활성 집합 펜스 → (재전달이면 기존 작업) → 입구 잠금 → 활성 판본 대조 → 진행 중 작업 수 한도 → 접수 행·run·감사`를 처리하고 커밋한다.
//! 커밋 응답을 잃어 확정할 수 없으면 `Unknown`이다(커밋 전 실패는 `Err(Unavailable)`이라 호출자가 안전하게 503으로 돌릴 수 있다).
//!
//! 잠금 순서: 활성 집합 → 입구 잠금(`pg_advisory_xact_lock`, 이 트랜잭션이 처음 쥔다) → 활성 판본 → 새 run(아직 아무도 모르는 행) → 감사 head.
//! 다른 어떤 경로도 입구 잠금을 쥔 채 다른 행을 기다리지 않는다(입구 잠금은 접수만 쥔다).

use crate::{
    accept::{check, insert_run},
    audit::{self, Entry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::final_text,
};
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiKey, ApiRun, ApiRuns, ApiState, ApprovalBlock},
    command::{Target, TokenUsage},
    id::*,
    identity::{IdentityKind, identity_kind},
    port::{Error, Reply},
    state::{ConversationKind, RunOrigin, RunState},
};
use sqlx::Row;

/// 끝나지 않은 run 상태(승인·자식·보류로 기다리는 것도 센다).
const UNFINISHED: &str = "('ready', 'leased', 'waiting', 'retry')";

/// 한 접수 행과 그 run을 읽는 질의. `$1..$4`가 (프로필, 입구, 주체, 키)다. 막힌 이유는 루트와 위임 자식에 **저장된** 판정에서만 읽는다:
/// 호출이 승인 경로 때문에 `denied`로 닫히며 남긴 낱말(`bot_calls.reason`)과, DM 전달 작업이 실패·불명으로 닫힌 기록(`bot_card_jobs.closed_reason`).
const VIEW: &str = "select r.id, r.created_ms, r.state, r.wait_kind, r.result, r.error, r.binding, r.agent, r.revision,
        r.truncated, r.usage_prompt, r.usage_completion, q.api_model, q.input_sha256,
        (select c.reason from bot_calls c join bot_runs t on t.profile = c.profile and t.id = c.run
          where t.profile = r.profile and t.root = r.id and c.state = 'denied'
            and c.reason in ('no_approver', 'approver_revoked', 'route_changed', 'approval_dm_unavailable')
          order by c.finished_ms nulls last, c.call limit 1) as call_block,
        (select j.closed_reason from bot_card_jobs j join bot_runs t on t.profile = j.profile and t.id = j.run
          where t.profile = r.profile and t.root = r.id and j.state = 'closed'
            and j.closed_reason in ('unavailable', 'held', 'no_approver', 'approver_revoked', 'route_changed')
          order by j.updated_ms, j.nonce limit 1) as job_block,
        exists (select 1 from bot_approvals a join bot_runs t on t.profile = a.profile and t.id = a.run
          where t.profile = r.profile and t.root = r.id and t.state = 'waiting' and t.wait_kind = 'approval'
            and a.state = 'pending' and a.generation = t.generation and a.expires_ms > bot_now_ms()
            and not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted')) as needs_human
     from bot_requests q join bot_runs r on r.profile = q.profile and r.id = q.run
     where q.profile = $1 and q.api_endpoint = $2 and q.principal = $3 and q.request = $4";

fn blocked(row: &sqlx::postgres::PgRow) -> Option<ApprovalBlock> {
    row.get::<Option<String>, _>("call_block")
        .and_then(|code| ApprovalBlock::parse(&code))
        .or_else(|| {
            // DM 전달 작업의 닫힌 기록: 전달 실패·불명(`unavailable`·`held`)은 DM 전달 실패이고, 나머지는 승인 경로 판정 낱말 그대로다
            row.get::<Option<String>, _>("job_block")
                .and_then(|reason| match reason.as_str() {
                    "unavailable" | "held" => Some(ApprovalBlock::ApprovalDmUnavailable),
                    other => ApprovalBlock::parse(other),
                })
        })
}

fn view(row: &sqlx::postgres::PgRow) -> R<ApiRun> {
    let run_state = state_from(&row.get::<String, _>("state"))?;
    let verdict = blocked(row);
    let state = match run_state {
        RunState::Succeeded => match verdict {
            Some(why) => ApiState::ApprovalBlocked(why),
            None => ApiState::Final {
                text: final_text(&row.get::<Option<String>, _>("result").unwrap_or_default()),
                truncated: row.get::<Option<bool>, _>("truncated").unwrap_or(false),
                usage: row
                    .get::<Option<i64>, _>("usage_prompt")
                    .zip(row.get::<Option<i64>, _>("usage_completion"))
                    .map(|(prompt, completion)| TokenUsage {
                        prompt: prompt.max(0) as u64,
                        completion: completion.max(0) as u64,
                    }),
            },
        },
        RunState::Dead => ApiState::Failed {
            reason: row
                .get::<Option<String>, _>("error")
                .unwrap_or_else(|| "run_failed".to_owned()),
        },
        RunState::Cancelled => ApiState::Cancelled,
        // 진행 중: 사람이 지금 결정할 수 있는 카드가 루트 또는 대기 중인 위임 자손에 있으면 승인 대기다(Slack 카드는 게시 영수증이 확정된 뒤부터).
        // 자식 대기·보류만으로, 또는 DM 카드가 아직 쌓이는 중이거나 올라가는 중이면 승인 대기가 아니다
        RunState::Waiting | RunState::Ready | RunState::Leased | RunState::Retry => {
            if row.get::<bool, _>("needs_human") {
                ApiState::NeedsHuman
            } else if run_state == RunState::Waiting
                && row.get::<Option<String>, _>("wait_kind").as_deref() == Some("hold")
            {
                ApiState::Held
            } else {
                verdict.map_or(ApiState::Active, ApiState::ApprovalBlocked)
            }
        }
    };
    Ok(ApiRun {
        run: RunId(row.get("id")),
        created: row.get::<i64, _>("created_ms").max(0) as u64 / 1000,
        model: row.get("api_model"),
        binding: BindingId(row.get("binding")),
        agent: AgentId(row.get("agent")),
        revision: Revision(row.get("revision")),
        digest: to32(row.get("input_sha256"))?,
        state,
    })
}

async fn find(tx: &mut Tx<'_>, key: &ApiKey) -> R<Option<ApiRun>> {
    let row = sqlx::query(VIEW)
        .bind(&key.profile.0)
        .bind(&key.endpoint)
        .bind(&key.principal.0)
        .bind(&key.key.0)
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(view).transpose()
}

/// 입구·주체·키·모델·한도·대상이 이 입구의 접수로 쓸 수 있는 모양인가.
fn valid(req: &ApiAccept) -> R<()> {
    let e = &req.accept.event;
    let endpoint_of_principal = e
        .principal
        .0
        .strip_prefix("http:")
        .and_then(|rest| rest.split_once('/'))
        .map(|(endpoint, _)| endpoint);
    let ok = (1..=256).contains(&req.max_active)
        && id_ok(&req.endpoint)
        && !req.endpoint.contains('/')
        && identity_kind(&e.principal.0) == Some(IdentityKind::Http)
        && endpoint_of_principal == Some(req.endpoint.as_str())
        && (1..=128).contains(&e.request.0.len())
        && (1..=63).contains(&req.model.len())
        && id_ok(&req.model)
        && e.conversation.kind == ConversationKind::Http
        // 모델을 고르면 바인딩 에이전트를 직접 접수한다: 도구 직접 실행은 이 입구로 통하지 않는다
        && matches!(req.accept.target, Target::Agent { .. });
    if ok { Ok(()) } else { invalid() }
}

/// 이 SQLSTATE가 «커밋하지 않았다»를 **확정**하는가: 무결성 위반(23)과 확실한 롤백(40001 직렬화 실패·40002 무결성 롤백·40P01 교착)만이다.
/// `40003`(문장 완료 불명)·`08007`(트랜잭션 해결 불명)·연결 유실·관리자 종료(57P01) 등은 커밋됐을 수 있다.
fn rolled_back(code: &str) -> bool {
    code.starts_with("23") || matches!(code, "40001" | "40002" | "40P01")
}

/// 이 커밋 오류가 «커밋됐는지 알 수 없다»인가(서버가 위 코드로 거절한 것만 확실히 커밋 전이다).
fn commit_unknown(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => !db.code().is_some_and(|code| rolled_back(&code)),
        _ => true,
    }
}

impl Store {
    /// 원자 accept-or-replay(위 문서).
    pub(crate) async fn api_accept(&self, req: ApiAccept) -> R<ApiAdmission> {
        valid(&req)?;
        let a = &req.accept;
        let e = &a.event;
        let checked = check(a)?;
        let profile = &e.profile.0;
        let key = ApiKey {
            profile: e.profile.clone(),
            endpoint: req.endpoint.clone(),
            principal: e.principal.clone(),
            key: e.request.clone(),
        };
        let mut tx = match self.begin_authorized().await {
            Ok(tx) => tx,
            Err(Fail::Core(Error::Stale)) => return Ok(ApiAdmission::Stale),
            Err(other) => return Err(other),
        };
        // 재전달은 잠금 없이 끝난다(새 슬롯·새 실행 0)
        if let Some(found) = find(&mut tx, &key).await? {
            return Ok(replay(found, &req));
        }
        // 새 키: 같은 입구의 접수를 한 줄로 세운다. 같은 키의 동시 요청도 여기서 차례를 얻어 둘째는 첫째의 커밋을 본다
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("api:{profile}:{}", req.endpoint))
            .execute(&mut *tx)
            .await?;
        if let Some(found) = find(&mut tx, &key).await? {
            return Ok(replay(found, &req));
        }
        match crate::active::guard_active(&mut tx, profile, &a.revision).await {
            Ok(()) => {}
            Err(Fail::Core(Error::Stale)) => return Ok(ApiAdmission::Stale),
            Err(other) => return Err(other),
        }
        let active: i64 = sqlx::query_scalar(&format!(
            "select count(*) from bot_runs where profile = $1 and api_endpoint = $2 and state in {UNFINISHED}"
        ))
        .bind(profile)
        .bind(&req.endpoint)
        .fetch_one(&mut *tx)
        .await?;
        if active >= i64::from(req.max_active) {
            return Ok(ApiAdmission::Capacity);
        }
        let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        insert_run(
            &mut tx,
            a,
            &checked,
            &run,
            RunOrigin::from_ingress(&e.conversation.kind, &e.principal),
            None,
            None,
        )
        .await?;
        sqlx::query("update bot_runs set api_endpoint = $3 where profile = $1 and id = $2")
            .bind(profile)
            .bind(&run)
            .bind(&req.endpoint)
            .execute(&mut *tx)
            .await?;
        // 접수 행: 같은 키(입구 기준)가 이미 있으면 위에서 끝났다. 유일 색인이 마지막 방어선이다
        let inserted = sqlx::query(
            "insert into bot_requests (profile, binding, principal, request, input_sha256, run,
                                       http_method, http_path, http_response, api_endpoint, api_model, api_team)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) on conflict do nothing",
        )
        .bind(profile)
        .bind(&e.binding.0)
        .bind(&e.principal.0)
        .bind(&e.request.0)
        .bind(req.digest.as_slice())
        .bind(&run)
        .bind(e.http.as_ref().map(|h| h.method.as_str()))
        .bind(e.http.as_ref().map(|h| h.path.as_str()))
        .bind(e.http.as_ref().map(|h| http_response(h.response)))
        .bind(&req.endpoint)
        .bind(&req.model)
        .bind(&req.team)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !inserted {
            // 같은 (프로필, 바인딩, 주체, 키) 행이 다른 입구에 이미 있다: 이 입구의 접수가 아니다
            return Ok(ApiAdmission::Conflict);
        }
        let created = find(&mut tx, &key).await?.ok_or(Error::Unavailable)?;
        let mut entry = Entry::new(&e.principal.0, "run.accept", &run, "accepted");
        entry.args = Some(req.digest);
        audit::append(&mut tx, profile, &entry).await?;
        match tx.commit().await {
            Ok(()) => Ok(ApiAdmission::Accepted(created)),
            Err(error) if commit_unknown(&error) => Ok(ApiAdmission::Unknown),
            Err(error) => Err(error.into()),
        }
    }

    /// 범위가 정해진 조회: (프로필, 입구, 주체, 키)가 모두 맞는 접수만 보인다.
    pub(crate) async fn api_lookup(&self, key: ApiKey) -> R<Option<ApiRun>> {
        let mut tx = self.begin_authorized().await?;
        let found = find(&mut tx, &key).await?;
        tx.rollback().await?;
        Ok(found)
    }
}

fn http_response(mode: bot_core::profile::ResponseMode) -> &'static str {
    match mode {
        bot_core::profile::ResponseMode::Accepted => "accepted",
        bot_core::profile::ResponseMode::Sync => "sync",
    }
}

/// 같은 키의 기존 접수: 같은 지문·같은 모델이면 연결, 아니면 충돌. 지문에 모델이 들어 있어도 모델을 한 번 더 대조한다(같은 키의 의미가 둘로 나뉘지 않게).
fn replay(found: ApiRun, req: &ApiAccept) -> ApiAdmission {
    if found.digest == req.digest && found.model == req.model {
        ApiAdmission::Replayed(found)
    } else {
        ApiAdmission::Conflict
    }
}

impl ApiRuns for Store {
    fn accept(&self, request: ApiAccept) -> Reply<'_, ApiAdmission> {
        Box::pin(async move { self.api_accept(request).await.map_err(Fail::into_core) })
    }

    fn lookup(&self, key: ApiKey) -> Reply<'_, Option<ApiRun>> {
        Box::pin(async move { self.api_lookup(key).await.map_err(Fail::into_core) })
    }
}

#[cfg(test)]
mod tests {
    use super::rolled_back;

    #[test]
    fn only_a_certain_rollback_is_before_the_commit() {
        for certain in ["23503", "23505", "23514", "40001", "40002", "40P01"] {
            assert!(rolled_back(certain), "{certain}");
        }
        // 문장 완료 불명·트랜잭션 해결 불명·연결 예외·관리자 종료·내부 오류는 커밋됐을 수 있다
        for unknown in [
            "40003", "08007", "08006", "08000", "57P01", "57P02", "XX000", "53300", "",
        ] {
            assert!(!rolled_back(unknown), "{unknown}");
        }
    }
}

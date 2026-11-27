//! 큐: claim(SKIP LOCKED·세대 +1)·renew·checkpoint·fail(백오프·dead letter)·history.
//! 소유자의 모든 쓰기는 같은 트랜잭션에서 세대·리스를 대조한다. 긴 외부 호출 동안 잠금을 쥐지 않는다.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::*,
    handover,
};
use bot_core::{
    api::ApiOrigin,
    command::{Beat, Failure, JsonPayload, Retry, Step, Turn, Work},
    id::*,
    port::Error,
    state::*,
};
use serde_json::Value;
use sqlx::Row;
use std::{collections::BTreeSet, time::Duration};

/// 체크포인트 한 묶음의 최대 건수(한 모델 응답의 레코드·도구·그룹을 담기에 넉넉하다).
const MAX_BATCH: usize = 512;

struct Cand {
    run: RunRow,
    state: String,
    generation: i64,
    /// 직전 claim이 고정했던 호스트 활성 집합(없으면 구판 행·고정하지 않은 핸들)
    claim_set: Option<String>,
}

/// 가장 오래 기다린 집을 수 있는 run 하나를 잠근다(SKIP LOCKED): 때가 된 ready·retry, 또는 리스가 끝난 leased.
/// 직렬 키는 고유 색인과 같이 leased 상태가 소유한다. 만료된 소유자 자신은 인계할 수 있다.
async fn pick(
    tx: &mut Tx<'_>,
    skipped_profiles: &[String],
    skipped_runs: &[String],
) -> R<Option<Cand>> {
    let row = sqlx::query(include_str!("queue/pick.sql"))
        .bind(skipped_profiles)
        .bind(skipped_runs)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map(|r| Cand {
        run: RunRow {
            profile: r.get("profile"),
            id: r.get("id"),
            parent: r.get("parent"),
            parent_step: r.get("parent_step"),
            attempts: r.get("attempts"),
            cancel_requested: r.get("cancel_requested"),
            private: false,
        },
        state: r.get("state"),
        generation: r.get("generation"),
        claim_set: r.get("claim_set"),
    }))
}

/// 집은 run의 실행 내용을 읽는다.
pub(crate) async fn load(tx: &mut Tx<'_>, profile: &str, id: &str) -> R<RunContext> {
    let r = sqlx::query(&format!(
        "select {}, x.private_memory_read as tree_private, x.agent as root_agent,
                x.trigger as root_trigger, x.run_origin as root_origin,
                q.api_endpoint as root_api_endpoint, q.api_model as root_api_model
         from bot_runs r join bot_runs x on x.profile = r.profile and x.id = r.root
              left join bot_requests q on q.profile = x.profile and q.run = x.id and q.api_endpoint is not null
         where r.profile = $1 and r.id = $2",
        crate::cols::prefixed(crate::cols::RUNS, "r")
    ))
    .bind(profile)
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let steps =
        sqlx::query("select key, body from bot_steps where profile = $1 and run = $2 order by seq")
            .bind(profile)
            .bind(id)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .map(|s| Step {
                key: s.get("key"),
                body: JsonPayload(s.get("body")),
            })
            .collect();
    let target: Value = r.get("target");
    let scopes: Vec<String> = r.get("scopes");
    let thread: Option<String> = r.get("thread");
    Ok(RunContext {
        attempt: Attempt {
            profile: ProfileId(profile.to_owned()),
            run: RunId(id.to_owned()),
            claim: Some(ClaimEpoch(r.try_get::<i64, _>("claim_epoch")? as u64)),
            generation: Generation(r.get::<i64, _>("generation") as u64),
        },
        root: RunId(r.get("root")),
        origin: RunOrigin::from_recorded(r.get("root_origin")),
        principal: PrincipalId(r.get("principal")),
        agent: AgentId(r.get("agent")),
        conversation: Conversation {
            kind: kind_from(&r.get::<String, _>("conv_kind"))?,
            id: r.get("conv_id"),
            thread,
        },
        binding: BindingId(r.get("binding")),
        // 인증된 OpenAI 호환 입구의 접수 원장이 보증한 루트의 출처(위임 자식도 같다). 본문·레거시 webhook의 필드는 증거가 아니다
        api: r
            .get::<Option<String>, _>("root_api_endpoint")
            .zip(r.get::<Option<String>, _>("root_api_model"))
            .map(|(endpoint, model)| ApiOrigin { endpoint, model }),
        scopes: scopes.into_iter().collect::<BTreeSet<_>>(),
        private_memory_read: r.get("tree_private"),
        approver: r.get::<Option<String>, _>("approver").map(PrincipalId),
        // 예약 실행과 그 자식은 루트의 출처를 공유한다
        trigger: r
            .get::<Option<String>, _>("root_trigger")
            .map(|id| TriggerRef {
                agent: AgentId(r.get("root_agent")),
                id,
            }),
        work: Work {
            revision: Revision(r.get("revision")),
            target: target_from(&target)?,
            input: JsonPayload(r.get("input")),
            attachments: r.get("attachments"),
            steps,
            pass_pos: r
                .get::<Option<i32>, _>("pass_pos")
                .map(|pos| u32::try_from(pos).unwrap_or(0)),
        },
    })
}

impl Store {
    /// 가장 오래 기다린 run 하나를 집는다. 집을 때마다 세대가 오르고 리스가 시작되며, 이 핸들이 고정한 호스트 활성 집합을 같은 UPDATE에 기록한다.
    /// 리스가 끝난 leased는 인계다(시도 횟수 +1). 다만 직전 claim의 집합과 이 핸들의 집합이 둘 다 있고 다르면 활성 집합 교체 때문에 생긴 만료라
    /// 시도를 쓰지 않는다(`handover`). 한도를 넘었으면 집지 않고 dead letter로 보낸다.
    pub(crate) async fn claim_run(&self) -> R<Option<RunContext>> {
        self.expire_approvals().await?;
        self.sweep_private().await?;
        let (mut skipped_profiles, mut skipped_runs) = (Vec::new(), Vec::new());
        for _ in 0..8 {
            let mut tx = self.begin_authorized().await?;
            let Some(c) = pick(&mut tx, &skipped_profiles, &skipped_runs).await? else {
                tx.commit().await?;
                return Ok(None);
            };
            let run = c.run;
            let reclaimed = c.state == "leased";
            let pinned_rev = self.pinned_revision();
            let pinned = pinned_rev.as_ref().map(|set| set.0.as_str());
            let forgiven = reclaimed && !handover::spends_attempt(c.claim_set.as_deref(), pinned);
            let attempts = run.attempts + i32::from(reclaimed && !forgiven);
            let end = if run.cancel_requested {
                End::Cancelled("cancelled")
            } else if reclaimed && attempts >= self.cfg.max_attempts as i32 {
                End::Dead("lease_expired", attempts)
            } else {
                match load(&mut tx, &run.profile, &run.id).await {
                    Ok(mut claimed) => {
                        if let Err(error) = self
                            .lease_run(&mut tx, &mut claimed.attempt, attempts)
                            .await
                        {
                            // 같은 직렬 키의 다른 작업이 먼저 점유했다.
                            return if error.is_unique_violation() {
                                Ok(None)
                            } else {
                                Err(error)
                            };
                        }
                        if reclaimed {
                            let mut audits = Vec::new();
                            crate::approvals::supersede_generation_into(
                                &mut tx,
                                &run.profile,
                                &run.id,
                                c.generation,
                                &mut audits,
                            )
                            .await?;
                            let reason = if forgiven {
                                "set_replaced"
                            } else {
                                "lease_expired"
                            };
                            audits.push(OwnedEntry::new(
                                &run.profile,
                                "system",
                                "run.reclaim",
                                &run.id,
                                reason,
                            ));
                            audit::append_many(&mut tx, &audits).await?;
                        }
                        tx.commit().await?;
                        return Ok(Some(claimed));
                    }
                    // 저장한 실행 대상의 해독 오류만 격리한다. SQL 오류는 트랜잭션을 되돌린다.
                    Err(Fail::Core(Error::Invalid)) => End::Dead("invalid_target", attempts),
                    Err(error) => return Err(error),
                }
            };
            if !try_lock_parent(&mut tx, &run.profile, run.parent.as_deref()).await? {
                // 부모 → 자식 순서와 충돌하지 않도록 기다리지 않는다. 이번 점유에서 다른 후보를 본다.
                tx.rollback().await?;
                skipped_profiles.push(run.profile);
                skipped_runs.push(run.id);
                continue;
            }
            end_run(&mut tx, &run, end, "system").await?;
            tx.commit().await?;
        }
        Ok(None)
    }

    /// 잠근 후보의 작업을 해독한 뒤에만 새 점유 토큰을 발급한다.
    async fn lease_run(&self, tx: &mut Tx<'_>, at: &mut Attempt, attempts: i32) -> R<()> {
        let (generation, claim): (i64, i64) = sqlx::query_as(
            "update bot_runs set state = 'leased',
                    generation = case when keep_generation then generation else generation + 1 end,
                    keep_generation = false, claim_epoch = claim_epoch + 1, lease_owner = $3,
                    lease_until_ms = bot_now_ms() + $4, attempts = $5, wait_kind = null, wait_child = null,
                    claim_set = $6, updated_ms = bot_now_ms()
             where profile = $1 and id = $2 returning generation, claim_epoch",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&self.cfg.worker)
        .bind(self.cfg.lease.as_millis() as i64)
        .bind(attempts)
        .bind(self.pinned_revision().map(|set| set.0))
        .fetch_one(&mut **tx)
        .await?;
        at.generation = Generation(generation as u64);
        at.claim = Some(ClaimEpoch(claim as u64));
        Ok(())
    }

    /// heartbeat. 다른 소유권 검사와 같이 행을 잠근 뒤 DB 시각을 확인하고 리스를 늘린다.
    pub(crate) async fn renew_run(&self, at: &Attempt) -> R<Beat> {
        let mut tx = self.pool.begin().await?;
        let run = match lock_leased(&mut tx, at).await {
            Ok(run) if run.private => return Err(Error::Denied.into()),
            Err(Fail::Core(Error::Stale)) => {
                // 민감 DM 접수의 갱신은 토큰이나 만료 상태와 무관하게 전용 원장만 받는다.
                let private: Option<bool> = sqlx::query_scalar(
                    "select private from bot_runs where profile = $1 and id = $2",
                )
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .fetch_optional(&mut *tx)
                .await?;
                return Err(if private == Some(true) {
                    Error::Denied
                } else {
                    Error::Stale
                }
                .into());
            }
            other => other?,
        };
        sqlx::query(
            "update bot_runs set lease_until_ms = bot_now_ms() + $3, updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .bind(self.cfg.lease.as_millis() as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Beat {
            cancel_requested: run.cancel_requested,
        })
    }

    /// 체크포인트 묶음을 현재 claim으로 한 트랜잭션에 기록한다. 같은 key의 재기록은 같은 내용일 때만 받고(멱등),
    /// 하나라도 다르면 전체를 되돌린다. 비었거나 한 묶음 상한을 넘으면 Invalid다.
    pub(crate) async fn checkpoint_batch_run(&self, at: &Attempt, steps: Vec<Step>) -> R<()> {
        if steps.is_empty() || steps.len() > MAX_BATCH {
            return invalid();
        }
        let mut bodies = Vec::with_capacity(steps.len());
        for step in &steps {
            ids_ok(&[&step.key])?;
            bodies.push(canonical(&step.body)?.1);
        }
        let mut tx = self.pool.begin().await?;
        if lock_leased(&mut tx, at).await?.private {
            return Err(Error::Denied.into());
        }
        for (step, body) in steps.iter().zip(&bodies) {
            let fresh = sqlx::query("insert into bot_steps (profile, run, key, body) values ($1, $2, $3, $4) on conflict do nothing")
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .bind(&step.key)
                .bind(body)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                == 1;
            if !fresh {
                let stored: String = sqlx::query_scalar(
                    "select body from bot_steps where profile = $1 and run = $2 and key = $3",
                )
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .bind(&step.key)
                .fetch_one(&mut *tx)
                .await?;
                if stored != *body {
                    return Err(Error::Conflict.into());
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// 실패 보고. 일시 오류는 제한된 지수 백오프·jitter로 다시 줄을 서고, 영구 오류·시도 한도 초과는 dead letter다.
    /// 취소 요청이 서 있으면 재시도하지 않고 취소로 끝낸다.
    pub(crate) async fn fail_run(&self, at: &Attempt, failure: Failure) -> R<Retry> {
        let mut tx = self.pool.begin().await?;
        let parent = parent_of(&mut tx, at).await?;
        lock_parent(&mut tx, &at.profile.0, parent.as_deref()).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private {
            return Err(Error::Denied.into());
        }
        let outcome = self.settle_failure(&mut tx, &run, failure).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// 같은 대화의 이전 차례(오래된 순). 에이전트 대상 run의 입력과 결과만, 같은 프로필·바인딩·대화·스레드에서만 읽는다.
    pub(crate) async fn history_of(&self, at: &Attempt, limit: u32) -> R<Vec<Turn>> {
        // 결과를 확정하는 트랜잭션: 루트→run 잠금 뒤 claim·세대·리스·취소를 확인하고, 돌려주기 전에 보호 표지를 같은 커밋에 한다
        let (mut tx, me) = crate::memory::begin_read(self, at).await?;
        let root: String = me.get("root");
        let kind: String = me.get("conv_kind");
        // 위임 자식도 루트의 에이전트·트리거로 이력을 가른다("원래 에이전트")
        let origin =
            sqlx::query("select agent, trigger from bot_runs where profile = $1 and id = $2")
                .bind(&at.profile.0)
                .bind(&root)
                .fetch_one(&mut *tx)
                .await?;
        // 같은 대화(프로필·바인딩·종류·ID·스레드)의 끝난 에이전트 차례. 대화의 주체 경계를 지킨다:
        // - HTTP 입구: 같은 이름의 대화에도 호출 주체별로 가른다(다른 호출자의 차례를 보지 못한다).
        // - 예약: (원래 에이전트, 트리거, 주체)별로 가른다(다른 에이전트의 같은 이름 트리거와 섞이지 않는다).
        let rows = sqlx::query(
            "select u.principal, u.input, u.result,
                    exists (select 1 from bot_runs t where t.profile = u.profile and t.id = u.root and t.private_memory_read) as tainted
             from bot_runs u
             where u.profile = $1 and u.binding = $2 and u.conv_kind = $3 and u.conv_id = $4
               and u.thread is not distinct from $5 and u.parent is null and u.state = 'succeeded' and not u.private
               and u.target->>'kind' = 'agent' and u.id <> $6 and u.created_ms <= $7
               and ($3 <> 'http' or u.principal = $8)
               and ($3 <> 'scheduled' or (u.agent = $9 and u.trigger is not distinct from $10 and u.principal = $8))
             order by u.created_ms desc, u.id desc limit $11",
        )
        .bind(&at.profile.0)
        .bind(me.get::<String, _>("binding"))
        .bind(&kind)
        .bind(me.get::<String, _>("conv_id"))
        .bind(me.get::<Option<String>, _>("thread"))
        .bind(&at.run.0)
        .bind(me.get::<i64, _>("created_ms"))
        .bind(me.get::<String, _>("principal"))
        .bind(origin.get::<String, _>("agent"))
        .bind(origin.get::<Option<String>, _>("trigger"))
        .bind(i64::from(limit.min(200)))
        .fetch_all(&mut *tx)
        .await?;
        // private 기억을 읽은 실행의 답이 이 이력으로 들어온다: 보호 표지가 따라와야 한다(이 실행 트리에 같은 커밋으로 기록한다)
        if rows.iter().any(|row| row.get::<bool, _>("tainted")) {
            crate::memory::flag_private(&mut tx, at, &root).await?;
        }
        tx.commit().await?;
        let mut turns = Vec::new();
        for row in rows.into_iter().rev() {
            turns.push(Turn {
                from: Some(PrincipalId(row.get("principal"))),
                text: text_of(&row.get::<String, _>("input")),
            });
            let result: Option<String> = row.get("result");
            turns.push(Turn {
                from: None,
                text: result.map(|r| text_of(&r)).unwrap_or_default(),
            });
        }
        Ok(turns)
    }
}

impl Store {
    /// 이 핸들이 고정한 활성 집합이 **더는 현재가 아닌가**(다른 프로세스가 교체했다). `bot_active`를 FOR SHARE로 읽어 판본 교체와 같은 잠금 아래에서
    /// 판단하므로 교체가 진행 중이면 커밋 뒤의 집합으로 판정한다. 고정하지 않은 핸들·같은 집합·게시된 집합이 없음은 `false`다(구판이 아니다).
    /// 잠금 순서: bot_active → (부모) → run → 감사 head.
    async fn superseded(&self, tx: &mut Tx<'_>) -> R<bool> {
        let Some(pinned) = self.pinned_revision() else {
            return Ok(false);
        };
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active where singleton for share")
                .fetch_optional(&mut **tx)
                .await?;
        Ok(
            matches!(current.as_deref(), Some(revision) if !revision.is_empty() && revision != pinned.0),
        )
    }

    /// 잠근 run(지금 소유자)을 시도 횟수를 쓰지 않고 대기열(`ready`)로 돌리고 감사를 남긴다.
    async fn requeue_run(&self, tx: &mut Tx<'_>, run: &RunRow) -> R<()> {
        sqlx::query(
            "update bot_runs set state = 'ready', lease_until_ms = null, lease_owner = null,
                    updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .execute(&mut **tx)
        .await?;
        audit::append(
            tx,
            &run.profile,
            &Entry::new("system", "run.handover", &run.id, "stale_worker"),
        )
        .await?;
        Ok(())
    }

    /// 구판이 된 프로세스의 반납: 이 핸들이 고정한 활성 집합이 **더는 현재가 아니고**(다른 프로세스가 교체했다) 이 시도가 **지금** 리스·세대·claim의
    /// 소유자일 때만, run을 시도 횟수를 쓰지 않고 대기열(`ready`)로 돌려 새 판본의 워커가 집게 한다. 활성 집합이 바뀌어 생긴 반납은 run의
    /// 잘못이 아니므로 마지막 시도에서도 dead letter가 되지 않는다. 아니면 거절한다.
    /// - `Denied`: 고정하지 않은 핸들·같은 집합(현재 판본의 워커)·게시된 집합이 없음·민감 run·취소 요청이 선 run
    /// - `Stale`: 옛 세대·옛 claim·리스 만료·leased가 아님
    pub(crate) async fn release_stale_run(&self, at: &Attempt) -> R<()> {
        let mut tx = self.pool.begin().await?;
        if !self.superseded(&mut tx).await? {
            return Err(Error::Denied.into());
        }
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(Error::Denied.into());
        }
        self.requeue_run(&mut tx, &run).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 구판이 된 프로세스가 run을 대기열로 돌려준다(`release_stale_run`). 공개 경계라 코어 오류로 바꿔 돌려준다.
    pub async fn release_stale(&self, at: &Attempt) -> Result<(), Error> {
        self.release_stale_run(at).await.map_err(Fail::into_core)
    }

    /// 실패 보고와 구판 반납을 **한 DB 판정**으로 정한다. 한 트랜잭션에서 `bot_active`를 FOR SHARE로 읽어(교체와 같은 잠금) 이 핸들의 집합이 더는
    /// 현재가 아니고 이 시도가 지금 소유자이면 시도 횟수를 쓰지 않고 대기열로 돌리고(`Retry::After(0)`), 아니면 `fail_run`과 같은 일반 실패로 정산한다
    /// (현재 판본·고정하지 않은 핸들·취소 요청). 판정을 읽지 못하면(잠금 시간 초과 등) 아무것도 기록하지 않고 오류를 돌려준다 — 호출자는 일반
    /// 실패로 넘어가지 말고 같은 호출을 다시 한다(마지막 시도에서 구판의 반납이 dead letter가 되지 않게).
    pub(crate) async fn fail_or_release_run(&self, at: &Attempt, failure: Failure) -> R<Retry> {
        let mut tx = self.pool.begin().await?;
        let stale = self.superseded(&mut tx).await?;
        let parent = parent_of(&mut tx, at).await?;
        lock_parent(&mut tx, &at.profile.0, parent.as_deref()).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private {
            return Err(Error::Denied.into());
        }
        let outcome = if stale && !run.cancel_requested {
            self.requeue_run(&mut tx, &run).await?;
            Retry::After(Duration::ZERO)
        } else {
            self.settle_failure(&mut tx, &run, failure).await?
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// 실패 보고(`fail_run`)에 구판 반납 판정을 더한 공개 경계(`fail_or_release_run`).
    pub async fn fail_or_release(&self, at: &Attempt, failure: Failure) -> Result<Retry, Error> {
        self.fail_or_release_run(at, failure)
            .await
            .map_err(Fail::into_core)
    }
}

impl Store {
    /// 소유자 확인을 마친 run의 실패를 **같은 트랜잭션에서** 정산한다: 일시 오류는 제한된 지수 백오프·jitter로 다시 줄을 서고(시도 횟수 +1),
    /// 영구 오류·시도 한도 초과는 dead letter, 취소 요청이 서 있으면 취소다. `fail_run`과 `fail_or_release_run`이 같은 본문을 쓴다.
    async fn settle_failure(&self, tx: &mut Tx<'_>, run: &RunRow, failure: Failure) -> R<Retry> {
        Ok(match failure {
            Failure::Permanent("cancelled") | Failure::Transient(_) if run.cancel_requested => {
                end_run(tx, run, End::Cancelled("cancelled"), "system").await?;
                Retry::Dead
            }
            Failure::Permanent(code) => {
                end_run(tx, run, End::Dead(code, run.attempts), "system").await?;
                Retry::Dead
            }
            Failure::Transient(code) => {
                let attempts = run.attempts + 1;
                if attempts >= self.cfg.max_attempts as i32 {
                    end_run(tx, run, End::Dead(code, attempts), "system").await?;
                    Retry::Dead
                } else {
                    let delay: i64 = sqlx::query_scalar(
                        "update bot_runs set state = 'retry', attempts = $3, error = $4, lease_until_ms = null, lease_owner = null,
                                not_before_ms = bot_now_ms()
                                    + (least($5::float8, $6::float8 * power(2::float8, ($3 - 1)::float8)) * (0.75 + random() / 2))::bigint,
                                updated_ms = bot_now_ms()
                         where profile = $1 and id = $2
                         returning not_before_ms - bot_now_ms()",
                    )
                    .bind(&run.profile)
                    .bind(&run.id)
                    .bind(attempts)
                    .bind(code)
                    .bind(self.cfg.backoff_cap.as_millis() as f64)
                    .bind(self.cfg.backoff_base.as_millis() as f64)
                    .fetch_one(&mut **tx)
                    .await?;
                    audit::append(
                        tx,
                        &run.profile,
                        &Entry::new("system", "run.retry", &run.id, code),
                    )
                    .await?;
                    Retry::After(Duration::from_millis(delay.max(0) as u64))
                }
            }
        })
    }
}

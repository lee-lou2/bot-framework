//! 소유자 DM 승인 카드 전달 작업(`bot_card_jobs`): 점유(`claim`)·최초 전송자 확인(`begin`)·정산(`settle`)·영수증 조회.
//! 계약은 docs/reliability.md «Slack 소유자에게 승인 카드를 전달할 때».
//!
//! 채널 출처의 «명시 목적지 생성»·«전송 초안» 승인은 `approval_open`이 승인 행(pending)과 이 작업을 같은 커밋에 만든다. 작업이 `posted`가 되기 전의 카드는
//! 열리지 않은 카드다: 결정(`approval_decide`)·관리 목록·클릭 대상이 아니다. 영수증(팀·DM 채널·메시지 `ts`)을 확인한 한 커밋에서만 카드가 열리고
//! 원래 채널·스레드에는 «DM으로 승인 요청을 보냈어요» 한 줄만 간다.
//!
//! 열 수 없거나(`Unavailable`) 올라갔는지 알 수 없으면(`Held`, 120초를 넘긴 `sending`도 같다) 자동으로 다시 올리지 않고 `approval_dm_unavailable`로 닫는다:
//! 같은 커밋에서 승인을 시스템 거절로 닫고 감사를 한 번 남기며 원래 채널·스레드에 닫혔다는 안내 한 줄만 등록한다(올릴 수 없다고 확정했으면 `DM_UNAVAILABLE_NOTICE`, 불명이면
//! `DM_UNCONFIRMED_NOTICE`). 생성 승인이면 기다리던 run을 깨우고(예약 0), 전송 승인이면 이번 발생을 미발송으로 끝낸다(반복 예약의 정의는 그대로).
//! 같은 정산의 재전달은 같은 결과이고 중복이 없다.
//!
//! 잠금 순서: 활성 집합 → run → 호출 → 승인 → 작업 → 초안 → 감사 head.

mod closing;

use crate::{
    approvals::lock_approval,
    db::{Fail, R, Store, Tx},
    finish::push_reply,
    replies,
    unsent::lock_run,
};
use bot_core::{
    api::ApprovalBlock,
    cards::{CardJob, CardKey, CardOrigin, CardOutbox, CardOutcome, CardReceipt, DM_SENT_NOTICE},
    command::JsonPayload,
    id::*,
    port::{Error, Reply},
};
use closing::Closing;
use sqlx::{Row, postgres::PgRow};

/// 한 번 점유한 리스. 이 안에 `begin`을 부르지 않으면 다른 전달자가 다시 점유한다.
const LEASE_MS: i64 = 90_000;
/// 일시 실패 뒤 다시 점유하기까지의 기본 간격.
const BACKOFF_MS: i64 = 5_000;
/// 이만큼 점유하고도 올리지 못하면 DM 경로가 없는 것으로 닫는다.
const MAX_ATTEMPTS: i32 = 6;

/// 잠근 승인·작업 행과 run.
struct Locks {
    run: crate::unsent::Locked,
    call_state: Option<String>,
    approval: PgRow,
    job: PgRow,
}

/// 승인이 가리키는 run → 호출 → 승인 → 작업 순서로 잠근다. 승인이나 작업이 없으면 `Missing`.
async fn lock_attempt(tx: &mut Tx<'_>, key: &CardKey) -> R<Locks> {
    let nonce = &key.nonce.0;
    let seen = sqlx::query("select profile, run, call from bot_approvals where nonce = $1")
        .bind(nonce)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Missing)?;
    let (profile, run_id, call): (String, String, String) =
        (seen.get("profile"), seen.get("run"), seen.get("call"));
    let run = lock_run(tx, &profile, &run_id).await?;
    let call_state = sqlx::query_scalar::<_, String>(
        "select state from bot_calls where profile = $1 and run = $2 and call = $3 for update",
    )
    .bind(&profile)
    .bind(&run_id)
    .bind(&call)
    .fetch_optional(&mut **tx)
    .await?;
    let approval = lock_approval(tx, nonce).await?;
    let job = sqlx::query(
        "select profile, state, attempts, lease_until_ms, sending_ms, activation,
                team, dm, ts, origin_binding, origin_team
         from bot_card_jobs where nonce = $1 for update",
    )
    .bind(nonce)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Missing)?;
    if key.attempt == 0 || i64::from(job.get::<i32, _>("attempts")) != i64::from(key.attempt) {
        return Err(Error::Stale.into());
    }
    Ok(Locks {
        run,
        call_state,
        approval,
        job,
    })
}

fn key_of(row: &PgRow) -> CardKey {
    CardKey {
        nonce: ApprovalId(row.get("nonce")),
        attempt: row.get::<i32, _>("attempts") as u32,
    }
}

fn lease_expired(job: &PgRow, now: i64) -> bool {
    job.get::<String, _>("state") == "leased" && job.get::<i64, _>("lease_until_ms") < now
}

/// 지금 이 카드를 열 수 있는가: 승인이 대기 중이고 만료 전이며, run이 같은 세대로 승인을 기다리고 취소되지 않았다.
fn can_open(locks: &Locks, now: i64) -> bool {
    locks.approval.get::<String, _>("state") == "pending"
        && locks.approval.get::<i64, _>("expires_ms") > now
        && locks.run.state == "waiting"
        && !locks.run.row.cancel_requested
        && locks.run.generation == locks.approval.get::<i64, _>("generation")
}

async fn now_ms(tx: &mut Tx<'_>) -> R<i64> {
    Ok(sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?)
}

/// 호스트 활성 집합 판본. 비어 있으면(게시 전) `Stale`.
async fn active_set(tx: &mut Tx<'_>) -> R<String> {
    sqlx::query_scalar::<_, String>("select revision from bot_active where singleton for share")
        .fetch_optional(&mut **tx)
        .await?
        .filter(|revision| !revision.is_empty())
        .ok_or_else(|| Error::Stale.into())
}

fn job_card(card: &str) -> R<JsonPayload> {
    let value: serde_json::Value = serde_json::from_str(card).map_err(|_| Error::Invalid)?;
    let inner = value.get("approval").ok_or(Error::Invalid)?;
    Ok(JsonPayload(
        serde_json::to_string(inner).map_err(|_| Error::Invalid)?,
    ))
}

impl Store {
    /// 전달할 작업을 최대 `limit`건 점유한다(리스 90초). 점유 전에 낡은 작업을 정리한다: 카드가 더는 열 수 없는 작업은 닫고, 점유 한도를 넘긴 작업과
    /// 120초를 넘긴 `sending`은 `approval_dm_unavailable`로 닫는다(자동으로 다시 올리지 않는다). 카드가 대기 중·미만료일 때만 점유한다.
    pub(crate) async fn cards_claim(&self, limit: u32) -> R<Vec<CardJob>> {
        self.card_sweep().await?;
        let mut tx = self.begin_authorized().await?;
        let rows = sqlx::query(
            "with picked as (
                 select j.nonce from bot_card_jobs j join bot_approvals a on a.nonce = j.nonce
                 where ((j.state = 'staged' and j.not_before_ms <= bot_now_ms())
                        or (j.state = 'leased' and j.lease_until_ms < bot_now_ms()))
                   and a.state = 'pending' and a.expires_ms > bot_now_ms()
                 order by j.created_ms, j.nonce
                 limit $1 for update of j skip locked)
             update bot_card_jobs j
                set state = 'leased', lease_until_ms = bot_now_ms() + $2, attempts = j.attempts + 1, updated_ms = bot_now_ms()
               from picked, bot_approvals a
              where j.nonce = picked.nonce and a.nonce = j.nonce
              returning j.nonce, j.attempts, j.profile, j.binding, j.owner, j.generation, j.created_ms, a.card",
        )
        .bind(i64::from(limit.min(64)))
        .bind(LEASE_MS)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut rows = rows;
        rows.sort_by_key(|row| {
            (
                row.get::<i64, _>("created_ms"),
                row.get::<String, _>("nonce"),
            )
        });
        rows.iter()
            .map(|row| {
                Ok(CardJob {
                    key: key_of(row),
                    profile: ProfileId(row.get("profile")),
                    binding: BindingId(row.get("binding")),
                    owner: PrincipalId(row.get("owner")),
                    card: job_card(&row.get::<String, _>("card"))?,
                    generation: Generation(row.get::<i64, _>("generation") as u64),
                })
            })
            .collect()
    }

    /// 최초 전송자 확인: 지금 활성 집합·카드 유효·세대와 프로필 판본(`checked`, 게이트웨이가 바인딩·소유자를 확인한 판본)을 대조하고 `sending`을 기록한다.
    /// 점유한 작업에 처음 부른 쪽만 Ok이고 나머지는 `Conflict`다. 낡았으면 `Stale`이며 올리면 안 된다.
    pub(crate) async fn cards_begin(&self, key: &CardKey, checked: &Revision) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        if locks.job.get::<String, _>("state") != "leased" {
            return Err(Error::Conflict.into());
        }
        let now = now_ms(&mut tx).await?;
        if lease_expired(&locks.job, now) || !can_open(&locks, now) {
            return Err(Error::Stale.into());
        }
        let profile: String = locks.job.get("profile");
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
                .bind(&profile)
                .fetch_optional(&mut *tx)
                .await?;
        if current.as_deref() != Some(checked.0.as_str()) {
            return Err(Error::Stale.into());
        }
        let set = active_set(&mut tx).await?;
        sqlx::query(
            "update bot_card_jobs
                set state = 'sending', sending_ms = bot_now_ms(), lease_until_ms = null, activation = $2, checked_revision = $3,
                    updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&key.nonce.0)
        .bind(&set)
        .bind(&checked.0)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 결과를 정산한다. 같은 정산의 재전달은 같은 결과(`Ok`)이고 중복 기록은 없다. 이미 다른 결과로 끝난 작업은 `Conflict`, 낡은 카드는 `Stale`이다.
    pub(crate) async fn cards_settle(&self, key: &CardKey, outcome: CardOutcome) -> R<()> {
        match outcome {
            CardOutcome::Posted { team, dm, ts } => self.card_post(key, (&team, &dm, &ts)).await,
            CardOutcome::NotSent { retry_after, .. } => {
                let wait = retry_after.map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                self.card_retry(key, wait).await
            }
            CardOutcome::Unavailable => self.card_close(key, Closing::Unavailable).await,
            CardOutcome::Held => self.card_close(key, Closing::Held).await,
        }
    }

    /// 게시 확인: 한 커밋에서 다시 대조하고(sending·활성 집합·세대·카드 유효기간) 영수증을 남기고 카드를 열며 원래 채널에 한 줄을 등록한다.
    async fn card_post(&self, key: &CardKey, receipt: (&str, &str, &str)) -> R<()> {
        let (team, dm, ts) = receipt;
        if [team, dm, ts].iter().any(|v| !crate::codec::id_ok(v)) {
            return crate::codec::invalid();
        }
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        match locks.job.get::<String, _>("state").as_str() {
            "sending" => {}
            "posted" => {
                // 같은 정산의 재전달은 같은 결과, 다른 영수증이면 충돌이다
                let same = locks.job.get::<Option<String>, _>("team").as_deref() == Some(team)
                    && locks.job.get::<Option<String>, _>("dm").as_deref() == Some(dm)
                    && locks.job.get::<Option<String>, _>("ts").as_deref() == Some(ts);
                return if same {
                    Ok(())
                } else {
                    Err(Error::Conflict.into())
                };
            }
            _ => return Err(Error::Conflict.into()),
        }
        // HTTP 출처 카드는 올라간 설치(team)가 원래 요청에 고정한 전달 설치와 정확히 같아야 영수증이 된다: 현재 상태·활성 집합만으로는 같은 별칭의 팀이 바뀐 것을 알 수 없다.
        // 다르거나 고정한 team이 없으면 영수증·결정 대상·사람이 할 일이 없고, 지정된 사람의 클릭은 소비되지 않으며 승인 경로가 어긋난 것(`route_changed`)으로 닫는다
        let now = now_ms(&mut tx).await?;
        if locks
            .job
            .get::<Option<String>, _>("origin_binding")
            .is_some()
            && locks.job.get::<Option<String>, _>("origin_team").as_deref() != Some(team)
        {
            self.close_card(
                tx,
                locks,
                &key.nonce,
                Closing::Blocked(ApprovalBlock::RouteChanged),
                now,
            )
            .await?;
            return Err(Error::Stale.into());
        }
        let set = active_set(&mut tx).await?;
        let same_set =
            locks.job.get::<Option<String>, _>("activation").as_deref() == Some(set.as_str());
        if !(can_open(&locks, now) && same_set) {
            // 올린 뒤 카드가 낡았다(집합 교체·만료·세대·취소): 카드는 열지 않는다. DM 메시지만 남고 영수증이 없어 눌러도 소비되지 않는다
            sqlx::query(
                "update bot_card_jobs set state = 'closed', closed_reason = 'stale', updated_ms = bot_now_ms() where nonce = $1",
            )
            .bind(&key.nonce.0)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(Error::Stale.into());
        }
        sqlx::query(
            "update bot_card_jobs
                set state = 'posted', team = $2, dm = $3, ts = $4, posted_ms = bot_now_ms(), updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&key.nonce.0)
        .bind(team)
        .bind(dm)
        .bind(ts)
        .execute(&mut *tx)
        .await?;
        let notice = replies::answer(&JsonPayload(
            serde_json::json!({ "text": DM_SENT_NOTICE }).to_string(),
        ))?;
        push_reply(&mut tx, &locks.run.row.profile, &locks.run.row.id, &notice).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 아무것도 올라가지 않았다: 백오프 뒤 다시 점유하게 되돌린다. 점유 한도에 닿았으면 DM 경로가 없는 것으로 닫는다.
    async fn card_retry(&self, key: &CardKey, retry_after_ms: Option<i64>) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        match locks.job.get::<String, _>("state").as_str() {
            "staged" => return Ok(()),
            "sending" | "leased" => {}
            _ => return Err(Error::Conflict.into()),
        }
        let now = now_ms(&mut tx).await?;
        if lease_expired(&locks.job, now) {
            return Err(Error::Stale.into());
        }
        if locks.job.get::<i32, _>("attempts") >= MAX_ATTEMPTS {
            return self
                .close_card(tx, locks, &key.nonce, Closing::Unavailable, now)
                .await;
        }
        let wait = retry_after_ms.unwrap_or(BACKOFF_MS).clamp(0, 3_600_000);
        sqlx::query(
            "update bot_card_jobs
                set state = 'staged', not_before_ms = bot_now_ms() + $2, lease_until_ms = null, sending_ms = null,
                    activation = null, checked_revision = null, updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&key.nonce.0)
        .bind(wait)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// HTTP(OpenAI 호환) 출처 카드가 원래 요청에서 이어받은 고정 증거. 채널 출처 카드·없는 작업은 `None`.
    pub(crate) async fn cards_origin(&self, nonce: &ApprovalId) -> R<Option<CardOrigin>> {
        let row = sqlx::query(
            "select origin_binding, requester, revision, origin_team from bot_card_jobs where nonce = $1 and origin_binding is not null",
        )
        .bind(&nonce.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| CardOrigin {
            binding: BindingId(row.get("origin_binding")),
            requester: PrincipalId(row.get("requester")),
            revision: Revision(row.get("revision")),
            team: row.get("origin_team"),
        }))
    }

    /// 게시를 확인한 영수증. 확정 전이거나 소유자 DM으로 전달한 카드가 아니면 `None`.
    pub(crate) async fn cards_receipt(&self, nonce: &ApprovalId) -> R<Option<CardReceipt>> {
        let row = sqlx::query(
            "select team, dm, ts, owner, profile, binding from bot_card_jobs where nonce = $1 and state = 'posted'",
        )
        .bind(&nonce.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| CardReceipt {
            team: row.get("team"),
            dm: row.get("dm"),
            ts: row.get("ts"),
            owner: PrincipalId(row.get("owner")),
            profile: ProfileId(row.get("profile")),
            binding: BindingId(row.get("binding")),
        }))
    }
}

/// 게이트웨이가 쓰는 포트: 점유·최초 전송자 확인·정산(`bot_core::cards`).
impl CardOutbox for Store {
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>> {
        Box::pin(async move { self.cards_claim(limit).await.map_err(Fail::into_core) })
    }

    fn begin(&self, key: CardKey, checked_revision: Revision) -> Reply<'_, ()> {
        Box::pin(async move {
            self.cards_begin(&key, &checked_revision)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn settle(&self, key: CardKey, outcome: CardOutcome) -> Reply<'_, ()> {
        Box::pin(async move {
            self.cards_settle(&key, outcome)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn origin(&self, nonce: ApprovalId) -> Reply<'_, Option<CardOrigin>> {
        Box::pin(async move { self.cards_origin(&nonce).await.map_err(Fail::into_core) })
    }

    fn block(&self, key: CardKey, why: ApprovalBlock) -> Reply<'_, ()> {
        Box::pin(async move {
            self.card_close(&key, Closing::Blocked(why))
                .await
                .map_err(Fail::into_core)
        })
    }
}

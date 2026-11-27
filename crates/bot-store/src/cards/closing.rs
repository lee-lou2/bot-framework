//! 카드 청소와 실패 종결. 승인·감사·안내는 한 트랜잭션으로 쓴다.

use super::{CardKey, Locks, MAX_ATTEMPTS, can_open, key_of, lock_attempt, now_ms};
use crate::{
    approvals::wake_waiters,
    audit::{self, OwnedEntry},
    codec::to32,
    db::{Fail, R, Store, Tx},
    finish::{End, end_run_into, push_reply_into},
    replies,
    unsent::drop_draft,
};
use bot_core::{
    api::ApprovalBlock,
    cards::{APPROVAL_DM_UNAVAILABLE, DM_UNAVAILABLE_NOTICE, DM_UNCONFIRMED_NOTICE},
    command::JsonPayload,
    id::ApprovalId,
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL},
};
use sqlx::{Row, postgres::PgRow};

/// 미발송 열(`bot_deliveries.unsent`)에 저장하는 낱말.
const UNSENT_WORD: &str = "dm_unavailable";
/// `sending`이 이보다 오래되면 올라갔는지 불명으로 닫는다(자동 재게시 0).
const SENDING_MS: i64 = 120_000;

/// 닫는 이유(작업 열 `closed_reason`).
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Closing {
    Unavailable,
    Held,
    /// 올리기 전에 승인 경로가 어긋났다(HTTP 출처 전용): 판정 낱말 그대로 저장한다.
    Blocked(ApprovalBlock),
}

impl Closing {
    /// 원래 채널·스레드에 남기는 안내: 올릴 수 없다고 확정했으면 그렇게, 불명이면 확인하지 못했다고만 말한다. 승인 경로 판정은 HTTP 출처 전용이라 채널 안내가 없다.
    fn notice(self) -> Option<&'static str> {
        match self {
            Closing::Unavailable | Closing::Blocked(ApprovalBlock::ApprovalDmUnavailable) => {
                Some(DM_UNAVAILABLE_NOTICE)
            }
            Closing::Held => Some(DM_UNCONFIRMED_NOTICE),
            Closing::Blocked(_) => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Closing::Unavailable | Closing::Blocked(ApprovalBlock::ApprovalDmUnavailable) => {
                "unavailable"
            }
            Closing::Held => "held",
            Closing::Blocked(why) => why.code(),
        }
    }

    /// 감사 행의 (행동, 판정 낱말).
    fn audit(self) -> (&'static str, &'static str) {
        match self {
            Closing::Blocked(why) if why != ApprovalBlock::ApprovalDmUnavailable => {
                ("approval.blocked", why.code())
            }
            _ => ("approval.dm_unavailable", APPROVAL_DM_UNAVAILABLE),
        }
    }
}

impl Store {
    /// 낡은 작업 정리. 객관적으로 닫힌 승인은 한 문장으로 처리하고 이후 후보는 잠금 뒤 다시 판정한다.
    pub(super) async fn card_sweep(&self) -> R<()> {
        sqlx::query(
            "update bot_card_jobs j
                set state = 'closed', closed_reason = 'stale', lease_until_ms = null, updated_ms = bot_now_ms()
               from bot_approvals a
              where a.nonce = j.nonce and j.state in ('staged', 'leased')
                and (a.state <> 'pending' or a.expires_ms <= bot_now_ms())",
        )
        .execute(&self.pool)
        .await?;
        let stuck = sqlx::query(
            "select nonce, attempts from bot_card_jobs
             where (state = 'sending' and sending_ms <= bot_now_ms() - $1)
                or (state = 'staged' and attempts >= $2)
                or (state = 'leased' and attempts >= $2 and lease_until_ms < bot_now_ms())
             order by created_ms, nonce limit 8",
        )
        .bind(SENDING_MS)
        .bind(MAX_ATTEMPTS)
        .fetch_all(&self.pool)
        .await?;
        for row in stuck {
            match self.sweep_card(&key_of(&row)).await {
                Ok(()) | Err(Fail::Core(Error::Stale | Error::Conflict | Error::Missing)) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    async fn sweep_card(&self, key: &CardKey) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        let now = now_ms(&mut tx).await?;
        let Some(closing) = sweep_reason(&locks.job, now) else {
            return Ok(());
        };
        self.close_card(tx, locks, &key.nonce, closing, now).await
    }

    /// 점유한 작업의 전달 실패나 전송 전 경로 판정.
    pub(super) async fn card_close(&self, key: &CardKey, closing: Closing) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        let state: String = locks.job.get("state");
        match state.as_str() {
            "closed" => return Ok(()),
            "leased" => {}
            "sending" if !matches!(closing, Closing::Blocked(_)) => {}
            _ => return Err(Error::Conflict.into()),
        }
        let now = now_ms(&mut tx).await?;
        if super::lease_expired(&locks.job, now) {
            return Err(Error::Stale.into());
        }
        self.close_card(tx, locks, &key.nonce, closing, now).await
    }

    /// 잠금 안에서 판단한 사유로 작업과 승인을 닫는다. 생성 승인은 실행을 깨우고 전송 승인은 이번 발생을 미발송으로 끝낸다.
    pub(super) async fn close_card(
        &self,
        mut tx: Tx<'_>,
        locks: Locks,
        nonce: &ApprovalId,
        closing: Closing,
        now: i64,
    ) -> R<()> {
        if !can_open(&locks, now) {
            sqlx::query(
                "update bot_card_jobs set state = 'closed', closed_reason = 'stale', lease_until_ms = null, updated_ms = bot_now_ms() where nonce = $1",
            )
            .bind(&nonce.0)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(());
        }
        let (profile, run_id) = (locks.run.row.profile.clone(), locks.run.row.id.clone());
        // 승인은 소유자의 결정이 아니라 시스템이 거절한 것으로 닫는다
        sqlx::query(
            "update bot_approvals set state = 'denied', decided_by = 'system', decided_ms = bot_now_ms() where nonce = $1",
        )
        .bind(&nonce.0)
        .execute(&mut *tx)
        .await?;
        let tool: String = locks.approval.get("tool");
        let drafted = if tool == DELIVER_TOOL {
            sqlx::query_scalar::<_, String>(
                "select state from bot_deliveries where profile = $1 and run = $2 for update",
            )
            .bind(&profile)
            .bind(&run_id)
            .fetch_optional(&mut *tx)
            .await?
            .as_deref()
                == Some("drafted")
        } else {
            false
        };
        let mut audits = Vec::new();
        if drafted && locks.run.row.parent.is_none() {
            // 전송 승인: 이번 발생만 미발송으로 끝낸다(영수증을 한 번 만들고 초안 본문은 지운다)
            let receipt = serde_json::json!({ "status": APPROVAL_DM_UNAVAILABLE }).to_string();
            let set = locks.active.as_deref().ok_or(Error::Stale)?;
            drop_draft(
                &mut tx,
                (&profile, &run_id),
                locks.call_state.as_deref() == Some("open")
                    && locks.approval.get::<String, _>("call") == DELIVER_CALL,
                (UNSENT_WORD, &receipt, set),
                Some(&nonce.0),
            )
            .await?;
            end_run_into(
                &mut tx,
                &locks.run.row,
                End::Succeeded(&receipt),
                "system",
                &mut audits,
            )
            .await?;
        } else {
            // 생성 승인(과 그 밖): 기다리던 run을 깨운다. 호출은 거절된 승인을 만나 `Denied`로 끝난다
            wake_waiters(&mut tx, &profile, &run_id).await?;
        }
        sqlx::query(
            "update bot_card_jobs
                set state = 'closed', closed_reason = $2, lease_until_ms = null, updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&nonce.0)
        .bind(closing.word())
        .execute(&mut *tx)
        .await?;
        // 요청자는 요청이 닫혔다는 사실을 알아야 한다: 원래 채널·스레드에 한 줄만(초안·수신자·해시·승인 식별자 없음). 닫는 커밋이 한 번이라 재전달에도 한 번이다.
        // HTTP 출처 실행에는 원래 채널이 없어 쌓이는 줄이 없다(`push_reply_into`는 직접·채널 대화에만 쌓는다)
        if let Some(text) = closing.notice() {
            let notice = replies::answer(&JsonPayload(
                serde_json::json!({ "text": text }).to_string(),
            ))?;
            push_reply_into(&mut tx, &profile, &run_id, &notice, &mut audits).await?;
        }
        let (action, verdict) = closing.audit();
        let mut entry = OwnedEntry::new(&profile, "system", action, &nonce.0, verdict);
        entry.args = Some(to32(locks.approval.get("args_sha256"))?);
        audits.push(entry);
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}

fn sweep_reason(job: &PgRow, now: i64) -> Option<Closing> {
    let spent = job.get::<i32, _>("attempts") >= MAX_ATTEMPTS;
    match job.get::<String, _>("state").as_str() {
        "sending" if job.get::<i64, _>("sending_ms") <= now - SENDING_MS => Some(Closing::Held),
        "staged" if spent => Some(Closing::Unavailable),
        "leased" if spent && super::lease_expired(job, now) => Some(Closing::Unavailable),
        _ => None,
    }
}

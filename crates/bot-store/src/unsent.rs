//! 보내지 않고 끝내는 전송 초안(미발송 종결). 동결한 초안이 수신자에게 가지 못하는 두 경우를 한 커밋으로 마무리한다.
//! - `Unrouted`: 현재 운영자 등록부가 저장된 수신자를 거절하거나 다른 곳을 가리킨다. 원래 대화에 고정 안내 한 건만 남기고 초안·승인을 닫는다.
//! - `Rejected`: 소유자 본인이 전송을 거절하거나 소비 전 승인을 철회했다(저장된 본인의 결정이 있을 때만). 안내는 없다.
//!
//! 종결은 어느 쪽이든 초안 본문을 지우고(지문·집합 판본만 남긴다), 열린 승인을 무효로 하고, run을 미발송 영수증으로 성공 처리한다.
//! 이미 등록된(승인 소비·outbox) 답은 되돌릴 수 없다(`Conflict`). 같은 종결의 재전달은 같은 영수증이다.
//!
//! 펜스: `Claim`은 지금의 유효한 claim(리스·세대·claim_epoch), `Approval`은 이 발생의 살아 있는 카드와 인증된 소유자다.
//! 잠금 순서: 활성 판본 → run → 호출 → 승인 → 초안 → 감사 head.

use crate::{
    audit::{self, Entry},
    codec::{kind_from, to32},
    db::{R, Store, Tx},
    delivery::peek,
    finish::{End, RunRow, end_run, lock_leased, parent_of, push_reply},
    replies,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    port::Error,
    schedule::{
        DELIVER_CALL, DELIVER_TOOL, DeliveryFence, DeliveryTarget, UNROUTED_NOTICE, UnsentReason,
    },
    state::Conversation,
};
use sqlx::{Row, postgres::PgRow};

fn target_of(row: &PgRow) -> R<DeliveryTarget> {
    Ok(DeliveryTarget {
        binding: BindingId(row.get("dest_binding")),
        conversation: Conversation {
            kind: kind_from(&row.get::<String, _>("dest_kind"))?,
            id: row.get("dest_id"),
            thread: row.get("dest_thread"),
        },
    })
}

/// 저장 열의 사유 낱말(영수증의 `delivery_…` 코드와 따로, 열 검사가 이 둘만 받는다).
fn word(reason: UnsentReason) -> &'static str {
    match reason {
        UnsentReason::Rejected => "rejected",
        UnsentReason::Unrouted => "unrouted",
    }
}

fn same_target(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

/// 잠근 run과 그 소유 정보(리스·세대·claim과 소유자).
pub(crate) struct Locked {
    pub(crate) row: RunRow,
    pub(crate) state: String,
    pub(crate) generation: i64,
    claim_epoch: i64,
    lease_live: bool,
    owner: String,
}

impl Locked {
    /// 이 시도가 이 run의 마지막 claim(세대·claim_epoch가 같다)인가.
    fn same_claim(&self, at: &Attempt) -> bool {
        self.generation == at.generation.0 as i64
            && at.claim.is_some_and(|c| self.claim_epoch == c.0 as i64)
    }
}

pub(crate) async fn lock_run(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Locked> {
    let row = sqlx::query(
        "select profile, id, parent, parent_step, attempts, cancel_requested, private, state, generation, claim_epoch,
                lease_until_ms, principal
         from bot_runs where profile = $1 and id = $2 for update",
    )
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Missing)?;
    // 시각은 잠금을 얻은 다음 문장에서 읽는다
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?;
    let lease: Option<i64> = row.get("lease_until_ms");
    Ok(Locked {
        state: row.get("state"),
        generation: row.get("generation"),
        claim_epoch: row.get("claim_epoch"),
        lease_live: lease.is_some_and(|until| until >= now),
        owner: row.get("principal"),
        row: RunRow {
            profile: row.get("profile"),
            id: row.get("id"),
            parent: row.get("parent"),
            parent_step: row.get("parent_step"),
            attempts: row.get("attempts"),
            cancel_requested: row.get("cancel_requested"),
            private: row.get("private"),
        },
    })
}

/// 잠근 run·호출·초안 위에서 동결한 초안을 보내지 않고 닫는 공통 꼬리: 열린 전송 호출은 영수증으로 끝내고(이미 거절·완료된 호출은 그대로 둔다)
/// 초안은 본문을 지운 `dropped`로 둔다. `ended`는 (저장 열의 사유 낱말, 영수증, 종결한 때의 호스트 활성 집합 판본)이다.
pub(crate) async fn drop_draft(
    tx: &mut Tx<'_>,
    run: (&str, &str),
    call_open: bool,
    ended: (&str, &str, &str),
    nonce: Option<&str>,
) -> R<()> {
    let ((profile, run_id), (word, receipt, current)) = (run, ended);
    if call_open {
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(profile)
        .bind(run_id)
        .bind(DELIVER_CALL)
        .bind(receipt)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "update bot_deliveries
         set state = 'dropped', unsent = $3, unsent_receipt = $4, unsent_activation = $5, unsent_nonce = $6,
             unsent_ms = bot_now_ms(), result = null, answers = null, updated_ms = bot_now_ms()
         where profile = $1 and run = $2",
    )
    .bind(profile)
    .bind(run_id)
    .bind(word)
    .bind(receipt)
    .bind(current)
    .bind(nonce)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// 이 claim이 가진 run의 동결한 수신자만 돌려준다(본문 없음). 초안을 옛 활성 집합에서 동결했어도 읽는다: 수신자가 철회됐는지 가리려는 용도다.
    /// 초안이 없거나(일반 실행·동결 전) 이미 닫혔으면 `None`, 현재 claim이 아니면 `Stale`이다.
    pub(crate) async fn delivery_target(&self, at: &Attempt) -> R<Option<DeliveryTarget>> {
        let mut tx = self.begin_authorized().await?;
        if parent_of(&mut tx, at).await?.is_some() {
            tx.rollback().await?;
            return Ok(None); // 위임한 자식의 답은 부모만 보낸다
        }
        lock_leased(&mut tx, at).await?;
        let row = peek(&mut tx, &at.profile.0, &at.run.0).await?;
        tx.rollback().await?;
        match row {
            Some(row) if row.get::<String, _>("state") == "drafted" => Ok(Some(target_of(&row)?)),
            _ => Ok(None),
        }
    }

    /// 이 claim의 등록 영수증, 없으면 미발송 영수증(마지막 claim의 성공한 run에만).
    pub(crate) async fn delivery_any_receipt(&self, at: &Attempt) -> R<Option<JsonPayload>> {
        if let Some(receipt) = self.delivery_receipt(at).await? {
            return Ok(Some(receipt));
        }
        let row = sqlx::query(
            "select d.unsent_receipt, d.unsent_activation, r.generation, r.claim_epoch,
                    (select revision from bot_active where singleton) as current
             from bot_deliveries d
             join bot_runs r on r.profile = d.profile and r.id = d.run
             where d.profile = $1 and d.run = $2 and d.state = 'dropped' and d.unsent is not null and r.state = 'succeeded'",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let current: Option<String> = row.get("current");
        let ended_in: Option<String> = row.get("unsent_activation");
        let same_claim = row.get::<i64, _>("generation") == at.generation.0 as i64
            && at
                .claim
                .is_some_and(|c| row.get::<i64, _>("claim_epoch") == c.0 as i64);
        let pinned = self
            .activation
            .as_ref()
            .is_none_or(|expected| current.as_deref() == Some(expected.0.as_str()));
        if !same_claim || !pinned || current.is_none() || current != ended_in {
            return Err(Error::Stale.into());
        }
        Ok(row
            .get::<Option<String>, _>("unsent_receipt")
            .map(JsonPayload))
    }

    /// 동결한 초안을 보내지 않고 끝낸다(위 문서). 돌려주는 값은 미발송 영수증이다.
    pub(crate) async fn delivery_finish_unsent(
        &self,
        fence: DeliveryFence,
        target: DeliveryTarget,
        reason: UnsentReason,
    ) -> R<JsonPayload> {
        let mut tx = self.begin_authorized().await?;
        // 어느 run인가: Claim은 시도가, Approval은 카드가 알려 준다
        let (profile, run_id) = match &fence {
            DeliveryFence::Claim(at) => (at.profile.0.clone(), at.run.0.clone()),
            DeliveryFence::Approval { nonce, .. } => {
                let row = sqlx::query("select profile, run from bot_approvals where nonce = $1")
                    .bind(&nonce.0)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(Error::Missing)?;
                (row.get("profile"), row.get("run"))
            }
        };
        let run = lock_run(&mut tx, &profile, &run_id).await?;
        if run.row.parent.is_some() {
            return Err(Error::Conflict.into()); // 위임한 자식은 회신하지 않는다
        }
        if run.row.private {
            return Err(Error::Denied.into());
        }
        // 호출 → 승인 순서로 잠근다(호출은 아직 없을 수 있다)
        let call_state: Option<String> = sqlx::query_scalar(
            "select state from bot_calls where profile = $1 and run = $2 and call = $3 and tool = $4 for update",
        )
        .bind(&profile)
        .bind(&run_id)
        .bind(DELIVER_CALL)
        .bind(DELIVER_TOOL)
        .fetch_optional(&mut *tx)
        .await?;
        let card = match &fence {
            DeliveryFence::Approval { nonce, actor } => {
                let card = sqlx::query(
                    "select profile, run, call, approver, state, expires_ms, bot_now_ms() as now
                     from bot_approvals where nonce = $1 for update",
                )
                .bind(&nonce.0)
                .fetch_one(&mut *tx)
                .await?;
                // 이 발생의 전송 카드이고 인증된 소유자 본인만 닫는다
                if card.get::<String, _>("call") != DELIVER_CALL
                    || card.get::<String, _>("approver") != actor.0
                    || actor.0 != run.owner
                {
                    return Err(Error::Denied.into());
                }
                Some((nonce.0.clone(), card))
            }
            DeliveryFence::Claim(_) => None,
        };
        let delivery = sqlx::query(&format!(
            "select {} from bot_deliveries where profile = $1 and run = $2 for update",
            crate::cols::DELIVERIES
        ))
        .bind(&profile)
        .bind(&run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        // 보관한 수신자와 정확히 같아야 한다(다른 수신자에 대한 판정으로 이 발생을 닫지 못한다)
        if !same_target(&target_of(&delivery)?, &target) {
            return Err(Error::Conflict.into());
        }
        // 소유자 본인의 저장된 거절·철회 결정이 있으면 요청한 이유와 무관하게 «거절»이다: 기록된 결정이 그 뒤의 등록부 상태보다 먼저다.
        // run 잠금을 쥔 채 읽는다: 같은 run 잠금을 먼저 잡는 소유자의 결정(`approval_decide`)과 직렬화돼, 결정이 먼저 커밋됐다면 여기서 보이고 아니라면
        // 이 종결이 카드를 닫은 뒤라 그 결정은 받아들여지지 않는다.
        let decided = sqlx::query_scalar::<_, i32>(
            "select 1 from bot_approvals
             where profile = $1 and run = $2 and call = $3 and state in ('denied', 'revoked') and decided_by = $4 limit 1",
        )
        .bind(&profile)
        .bind(&run_id)
        .bind(DELIVER_CALL)
        .bind(&run.owner)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        let reason = if decided {
            UnsentReason::Rejected
        } else {
            reason
        };
        let state: String = delivery.get("state");
        let ended: Option<String> = delivery.get("unsent");
        if state == "dropped" && ended.as_deref() == Some(word(reason)) {
            // 같은 종결의 재전달: 지금 이 펜스가 그 종결의 것일 때만 같은 영수증을 돌려준다
            let fenced = match &fence {
                DeliveryFence::Claim(at) => run.state == "succeeded" && run.same_claim(at),
                DeliveryFence::Approval { nonce, .. } => {
                    delivery.get::<Option<String>, _>("unsent_nonce").as_deref() == Some(&nonce.0)
                }
            };
            if !fenced {
                return Err(Error::Stale.into());
            }
            let receipt: String = delivery.get("unsent_receipt");
            tx.rollback().await?;
            return Ok(JsonPayload(receipt));
        }
        if state != "drafted" {
            return Err(Error::Conflict.into()); // 동결 전이거나 이미 등록(발송)·다른 사유로 닫혔다
        }
        // 펜스: 지금의 유효한 claim 또는 살아 있는 카드
        let nonce = match (&fence, &card) {
            (DeliveryFence::Claim(at), _) => {
                if run.state != "leased" || !run.lease_live || !run.same_claim(at) {
                    return Err(Error::Stale.into());
                }
                None
            }
            (DeliveryFence::Approval { .. }, Some((nonce, card))) => {
                if card.get::<String, _>("state") != "pending"
                    || card.get::<i64, _>("expires_ms") <= card.get::<i64, _>("now")
                    || run.state != "waiting"
                {
                    return Err(Error::Stale.into());
                }
                Some(nonce.clone())
            }
            _ => return Err(Error::Denied.into()),
        };
        if reason == UnsentReason::Rejected && !decided {
            // «거절»은 소유자 본인의 거절·철회 결정이 저장돼 있을 때만 쓴다
            return Err(Error::Denied.into());
        }
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active where singleton")
                .fetch_optional(&mut *tx)
                .await?;
        let current = current.filter(|r| !r.is_empty()).ok_or(Error::Stale)?;
        let sealed = to32(
            delivery
                .get::<Option<Vec<u8>>, _>("sha256")
                .ok_or(Error::Invalid)?,
        )?;
        let receipt = reason.receipt().0;

        if reason == UnsentReason::Unrouted {
            // 원래 대화에는 고정 안내 한 건만 간다(초안·수신자·지시문은 넣지 않는다)
            let notice = replies::answer(&JsonPayload(
                serde_json::json!({ "text": UNROUTED_NOTICE }).to_string(),
            ))?;
            push_reply(&mut tx, &profile, &run_id, &notice).await?;
        }
        drop_draft(
            &mut tx,
            (&profile, &run_id),
            call_state.as_deref() == Some("open"),
            (word(reason), &receipt, &current),
            nonce.as_deref(),
        )
        .await?;
        // 열린 승인은 이 종결로 무효가 된다(`end_run`이 닫는다)
        let actor = if card.is_some() || reason == UnsentReason::Rejected {
            run.owner.clone()
        } else {
            "system".to_owned()
        };
        end_run(&mut tx, &run.row, End::Succeeded(&receipt), &actor).await?;
        let mut entry = Entry::new(&actor, "delivery.unsent", &run_id, reason.code());
        entry.args = Some(sealed);
        audit::append(&mut tx, &profile, &entry).await?;
        tx.commit().await?;
        Ok(JsonPayload(receipt))
    }
}

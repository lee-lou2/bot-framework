//! 회신 발송 예약(outbox). 게이트웨이가 `claim`으로 점유하고 게시 자격을 검사한 뒤 `begin`이 커밋돼야만 보낸다.
//! 발송 세대는 run 세대와 별개이며 claim마다 오른다. begin 전에 리스가 끝나면 다시 점유하고,
//! begin 뒤(sending)에 응답을 잃으면 held로 둔다. settle은 영수증·감사와 한 커밋이다. 같은 발송 키(profile, run, seq)는 끝까지 유지된다.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store},
};
use bot_core::{
    command::JsonPayload,
    id::*,
    intake::{DeliveryKey, DeliveryOutcome, NotSentReason, Outgoing},
    port::Error,
    state::{Conversation, ConversationKind},
};
use sqlx::Row;

const MAX_BATCH: u32 = 100;

fn receipt_ok(receipt: &str) -> bool {
    id_ok(receipt) && receipt.len() <= 512
}

fn bind_key<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    key: &'q DeliveryKey,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    query
        .bind(&key.profile.0)
        .bind(&key.run.0)
        .bind(i32::try_from(key.seq).unwrap_or(i32::MAX))
}

impl Store {
    /// 발송 예약을 리스로 점유한다. 리스가 끝난 sending은 불명이므로 먼저 held로 옮기고 자동 claim 대상에서 뺀다.
    pub(crate) async fn outbox_claim(
        &self,
        kinds: &[ConversationKind],
        limit: u32,
    ) -> R<Vec<Outgoing>> {
        if kinds.is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        let kinds: Vec<&str> = kinds.iter().map(kind_str).collect();
        let mut tx = self.begin_authorized().await?;
        let lost = sqlx::query(
            "update bot_outbox set state = 'held', reason = 'owner_lost', updated_ms = bot_now_ms()
             where state = 'sending' and lease_until_ms < (select bot_now_ms()) returning profile, call",
        )
        .fetch_all(&mut *tx)
        .await?;
        for row in &lost {
            let (profile, call): (String, String) = (row.get("profile"), row.get("call"));
            audit::append(
                &mut tx,
                &profile,
                &Entry::new("system", "outbox.held", &call, "owner_lost"),
            )
            .await?;
        }
        // 같은 run의 앞선 회신이 아직 나가는 중이면 뒤의 것을 잡지 않는다(순서 유지)
        let rows = sqlx::query(
            "select o.profile, o.run, o.seq from bot_outbox o
             where o.state = 'pending' and o.conv_kind = any($1)
               and o.not_before_ms <= (select bot_now_ms())
               and (o.lease_until_ms is null or o.lease_until_ms < (select bot_now_ms()))
               and not exists (select 1 from bot_outbox p
                               where p.profile = o.profile and p.run = o.run and p.seq < o.seq
                                 and p.state in ('pending', 'sending'))
             order by o.created_ms, o.profile, o.run, o.seq
             limit $2 for update of o skip locked",
        )
        .bind(&kinds)
        .bind(i64::from(limit.min(MAX_BATCH)))
        .fetch_all(&mut *tx)
        .await?;
        let mut out = Vec::new();
        for row in rows {
            let (profile, run, seq): (String, String, i32) =
                (row.get("profile"), row.get("run"), row.get("seq"));
            let claimed = sqlx::query(
                "update bot_outbox set generation = generation + 1, lease_until_ms = bot_now_ms() + $4, updated_ms = bot_now_ms()
                 where profile = $1 and run = $2 and seq = $3
                 returning generation, call, binding, principal, conv_kind, conv_id, thread, body",
            )
            .bind(&profile)
            .bind(&run)
            .bind(seq)
            .bind(self.cfg.delivery_lease.as_millis() as i64)
            .fetch_one(&mut *tx)
            .await?;
            out.push(Outgoing {
                key: DeliveryKey {
                    profile: ProfileId(profile),
                    run: RunId(run),
                    seq: seq as u32,
                    generation: Generation(claimed.get::<i64, _>("generation") as u64),
                },
                call: CallId(claimed.get("call")),
                binding: BindingId(claimed.get("binding")),
                principal: PrincipalId(claimed.get("principal")),
                destination: Conversation {
                    kind: kind_from(&claimed.get::<String, _>("conv_kind"))?,
                    id: claimed.get("conv_id"),
                    thread: claimed.get("thread"),
                },
                body: JsonPayload(claimed.get("body")),
            });
        }
        tx.commit().await?;
        Ok(out)
    }

    /// 현재 리스·현재 활성 판본과의 일치를 확인하고 sending을 기록한다. 최초 호출만 Ok다(재호출은 `Conflict`).
    pub(crate) async fn outbox_begin(&self, key: &DeliveryKey, checked: &Revision) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        // 게시 자격 검사에 쓴 판본이 지금도 활성인지 교체와 직렬화해 대조한다(철회·교체 중에는 기다렸다가 Stale)
        crate::active::guard_active(&mut tx, &key.profile.0, checked).await?;
        let row = bind_key(
            sqlx::query("select state, generation, lease_until_ms, call from bot_outbox where profile = $1 and run = $2 and seq = $3 for update"),
            key,
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let (state, generation): (String, i64) = (row.get("state"), row.get("generation"));
        if generation as u64 != key.generation.0 {
            return Err(Error::Stale.into());
        }
        if state == "sending" {
            return Err(Error::Conflict.into());
        }
        let leased = row
            .get::<Option<i64>, _>("lease_until_ms")
            .is_some_and(|until| until >= now);
        if state != "pending" || !leased {
            return Err(Error::Stale.into());
        }
        bind_key(
            sqlx::query("update bot_outbox set state = 'sending', updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
            key,
        )
        .execute(&mut *tx)
        .await?;
        let call: String = row.get("call");
        audit::append(
            &mut tx,
            &key.profile.0,
            &Entry::new("gateway", "outbox.begin", &call, &generation.to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 결과를 기록한다. 현재 발송 세대의 sending에서만 받는다(옛 세대·이미 정산된 건은 `Stale`).
    /// 이미 같은 결과로 정산됐다면 멱등으로 Ok다. 보류(held) 뒤에 늦게 온 확정 영수증은 받아 확정으로 바꾼다.
    pub(crate) async fn outbox_settle(&self, key: &DeliveryKey, outcome: DeliveryOutcome) -> R<()> {
        if let DeliveryOutcome::Confirmed { receipt } = &outcome
            && !receipt_ok(receipt)
        {
            return invalid();
        }
        let mut tx = self.pool.begin().await?;
        let row = bind_key(
            sqlx::query("select state, generation, attempts, call, receipt from bot_outbox where profile = $1 and run = $2 and seq = $3 for update"),
            key,
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let (state, call): (String, String) = (row.get("state"), row.get("call"));
        if row.get::<i64, _>("generation") as u64 != key.generation.0 {
            return Err(Error::Stale.into());
        }
        let (target, verdict) = match &outcome {
            DeliveryOutcome::Confirmed { .. } => ("confirmed", "confirmed".to_owned()),
            DeliveryOutcome::Rejected => ("rejected", "rejected".to_owned()),
            DeliveryOutcome::Held => ("held", "unknown_outcome".to_owned()),
            DeliveryOutcome::NotSent { reason, .. } => (
                "pending",
                format!(
                    "not_sent:{}",
                    match reason {
                        NotSentReason::BeforeWrite => "before_write",
                        NotSentReason::RateLimited => "rate_limited",
                    }
                ),
            ),
        };
        let late_confirm = matches!(outcome, DeliveryOutcome::Confirmed { .. }) && state == "held";
        if state == target && target != "pending" {
            let same_receipt = match &outcome {
                DeliveryOutcome::Confirmed { receipt } => {
                    row.get::<Option<String>, _>("receipt").as_deref() == Some(receipt)
                }
                _ => true,
            };
            tx.rollback().await?;
            return if same_receipt {
                Ok(())
            } else {
                Err(Error::Conflict.into())
            };
        }
        if state != "sending" && !late_confirm {
            return Err(Error::Stale.into());
        }
        let attempts: i32 = row.get("attempts");
        let mut verdict = verdict;
        match outcome {
            DeliveryOutcome::Confirmed { receipt } => {
                bind_key(
                    sqlx::query("update bot_outbox set state = 'confirmed', receipt = $4, reason = null, lease_until_ms = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
                    key,
                )
                .bind(receipt)
                .execute(&mut *tx)
                .await?;
            }
            DeliveryOutcome::Rejected | DeliveryOutcome::Held => {
                let reason = (target == "held").then_some("unknown_outcome");
                bind_key(
                    sqlx::query("update bot_outbox set state = $4, reason = $5, lease_until_ms = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
                    key,
                )
                .bind(target)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
            }
            DeliveryOutcome::NotSent { retry_after, .. } => {
                let attempts = attempts + 1;
                if attempts >= self.cfg.max_attempts as i32 {
                    // 재시도를 다 썼다: 운영자에게 최종 실패로 보이도록 보류한다
                    verdict = "retries_exhausted".to_owned();
                    bind_key(
                        sqlx::query("update bot_outbox set state = 'held', reason = 'retries_exhausted', attempts = $4, lease_until_ms = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
                        key,
                    )
                    .bind(attempts)
                    .execute(&mut *tx)
                    .await?;
                } else {
                    // 다음 claim은 백오프와 수신처가 준 대기 시간 중 더 긴 쪽 이후에 새 세대로 잡는다
                    let floor = retry_after.map_or(0, |d| d.as_millis() as i64);
                    bind_key(
                        sqlx::query(
                            "update bot_outbox set state = 'pending', attempts = $4, lease_until_ms = null, updated_ms = bot_now_ms(),
                                    not_before_ms = bot_now_ms() + greatest($7::bigint,
                                        (least($5::float8, $6::float8 * power(2::float8, ($4 - 1)::float8)) * (0.75 + random() / 2))::bigint)
                             where profile = $1 and run = $2 and seq = $3",
                        ),
                        key,
                    )
                    .bind(attempts)
                    .bind(self.cfg.backoff_cap.as_millis() as f64)
                    .bind(self.cfg.backoff_base.as_millis() as f64)
                    .bind(floor)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        audit::append(
            &mut tx,
            &key.profile.0,
            &Entry::new("gateway", "outbox.settle", &call, &verdict),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

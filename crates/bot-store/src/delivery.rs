//! 명시 목적지(다른 수신자) 예약 발화의 회신: 최종 결과·답을 한 번 동결하는 초안(`draft`)과, 소유자가 승인한 초안만 목적지 outbox로 쌓고 run을 마치는 `enqueue`.
//! - 초안은 run마다 하나(`bot_deliveries`)이고 목적지는 발화를 접수한 커밋에서 예약 정의로부터 고정한다(`user_due`).
//! - 동결 입구에서 원문 답으로 승인 문서(`DeliveryDraft::approval_json`)를 만들 수 있는지 먼저 검사한다(중복 키가 JSONB로 사라지기 전). 답은 JSON으로 다시 쓰지
//!   않은 원문(text)으로 저장하고, `begin`·승인 열기·`enqueue`가 저장된 바이트로 같은 문서와 SHA-256을 다시 계산한다.
//! - 승인한 초안만 보낸다: 호출에 봉인된 지문·승인 소비·호스트 활성 집합·목적지 outbox·run 완료가 한 커밋이다.
//!
//! 잠금 순서: 활성 판본 → 루트 run(공유) → run → 호출 → 승인 → 초안 → 감사 head.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::{End, end_run, final_text, has_attachments, lock_leased, parent_of},
    replies,
    user_schedule::{Claimed, audit_done, claim, finish_call},
};
use bot_core::{
    command::{Finish, JsonPayload},
    id::*,
    intake::PushTransportId,
    ledger::StartCall,
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL, DeliveryDraft, DeliveryTarget, quiet_completion},
    state::{Conversation, ConversationKind},
};
use sqlx::{Row, postgres::PgRow};

/// 한 발화의 답은 최대 이만큼이다(일반 회신과 같다).
const MAX_ANSWERS: usize = 16;

/// 저장된 열로 승인 문서의 입력을 되살린다(동결 전이면 `Invalid`).
pub(crate) fn draft_of(row: &PgRow) -> R<DeliveryDraft> {
    Ok(DeliveryDraft {
        activation: Revision(
            row.get::<Option<String>, _>("activation")
                .ok_or(Error::Invalid)?,
        ),
        target: DeliveryTarget {
            binding: BindingId(row.get("dest_binding")),
            conversation: Conversation {
                kind: kind_from(&row.get::<String, _>("dest_kind"))?,
                id: row.get("dest_id"),
                thread: row.get("dest_thread"),
            },
        },
        answers: row
            .get::<Option<Vec<String>>, _>("answers")
            .ok_or(Error::Invalid)?
            .into_iter()
            .map(JsonPayload)
            .collect(),
    })
}

/// 저장된 바이트에서 승인 지문을 다시 계산한다. 저장된 지문과 같아야 하고, 아니면 초안이 바뀐 것이다(`Conflict`).
fn recomputed(row: &PgRow) -> R<[u8; 32]> {
    let hash = draft_of(row)?.sha256()?;
    if row.get::<Option<Vec<u8>>, _>("sha256").as_deref() != Some(hash.as_slice()) {
        return Err(Error::Conflict.into());
    }
    Ok(hash)
}

/// 이 run의 동결한 초안 행(잠그지 않는다). 없으면 `None`이다.
pub(crate) async fn peek(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Option<PgRow>> {
    Ok(sqlx::query(&format!(
        "select {} from bot_deliveries where profile = $1 and run = $2",
        crate::cols::DELIVERIES
    ))
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn lock(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Option<PgRow>> {
    Ok(sqlx::query(&format!(
        "select {} from bot_deliveries where profile = $1 and run = $2 for update",
        crate::cols::DELIVERIES
    ))
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?)
}

/// 지금 게시된 호스트 활성 집합의 판본. 게시한 적이 없으면 `Stale`이다. `for share`로 읽어 집합 교체(`for update`)와 직렬화한다: 읽은 값은 이 커밋이 끝날 때까지
/// 바뀌지 않는다(호출자는 대개 이미 같은 잠금을 쥐고 있다 — 잠금 순서가 활성 집합이 먼저다).
async fn current_activation(tx: &mut Tx<'_>) -> R<String> {
    let current: Option<String> =
        sqlx::query_scalar("select revision from bot_active where singleton for share")
            .fetch_optional(&mut **tx)
            .await?;
    current.filter(|r| !r.is_empty()).ok_or(Error::Stale.into())
}

/// `begin`이 DELIVER_TOOL 호출을 기록할 때 쓰는 확인: 동결한 초안이 있고 그 승인 지문과 같은 호출만 기록한다.
pub(crate) async fn check_begin(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    args: &[u8; 32],
) -> R<()> {
    let Some(row) = peek(tx, profile, run).await? else {
        return Err(Error::Conflict.into());
    };
    if row.get::<String, _>("state") != "drafted" || recomputed(&row)?.as_slice() != args.as_slice()
    {
        return Err(Error::Conflict.into());
    }
    Ok(())
}

/// 승인을 열 때의 확인: 동결한 초안의 승인 문서가 카드에 그대로 보이고(보이는 것 = 보내는 것), 승인 지문이 같으며, 승인자는 예약을 만든 소유자 본인이고,
/// 초안을 동결한 활성 집합이 지금도 현재 집합이다.
pub(crate) async fn check_open(
    tx: &mut Tx<'_>,
    approval: &bot_core::state::Approval,
    card: &str,
) -> R<()> {
    let at = &approval.attempt;
    if approval.call.0 != DELIVER_CALL
        || approval.risk != bot_core::state::Risk::Irreversible
        || approval.requester != approval.approver
    {
        return Err(Error::Denied.into());
    }
    let owner: String =
        sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .fetch_one(&mut **tx)
            .await?;
    if owner != approval.approver.0 {
        return Err(Error::Denied.into());
    }
    let row = peek(tx, &at.profile.0, &at.run.0)
        .await?
        .ok_or(Error::Conflict)?;
    if row.get::<String, _>("state") != "drafted"
        || recomputed(&row)? != approval.args_sha256
        || shown_draft(card).as_deref() != Some(draft_of(&row)?.approval_json()?.as_str())
    {
        return Err(Error::Conflict.into());
    }
    // 초안을 동결한 호스트 활성 집합이 지금도 현재 집합이어야 카드를 연다(수신자 매핑이 달라졌을 수 있는 옛 집합의 초안은 카드로 보이지 않는다)
    if current_activation(tx).await? != draft_of(&row)?.activation.0 {
        return Err(Error::Conflict.into());
    }
    Ok(())
}

/// 카드의 `전송 초안` 한 행(첫 줄 `전송 초안: `, 이어지는 줄은 공백 두 칸으로 시작)을 이어 붙인 문서. 정확히 하나가 아니면 `None`이다.
pub(crate) fn shown_draft(card: &str) -> Option<String> {
    shown_row(card, bot_core::schedule::DELIVERY_CARD_LABEL)
}

/// 수신자를 지정한 예약 생성 카드인가: 카드의 `인자` 행(브로커가 봉인한 정규 인자 JSON)의 최상위에 `destination`이 있다. 카드는 브로커가 연 승인에 저장된 값이고
/// 그 안의 지시문 같은 값은 JSON 문자열 안에 있으므로 최상위 키로 읽히지 않는다. 일반 생성(원래 대화)은 **올바른 객체에 `destination`이 없을 때만**이다:
/// `인자` 행이 없거나 둘이거나 JSON 객체가 아니면 어느 쪽인지 알 수 없으므로 `Invalid`다(일반 생성으로 보지 않는다).
pub(crate) fn create_names_destination(card: &str) -> R<bool> {
    let args = shown_row(card, "인자").ok_or(Error::Invalid)?;
    let args: serde_json::Value = serde_json::from_str(&args).map_err(|_| Error::Invalid)?;
    Ok(args
        .as_object()
        .ok_or(Error::Invalid)?
        .contains_key("destination"))
}

/// 예약 생성 카드가 보인 `인자` 행의 원문(브로커가 봉인한 정규 인자 JSON 그대로)과 그 UTF-8 바이트의 SHA-256. 행이 정확히 하나가 아니면 `Invalid`다.
/// 봉인(`ScheduleSpec::seal_create`)은 카드에 보일 문서와 지문을 한 값에서 만든다: 카드에 보인 것이 승인·호출 지문과 같은지 이 해시로 대조한다.
pub(crate) fn create_shown_args_sha256(card: &str) -> R<[u8; 32]> {
    let shown = shown_row(card, "인자").ok_or(Error::Invalid)?;
    Ok(crate::codec::sha256_text(&shown))
}

/// 카드의 `label` 한 행(첫 줄 `라벨: `, 이어지는 줄은 공백 두 칸으로 시작)을 이어 붙인 값. 정확히 하나가 아니면 `None`이다.
fn shown_row(card: &str, label: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(card).ok()?;
    let lines = value.get("approval")?.get("lines")?.as_array()?;
    let label = format!("{label}: ");
    let mut shown: Vec<String> = Vec::new();
    for line in lines.iter().filter_map(serde_json::Value::as_str) {
        if let Some(first) = line.strip_prefix(&label) {
            shown.push(first.to_owned());
        } else if let (Some(chunk), Some(last)) = (line.strip_prefix("  "), shown.last_mut()) {
            last.push_str(chunk);
        }
    }
    match shown.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// 목적지 대화로 회신 행을 쌓는다. 순번은 이 run의 기존 행 다음부터 잇는다(원래 대화로 간 승인 카드가 앞선 순번을 쓴다).
/// 명시 목적지 등록부(`bot-gateway/src/schedule_routes.rs`)와 승인 카드(`approval_open.rs`)가 Slack으로 한정돼 있어
/// 여기서는 `PushTransportId::SLACK`으로 고정한다. 다른 푸시 어댑터의 명시 목적지를 열려면 `bot_user_schedules`·`bot_deliveries`에
/// 목적지 발송 키를 함께 저장하고 이 고정을 제거해야 한다.
async fn push_to(tx: &mut Tx<'_>, row: &PgRow, principal: &str, body: &str) -> R<()> {
    let dest_kind: String = row.get("dest_kind");
    match kind_from(&dest_kind)? {
        ConversationKind::Direct | ConversationKind::Channel => {}
        ConversationKind::Cli | ConversationKind::Http | ConversationKind::Scheduled => {
            return invalid();
        }
    }
    sqlx::query(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, thread, body, transport)
         select $1, $2, n.seq, 'reply:' || $2 || ':' || n.seq::text, $3, $4, $5, $6, $7, $8, $9
         from lateral (select coalesce(max(o.seq) + 1, 0) as seq from bot_outbox o where o.profile = $1 and o.run = $2) n",
    )
    .bind(row.get::<String, _>("profile"))
    .bind(row.get::<String, _>("run"))
    .bind(row.get::<String, _>("dest_binding"))
    .bind(principal)
    .bind(&dest_kind)
    .bind(row.get::<String, _>("dest_id"))
    .bind(row.get::<Option<String>, _>("dest_thread"))
    .bind(body)
    .bind(PushTransportId::SLACK.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// 최종 결과·답을 한 번 동결해 초안을 돌려준다. `None`이면 이 run은 초안이 필요 없다(일반 실행·원래 대화로 보내는 답·첨부 없는 조용한 성공).
    /// `completion = None`은 재개 때 이미 동결한 초안만 읽는다. 동결한 초안의 활성 집합이 더는 현재가 아니면 보낼 수 없어 `Conflict`다.
    pub(crate) async fn delivery_draft(
        &self,
        at: &Attempt,
        completion: Option<Finish>,
    ) -> R<Option<DeliveryDraft>> {
        let mut tx = self.begin_authorized().await?;
        if parent_of(&mut tx, at).await?.is_some() {
            tx.rollback().await?;
            return Ok(None); // 위임한 자식의 답은 부모만 보낸다
        }
        let run = lock_leased(&mut tx, at).await?;
        let Some(row) = lock(&mut tx, &at.profile.0, &at.run.0).await? else {
            tx.rollback().await?;
            return Ok(None);
        };
        match (row.get::<String, _>("state").as_str(), completion) {
            ("drafted", _) => {
                // 한 번 동결한 초안은 바뀌지 않는다: 저장된 바이트로 다시 계산해 같은 초안을 돌려준다
                recomputed(&row)?;
                let draft = draft_of(&row)?;
                if current_activation(&mut tx).await? != draft.activation.0 {
                    return Err(Error::Conflict.into());
                }
                tx.rollback().await?;
                Ok(Some(draft))
            }
            ("pending", Some(done)) => {
                let (_, result) = canonical(&done.result)?;
                if done.answers.len() > MAX_ANSWERS {
                    return invalid();
                }
                // 일반 답 규칙(길이·첨부 개수·크기·카드 금지)을 통과해야 하고, 원문으로 승인 문서를 만들 수 있어야 한다
                let normalized = done
                    .answers
                    .iter()
                    .map(replies::answer)
                    .collect::<R<Vec<_>>>()?;
                let quiet: bool =
                    sqlx::query_scalar("select quiet from bot_runs where profile = $1 and id = $2")
                        .bind(&run.profile)
                        .bind(&run.id)
                        .fetch_one(&mut *tx)
                        .await?;
                // 조용한 예약: 성공 결과 전체가 `<조용히>`이고 첨부가 없으면 보낼 것이 없다(일반 완료가 답 없이 끝낸다)
                if quiet_completion(quiet, &final_text(&result))
                    && !normalized.iter().any(|a| has_attachments(a))
                {
                    sqlx::query(
                        "update bot_deliveries set state = 'dropped', updated_ms = bot_now_ms() where profile = $1 and run = $2",
                    )
                    .bind(&run.profile)
                    .bind(&run.id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    return Ok(None);
                }
                let active: Option<String> = sqlx::query_scalar(
                    "select revision from bot_active_profiles where profile = $1",
                )
                .bind(&run.profile)
                .fetch_optional(&mut *tx)
                .await?;
                if active.is_none() {
                    return Err(Error::Denied.into()); // 비활성 프로필은 새 초안을 동결하지 못한다
                }
                let activation = current_activation(&mut tx).await?;
                let draft = DeliveryDraft {
                    activation: Revision(activation.clone()),
                    target: DeliveryTarget {
                        binding: BindingId(row.get("dest_binding")),
                        conversation: Conversation {
                            kind: kind_from(&row.get::<String, _>("dest_kind"))?,
                            id: row.get("dest_id"),
                            thread: row.get("dest_thread"),
                        },
                    },
                    answers: done.answers,
                };
                let hash = draft.sha256()?; // 승인 문서를 만들 수 없으면 `Invalid`: 보내지 않는다
                sqlx::query(
                    "update bot_deliveries set state = 'drafted', activation = $3, result = $4, answers = $5, sha256 = $6,
                            drafted_ms = bot_now_ms(), updated_ms = bot_now_ms()
                     where profile = $1 and run = $2",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(&activation)
                .bind(&result)
                .bind(draft.answers.iter().map(|a| a.0.clone()).collect::<Vec<_>>())
                .bind(hash.as_slice())
                .execute(&mut *tx)
                .await?;
                let mut entry = Entry::new("system", "delivery.draft", &run.id, "frozen");
                entry.args = Some(hash);
                audit::append(&mut tx, &run.profile, &entry).await?;
                tx.commit().await?;
                Ok(Some(draft))
            }
            _ => {
                tx.rollback().await?;
                Ok(None)
            }
        }
    }

    /// 이미 커밋된 회신 등록의 영수증. 성공한 run의 마지막 claim에만 돌려준다: 한 시점(한 문장)에서 프로필·run·세대·claim_epoch와
    /// 현재 활성 집합·등록 당시 집합을 대조한다. 아직 끝나지 않았거나 이 run에 회신이 없으면 `None`(일반 원장 펜스가 판정한다),
    /// 자격 없는 시도(옛 세대·claim)나 바뀐 집합은 `Stale`이다. 초안 본문과 모델 결과는 읽지 않는다.
    pub(crate) async fn delivery_receipt(&self, at: &Attempt) -> R<Option<JsonPayload>> {
        let row = sqlx::query(
            "select d.state, d.activation, r.state as run_state, r.generation, r.claim_epoch, c.state as call_state, c.result,
                    (select revision from bot_active where singleton) as current
             from bot_deliveries d
             join bot_runs r on r.profile = d.profile and r.id = d.run
             left join bot_calls c on c.profile = d.profile and c.run = d.run and c.call = $3 and c.tool = $4
             where d.profile = $1 and d.run = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(DELIVER_CALL)
        .bind(DELIVER_TOOL)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        // 승인한 초안이 등록된 뒤의 성공한 run만 영수증이 있다(진행 중인 claim은 일반 펜스가 판정한다)
        if row.get::<String, _>("state") != "enqueued"
            || row.get::<String, _>("run_state") != "succeeded"
        {
            return Ok(None);
        }
        let current: Option<String> = row.get("current");
        let registered: Option<String> = row.get("activation");
        let same_claim = row.get::<i64, _>("generation") == at.generation.0 as i64
            && at
                .claim
                .is_some_and(|c| row.get::<i64, _>("claim_epoch") == c.0 as i64);
        let pinned = self
            .activation
            .as_ref()
            .is_none_or(|expected| current.as_deref() == Some(expected.0.as_str()));
        if !same_claim || !pinned || current.is_none() || current != registered {
            return Err(Error::Stale.into());
        }
        if row.get::<Option<String>, _>("call_state").as_deref() != Some("done") {
            return Ok(None);
        }
        Ok(row.get::<Option<String>, _>("result").map(JsonPayload))
    }

    /// 소유자가 승인한 동결 초안을 목적지 outbox로 쌓고 run을 마친다(승인 소비·호출 완료·감사와 한 커밋).
    ///
    /// `Stale`은 소유권 상실만이 아니다: 소비하려는 순간 승인이 만료됐거나 다른 카드로 대체돼도 `Stale`이다(커밋은 없다). 호출자는 같은 펜스(현재 claim·활성 집합)를
    /// 고정 수신자 읽기(`ScheduledReplies::target`)로 확인해 가린다. 만료는 DB 시계로 판정하므로, 여기서 `Stale`이면 만료한 승인을 곧바로 닫아 둔다:
    /// 같은 claim에서 다시 판정하는 쪽이 «만료»를 읽고 승인 만료로 끝낸다(호스트 시계와 어긋나 여전히 살아 있다고 보지 않게).
    pub(crate) async fn delivery_enqueue(&self, s: StartCall) -> R<()> {
        match self.enqueue_once(s).await {
            Err(Fail::Core(Error::Stale)) => {
                let _ = self.expire_approvals().await;
                Err(Error::Stale.into())
            }
            other => other,
        }
    }

    async fn enqueue_once(&self, s: StartCall) -> R<()> {
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0])?;
        if s.call.0 != DELIVER_CALL {
            return invalid();
        }
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        // 호출에 봉인한 지문과 겨룰 값은 동결한 초안에서 다시 계산한다(활성 판본 잠금 안에서 읽는다: 초안은 동결 뒤 바뀌지 않는다)
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let seen = peek(&mut tx, &at.profile.0, &at.run.0)
            .await?
            .ok_or(Error::Missing)?;
        let sealed = recomputed(&seen)?;
        let (run, _, approval) = match claim(&mut tx, &s, DELIVER_TOOL, &sealed).await? {
            Claimed::Done(_) => {
                tx.rollback().await?;
                return Ok(());
            }
            Claimed::Fresh {
                run,
                shared_flag,
                approval,
            } => (run, shared_flag, approval),
        };
        // 다른 수신자로 나가는 회신은 승인한 호출만 보낸다
        let approval = approval.ok_or(Error::Denied)?;
        let row = lock(&mut tx, &at.profile.0, &at.run.0)
            .await?
            .ok_or(Error::Missing)?;
        if row.get::<String, _>("state") != "drafted" {
            return Err(Error::Conflict.into());
        }
        // 저장된 바이트에서 승인 문서와 지문을 다시 계산한다
        if recomputed(&row)? != sealed {
            return Err(Error::Conflict.into());
        }
        let draft = draft_of(&row)?;
        if current_activation(&mut tx).await? != draft.activation.0 {
            return Err(Error::Conflict.into()); // 수신자 매핑이 바뀐 집합: 이 초안은 보내지 않는다
        }
        let owner: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .fetch_one(&mut *tx)
                .await?;
        let stored_approver: String =
            sqlx::query_scalar("select approver from bot_approvals where nonce = $1")
                .bind(&approval)
                .fetch_one(&mut *tx)
                .await?;
        if stored_approver != owner {
            return Err(Error::Denied.into()); // 승인자는 예약을 만든 소유자 본인이다
        }
        for answer in &draft.answers {
            push_to(&mut tx, &row, &owner, &replies::answer(answer)?).await?;
        }
        // 등록 영수증은 이 커밋에서 한 번 만들고 호출의 결과로 남긴다(재호출은 원장 바이트를 그대로 돌려준다)
        let receipt = draft.queued_receipt().0;
        finish_call(&mut tx, &s, &receipt, Some(&approval)).await?;
        sqlx::query(
            "update bot_deliveries set state = 'enqueued', updated_ms = bot_now_ms() where profile = $1 and run = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .execute(&mut *tx)
        .await?;
        let result: String = row
            .get::<Option<String>, _>("result")
            .ok_or(Error::Invalid)?;
        end_run(&mut tx, &run, End::Succeeded(&result), "system").await?;
        let mut entry = Entry::new(&owner, "delivery.enqueue", &at.run.0, "enqueued");
        entry.args = Some(sealed);
        audit::append(&mut tx, &at.profile.0, &entry).await?;
        audit_done(&mut tx, &at.profile.0, &owner, &s.call.0, sealed.to_vec()).await?;
        tx.commit().await?;
        Ok(())
    }
}

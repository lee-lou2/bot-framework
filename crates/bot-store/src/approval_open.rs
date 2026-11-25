//! 승인 열기(`approval_open`): 호출이 `Open`일 때만 카드를 기록하고 run을 승인 대기로 넘긴다.
//! 푸시 대화에는 카드 회신 행을 같은 커밋으로 쌓고, 채널 출처의 명시 목적지 생성·전송 초안 카드는 공개 채널 대신 소유자 DM 전달 작업(`cards.rs`)을 쌓는다.
//!
//! 잠금 순서: 활성 집합 → run → 호출 → 승인.

use crate::{
    audit::{self, Entry},
    calls::{fence, lock_call, risk_str},
    codec::*,
    db::{R, Store, time_to_ms},
    finish::*,
    replies,
};
use bot_core::{ledger::OpenApproval, port::Error};

/// 이 실행의 루트가 접수 원장의 인증된 API 행이면 그 행의 (인증된 주체, 접수 때 확인한 전달 설치 team). 아니면 `None`.
async fn api_origin(
    pool: &sqlx::PgPool,
    profile: &str,
    run: &str,
) -> R<Option<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "select q.principal, q.api_team from bot_runs r
         join bot_requests q on q.profile = r.profile and q.run = r.root and q.api_endpoint is not null
         where r.profile = $1 and r.id = $2",
    )
    .bind(profile)
    .bind(run)
    .fetch_optional(pool)
    .await?)
}

impl Store {
    /// 승인을 연다. 호출이 `Open`일 때만, 이전 대기·승인은 `Superseded`로 닫는다. run은 승인 대기로 넘어간다.
    pub(crate) async fn approval_open(&self, open: OpenApproval) -> R<()> {
        let a = &open.approval;
        ids_ok(&[
            &a.nonce.0,
            &a.call.0,
            &a.requester.0,
            &a.approver.0,
            &a.policy_revision.0,
            &a.tool.0,
            &a.tool_revision.0,
        ])?;
        if open.reasons.len() > 16 || open.reasons.iter().any(|r| !id_ok(r) || r.len() > 64) {
            return invalid();
        }
        let (_, card) = canonical(&open.card)?;
        // 다른 수신자로 가는 승인 카드(예약 회신 전송, 수신자를 지정한 예약 생성)는 호스트 활성 집합 잠금 아래에서 연다: 카드에 보인 수신자를 확인한 등록부가
        // 속한 집합이 이 커밋에서도 현재 집합이어야 한다(집합 교체와 직렬화된다). 다른 승인(수신자 없는 생성 포함)은 고정 핸들이 아니거나 집합이 낡았더라도
        // 예전처럼 연다(이미 집은 run은 끝까지 한다).
        let delivery = a.tool.0 == bot_core::schedule::DELIVER_TOOL;
        let create_to = a.tool.0 == crate::user_schedule::CREATE_TOOL
            && crate::delivery::create_names_destination(&card)?;
        // 예약 생성 카드는 카드에 보인 인자(`인자` 행의 봉인한 정규 JSON)의 SHA-256이 승인 지문과 같아야 연다: 보이는 것(날짜·수신자·지시문)과 승인·소비되는 것이
        // 다르면(하루만 다른 날짜도) 카드를 만들지 않는다. 읽을 수 없는 행은 위에서 `Invalid`로 먼저 걸러진다. 아무것도 기록하기 전에 거절한다
        if a.tool.0 == crate::user_schedule::CREATE_TOOL
            && crate::delivery::create_shown_args_sha256(&card)? != a.args_sha256
        {
            return Err(Error::Conflict.into());
        }
        // 수신자를 지정한 생성은 등록부가 확인한 집합을 고정한 핸들로만 연다. 고정하지 않은 핸들은 그 집합을 몰라 이 커밋 안에서 비교할 수 없다(전송 카드는
        // 동결한 초안이 집합을 들고 있어 고정하지 않은 핸들도 비교한다). 아무것도 기록하기 전에 거절한다.
        if create_to && self.activation.is_none() {
            return Err(Error::Denied.into());
        }
        // OpenAI 호환 입구로 접수된 실행(루트가 접수 원장의 인증된 API 행)의 승인은 전부 활성 집합 잠금 아래에서 연다: 브로커가 승인 경로(지정된 사람·전달 바인딩)를
        // 확인한 집합이 이 커밋에서도 현재 집합이어야 한다. 교체가 먼저 커밋됐으면 옛 로컬 카드도 옛 전달 작업도 만들지 않는다(`Stale`). 집합을 고정하지 않은 핸들은
        // 비교할 대상이 없어 거절한다. 접수 행은 만든 뒤 바뀌지 않아 트랜잭션 밖에서 읽어도 같다. 본문·레거시 실행의 승인은 예전처럼 연다.
        let origin = api_origin(&self.pool, &a.attempt.profile.0, &a.attempt.run.0).await?;
        if origin.is_some() && self.activation.is_none() {
            return Err(Error::Denied.into());
        }
        // 전달 바인딩은 접수 원장이 보증한 루트의, 접수 때 확인한 전달 설치(team)가 있는 요청만 받는다. 브로커가 싣는 값이 아니라 접수 행이 근거다.
        // 아무것도 기록하기 전에 거절한다
        let slack = match (&open.delivery, &origin) {
            (None, _) => None,
            (Some(binding), Some((_, Some(team)))) => Some((binding, team.clone())),
            (Some(_), _) => return Err(Error::Denied.into()),
        };
        let routed = delivery || create_to;
        let mut tx = if routed || origin.is_some() {
            self.begin_authorized().await?
        } else {
            self.pool.begin().await?
        };
        if routed && self.activation.is_none() {
            // 집합을 고정하지 않은 핸들(전송 카드만 여기에 온다)도 이 카드를 기록하는 커밋은 활성 집합 행을 먼저 `for share`로 잠근다(집합 교체와 직렬화). 아래 초안의
            // 집합 대조가 이 커밋 안의 값이 된다(고정한 핸들은 `begin_authorized`가 이미 잠갔다). 잠금 순서: 활성 집합 → run → 호출.
            sqlx::query("select 1 from bot_active where singleton for share")
                .fetch_optional(&mut *tx)
                .await?;
        }
        let run = fence(&mut tx, &a.attempt).await?;
        if run.private {
            return Err(Error::Denied.into()); // 승인 카드에는 민감 인자를 실을 수 없다
        }
        // 소유자 DM으로 가는 카드(채널 출처의 명시 목적지 생성·전송 초안)는 인증된 요청자와 지정 승인자가 같은 사람일 때만 연다. 아무것도 기록하기 전에 거절한다
        if routed && a.requester != a.approver {
            let kind: String =
                sqlx::query_scalar("select conv_kind from bot_runs where profile = $1 and id = $2")
                    .bind(&run.profile)
                    .bind(&run.id)
                    .fetch_one(&mut *tx)
                    .await?;
            if kind == "channel" {
                return Err(Error::Denied.into());
            }
        }
        // API 출처 승인의 요청자는 접수 행이 인증한 주체(위임 자식도 루트의 주체를 이어받는다)와 같아야 한다: 브로커가 싣는 값이 아니라 접수 행이 근거다.
        // 아무것도 기록하기 전에 거절한다
        if let Some((principal, _)) = &origin
            && *principal != a.requester.0
        {
            return Err(Error::Denied.into());
        }
        let call = lock_call(&mut tx, &a.attempt, &a.call).await?;
        // 예약 회신 승인은 동결한 초안의 승인 문서가 카드에 그대로 보이고 소유자 본인이 승인자일 때만 연다
        if delivery {
            crate::delivery::check_open(&mut tx, a, &card).await?;
        }
        let recorded = crate::calls::record_of(&call)?;
        // 예약 생성은 호출을 연 지문(`begin`)도 같아야 한다: 카드·승인·호출 세 지문이 하나다
        if a.tool.0 == crate::user_schedule::CREATE_TOOL && recorded.args_sha256 != a.args_sha256 {
            return Err(Error::Conflict.into());
        }
        let previous = match recorded.phase {
            bot_core::ledger::CallPhase::Open { approval } => approval,
            _ => return Err(Error::Conflict.into()),
        };
        let expires = time_to_ms(a.expires_at);
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        if expires <= now {
            return invalid();
        }
        if let Some(old) = previous {
            let closed = sqlx::query(
                "update bot_approvals set state = 'superseded' where nonce = $1 and state in ('pending', 'approved')",
            )
            .bind(&old.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if closed == 1 {
                audit::append(
                    &mut tx,
                    &run.profile,
                    &Entry::new(&a.requester.0, "approval.supersede", &old.0, "superseded"),
                )
                .await?;
            }
        }
        let inserted = sqlx::query(
            "insert into bot_approvals (nonce, profile, run, generation, call, requester, approver, risk, policy_revision,
                                        tool, tool_revision, args_sha256, expires_ms, reasons, card)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(&a.nonce.0)
        .bind(&run.profile)
        .bind(&run.id)
        .bind(a.attempt.generation.0 as i64)
        .bind(&a.call.0)
        .bind(&a.requester.0)
        .bind(&a.approver.0)
        .bind(risk_str(a.risk))
        .bind(&a.policy_revision.0)
        .bind(&a.tool.0)
        .bind(&a.tool_revision.0)
        .bind(a.args_sha256.as_slice())
        .bind(expires)
        .bind(&open.reasons)
        .bind(&card)
        .execute(&mut *tx)
        .await;
        if let Err(e) = inserted {
            let fail = crate::db::Fail::from(e);
            return Err(if fail.is_unique_violation() {
                Error::Conflict.into()
            } else {
                fail
            });
        }
        sqlx::query("update bot_calls set approval = $4, risk = $5, tool_revision = $6, updated_ms = bot_now_ms() where profile = $1 and run = $2 and call = $3")
            .bind(&run.profile)
            .bind(&run.id)
            .bind(&a.call.0)
            .bind(&a.nonce.0)
            .bind(risk_str(a.risk))
            .bind(&a.tool_revision.0)
            .execute(&mut *tx)
            .await?;
        // 푸시 대화에는 승인 카드를 회신 행으로 같은 커밋에 쌓는다(브로커가 만든 카드만 허용한다)
        let (kind, binding, revision): (String, String, String) = sqlx::query_as(
            "select conv_kind, binding, revision from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        if let Some((delivery, team)) = slack {
            // OpenAI 호환 입구로 접수된 실행의 일반 도구 승인: 지정된 사람의 Slack DM으로만 전달한다(전달 바인딩은 브로커가 승인 경로에서 정했다).
            // 인증된 접수 원장이 보증한 루트(위임 자식이면 그 루트)일 때만 받는다: 본문·레거시 webhook 실행에는 이 경로가 없다. 카드는 영수증이 확정되기 전까지 열리지 않고,
            // 원래 요청의 바인딩·요청자·판본·전달 설치(team)를 작업에 고정해 둔다. HTTP 출처에는 원래 채널이 없어 공개 안내를 쌓지 않는다
            ids_ok(&[&delivery.0])?;
            sqlx::query(
                "insert into bot_card_jobs (nonce, profile, run, binding, owner, generation, origin_binding, requester, revision, origin_team)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(&a.nonce.0)
            .bind(&run.profile)
            .bind(&run.id)
            .bind(&delivery.0)
            .bind(&a.approver.0)
            .bind(a.attempt.generation.0 as i64)
            .bind(&binding)
            .bind(&a.requester.0)
            .bind(&revision)
            .bind(team)
            .execute(&mut *tx)
            .await?;
        } else if kind == "channel" && routed {
            // 채널 출처의 명시 목적지 생성·전송 초안 카드는 공개 채널에 올리지 않는다: 소유자 1:1 DM으로만 전달한다. 같은 커밋에 전달 작업을 쌓아 두면
            // 카드는 영수증(DM 채널·메시지 ts)이 확정될 때까지 열리지 않는다(결정·목록·클릭 대상 아님). 공개 안내는 그 확정 커밋의 한 줄뿐이다
            sqlx::query(
                "insert into bot_card_jobs (nonce, profile, run, binding, owner, generation) values ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&a.nonce.0)
            .bind(&run.profile)
            .bind(&run.id)
            .bind(&binding)
            .bind(&a.approver.0)
            .bind(a.attempt.generation.0 as i64)
            .execute(&mut *tx)
            .await?;
        } else if matches!(kind.as_str(), "direct" | "channel") {
            let card = replies::card(&open.card, &a.nonce.0)?;
            push_reply(&mut tx, &run.profile, &run.id, &card).await?;
        }
        park(&mut tx, &run, &Wait::Approval).await?;
        let mut entry = Entry::new(
            &a.requester.0,
            "approval.open",
            &a.nonce.0,
            risk_str(a.risk),
        );
        entry.args = Some(a.args_sha256);
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(())
    }
}

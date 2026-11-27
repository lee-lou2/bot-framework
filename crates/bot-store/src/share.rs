//! 기억 공유(`MemoryStore::share`): 활성 판본·리스·소유자·본문 지문을 확인하고, 승인 소비·가시성 변경·호출 완료·감사를 **한 커밋**으로 한다.
//! 지문은 카드에 보인 정확한 본문의 SHA-256과 기억 ID를 묶은 값(`bot_core::command::memory_share_hash`)이다. 호출·승인의 인자 해시가
//! 지금 본문의 지문과 다르면(카드를 본 뒤 편집) 승인을 소비하지 않고 `Conflict`다. 완료한 호출의 재접수는 아무것도 바꾸지 않는다.
//! 일반 `CallLedger::start`는 이 도구를 거절한다.

use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, lock_root_shared, phase_of, risk_str, tree_flagged},
    codec::*,
    db::{R, Store},
    memory::lineage,
};
use bot_core::{
    command::memory_share_hash,
    id::*,
    ledger::{CallPhase, StartCall},
    port::Error,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;

/// 기억 공유 도구의 ID.
pub(crate) use bot_core::builtin::SHARE_TOOL;

impl Store {
    pub(crate) async fn memory_share(&self, s: StartCall, id: MemoryId) -> R<MemoryId> {
        let at = &s.attempt;
        ids_ok(&[&s.owner, &s.checked_revision.0, &id.0, &s.call.0])?;
        let mut tx = self.begin_authorized().await?;
        // 잠금 순서: 활성 판본 → 루트 run(공유) → 이 run → 호출 → 승인 → 기억 → 감사
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let call = lock_call(&mut tx, at, &s.call).await?;
        if call.get::<String, _>("tool") != SHARE_TOOL {
            return Err(Error::Denied.into());
        }
        match phase_of(&call)? {
            CallPhase::Done(result) => {
                // 완료한 호출의 재접수: 같은 기억이면 다시 공유하지 않는다
                tx.rollback().await?;
                let done = serde_json::from_str::<Value>(&result.0)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned));
                return if done.as_deref() == Some(id.0.as_str()) {
                    Ok(id)
                } else {
                    Err(Error::Conflict.into())
                };
            }
            CallPhase::Open { .. } => {}
            _ => return Err(Error::Conflict.into()),
        }
        if tree_flagged(&mut tx, &run, shared_flag).await? && !s.private_recall {
            return Err(Error::Conflict.into()); // 판정한 뒤 다른 실행이 private 기억을 조회했다
        }
        // 승인은 소유자 본인이 자기 기억에 대해서만 한다
        let Some((expected, reasons)) = &s.approval else {
            return Err(Error::Denied.into());
        };
        let me = sqlx::query(
            "select principal, agent, conv_kind from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        let (principal, agent, kind): (String, String, String) =
            (me.get("principal"), me.get("agent"), me.get("conv_kind"));
        if expected.tool.0 != SHARE_TOOL
            || expected.requester.0 != principal
            || expected.approver.0 != principal
        {
            return Err(Error::Denied.into());
        }
        crate::approvals::consume(&mut tx, at, &s.call, expected, reasons).await?;
        let memory = sqlx::query(
            "select agent, owner, visibility, body from bot_memories
             where profile = $1 and id = $2 and deleted_ms is null for update",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let private = memory.get::<String, _>("visibility") == "private";
        if memory.get::<String, _>("owner") != principal
            || !lineage(&agent).contains(&memory.get::<String, _>("agent"))
            || (private && !matches!(kind.as_str(), "direct" | "cli"))
        {
            return Err(Error::Denied.into());
        }
        // 카드에 보인 본문 지문과 지금 본문의 지문이 같아야 한다(다르면 승인은 소비되지 않고 되돌아간다)
        let body: String = memory.get("body");
        let fingerprint = memory_share_hash(&id, &Sha256::digest(body.as_bytes()).into());
        let call_hash: Vec<u8> = call.get("args_sha256");
        if expected.args_sha256 != fingerprint || call_hash.as_slice() != fingerprint.as_slice() {
            return Err(Error::Conflict.into());
        }
        let shared = sqlx::query(
            "update bot_memories set visibility = 'profile', updated_ms = bot_now_ms()
             where profile = $1 and id = $2 and visibility = 'private'",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, started_generation = $5, owner = $6, started_ms = bot_now_ms(),
                    finished_ms = bot_now_ms(), updated_ms = bot_now_ms(), risk = $7, tool_revision = $8, approval = $9
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&s.call.0)
        .bind(json!({ "id": id.0 }).to_string())
        .bind(at.generation.0 as i64)
        .bind(&s.owner)
        .bind(risk_str(s.risk))
        .bind(&s.tool_revision.0)
        .bind(&expected.nonce.0)
        .execute(&mut *tx)
        .await?;
        let mut entry = OwnedEntry::new(
            &run.profile,
            &principal,
            "memory.share",
            &id.0,
            if shared { "shared" } else { "already_shared" },
        );
        entry.args = Some(fingerprint);
        let mut done = OwnedEntry::new(&run.profile, &principal, "call.done", &s.call.0, "done");
        done.args = Some(to32(call_hash)?);
        audit::append_many(&mut tx, &[entry, done]).await?;
        tx.commit().await?;
        Ok(id)
    }
}

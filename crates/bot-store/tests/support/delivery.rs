//! Scheduled delivery draft test helpers.
#![allow(dead_code)]

use super::*;
use bot_core::{
    port::Error,
    schedule::{
        DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DELIVERY_CARD_LABEL, DeliveryDraft,
        ScheduledReplies,
    },
};
use serde_json::json;

pub fn replies(db: &TestDb) -> &dyn ScheduledReplies {
    &db.store
}

pub async fn destination_run(db: &TestDb, kind: ConversationKind) -> RunContext {
    destination_run_as(db, "r1", kind).await
}

pub async fn destination_run_as(db: &TestDb, request: &str, kind: ConversationKind) -> RunContext {
    destination_run_to(db, request, kind, "D9").await
}

pub async fn destination_run_to(
    db: &TestDb,
    request: &str,
    kind: ConversationKind,
    dest: &str,
) -> RunContext {
    let run = run_in(db, request, OWNER, kind).await;
    sqlx::query(
        "insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id, dest_thread)
         values ('personal', $1, 'slack', 'direct', $2, 't1')",
    )
    .bind(&run.attempt.run.0)
    .bind(dest)
    .execute(db.store.pool())
    .await
    .unwrap();
    run
}

pub fn answers(texts: &[&str]) -> Finish {
    Finish {
        result: JsonPayload(json!({"text": texts.join("/")}).to_string()),
        answers: texts
            .iter()
            .map(|t| JsonPayload(json!({"text": t}).to_string()))
            .collect(),
    }
}

/// Formats an approval document under `DELIVERY_CARD_LABEL` wrapped at 480-byte UTF-8 boundaries with `"  "` continuations.
pub fn card_for(nonce: &str, document: &str) -> JsonPayload {
    let mut lines = Vec::new();
    let mut rest = document;
    while !rest.is_empty() {
        let prefix = if lines.is_empty() {
            format!("{DELIVERY_CARD_LABEL}: ")
        } else {
            "  ".to_owned()
        };
        let mut cut = (480 - prefix.len()).min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        lines.push(format!("{prefix}{head}"));
        rest = tail;
    }
    JsonPayload(
        json!({"approval": {"nonce": nonce, "title": "Scheduled delivery approval", "lines": lines}})
            .to_string(),
    )
}

pub fn delivery_approval(at: &Attempt, nonce: &str, hash: [u8; 32]) -> Approval {
    Approval {
        tool: tool(DELIVER_TOOL),
        tool_revision: Revision(DELIVER_REVISION.into()),
        risk: Risk::Irreversible,
        args_sha256: hash,
        ..binding(at, DELIVER_CALL, nonce)
    }
}

pub fn open_delivery(approval: &Approval, document: &str) -> OpenApproval {
    OpenApproval {
        approval: approval.clone(),
        reasons: vec!["irreversible".into()],
        card: card_for(&approval.nonce.0, document),
        delivery: None,
    }
}

pub async fn request_delivery(
    db: &TestDb,
    run: &RunContext,
    finish: Finish,
) -> (DeliveryDraft, Approval) {
    let draft = replies(db)
        .draft(run.attempt.clone(), Some(finish))
        .await
        .unwrap()
        .expect("expected draft");
    let hash = draft.sha256().unwrap();
    calls(db)
        .begin(
            run.attempt.clone(),
            call(DELIVER_CALL),
            tool(DELIVER_TOOL),
            hash,
        )
        .await
        .unwrap();
    let approval = delivery_approval(&run.attempt, "n1", hash);
    calls(db)
        .open_approval(open_delivery(&approval, &draft.approval_json().unwrap()))
        .await
        .unwrap();
    (draft, approval)
}

pub async fn approved_start(db: &TestDb, approval: &Approval) -> (Attempt, StartCall) {
    calls(db)
        .decide(Decision {
            nonce: approval.nonce.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(db).claim().await.unwrap().unwrap();
    let bound = Approval {
        attempt: resumed.attempt.clone(),
        ..approval.clone()
    };
    let start = StartCall {
        risk: Risk::Irreversible,
        tool_revision: Revision(DELIVER_REVISION.into()),
        ..start_call(
            &resumed.attempt,
            DELIVER_CALL,
            Some((bound, vec!["irreversible".into()])),
        )
    };
    (resumed.attempt, start)
}

pub async fn outbox_rows(db: &TestDb, conv_id: &str) -> i64 {
    db.scalar::<i64>(&format!(
        "select count(*) from bot_outbox where conv_id = '{conv_id}'"
    ))
    .await
}

pub fn is_conflict<T: std::fmt::Debug>(result: Result<T, Error>) -> bool {
    matches!(result, Err(Error::Conflict))
}

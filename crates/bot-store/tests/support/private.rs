//! Test helpers for the private DM ledger.
#![allow(dead_code)]

use bot_core::{
    id::*,
    intake::RequestKey,
    ledger::*,
    state::{Conversation, ConversationKind},
};

use super::*;

pub const TOOL: &str = "inbox.receive";

pub fn request(request: &str, fingerprint: u8) -> PrivateRequest {
    PrivateRequest {
        key: RequestKey {
            profile: ProfileId("personal".into()),
            binding: BindingId("direct".into()),
            principal: PrincipalId("local:owner".into()),
            request: RequestId(request.into()),
        },
        fingerprint: [fingerprint; 32],
        pass_fingerprint: None,
        revision: Revision("r1".into()),
        agent: AgentId("main".into()),
        tool: ToolId(TOOL.into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        scopes: scopes(&["inbox.write"]),
    }
}

pub fn ledger(db: &TestDb) -> &dyn PrivateLedger {
    &db.store
}

pub async fn reserve(db: &TestDb, name: &str, fingerprint: u8) -> Attempt {
    match ledger(db).begin(request(name, fingerprint)).await.unwrap() {
        PrivatePermit::Start { attempt: at, .. } => at,
        _ => panic!("first reservation must return Start"),
    }
}

pub fn start_of(at: &Attempt, fingerprint: u8) -> PrivateStart {
    PrivateStart {
        attempt: at.clone(),
        tool: ToolId(TOOL.into()),
        fingerprint: [fingerprint; 32],
        checked_revision: Revision("r1".into()),
    }
}

pub async fn started(db: &TestDb, name: &str, fingerprint: u8) -> Attempt {
    let at = reserve(db, name, fingerprint).await;
    ledger(db).start(start_of(&at, fingerprint)).await.unwrap();
    at
}

pub async fn row(db: &TestDb) -> String {
    db.scalar::<String>(
        "select state || ':' || coalesce(wait_kind, '-') || ':' || generation || ':' || claim_epoch || ':' || (private_started_ms is not null)::text from bot_runs where private",
    )
    .await
}

pub fn epoch(at: &Attempt) -> u64 {
    at.claim.unwrap().0
}

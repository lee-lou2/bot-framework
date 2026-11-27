//! Test helpers for the keyed OpenAI-compatible HTTP endpoint.
#![allow(dead_code)]

use super::*;
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiKey, ApiRun, ApiRuns},
    command::HttpRoute,
    id::*,
    profile::ResponseMode,
    state::*,
};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub fn principal(sub: &str) -> PrincipalId {
    PrincipalId(format!("http:openai/{sub}"))
}

/// Builds an API accept request with a fresh conversation ID per call.
pub fn request(key: &str, sub: &str, model: &str, digest: u8, max_active: u32) -> ApiAccept {
    let conversation = format!("conv-{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let mut accept = accept_as(
        "personal",
        key,
        &principal(sub).0,
        ConversationKind::Http,
        &conversation,
        "question",
    );
    accept.event.binding = BindingId(format!("b-{model}"));
    accept.event.http = Some(HttpRoute {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        response: ResponseMode::Accepted,
    });
    ApiAccept {
        accept,
        endpoint: "openai".into(),
        model: model.into(),
        digest: [digest; 32],
        max_active,
        team: None,
    }
}

pub fn lookup_key(key: &str, sub: &str) -> ApiKey {
    ApiKey {
        profile: ProfileId("personal".into()),
        endpoint: "openai".into(),
        principal: principal(sub),
        key: RequestId(key.into()),
    }
}

pub async fn admit(db: &TestDb, req: ApiAccept) -> ApiAdmission {
    ApiRuns::accept(&db.store, req).await.unwrap()
}

pub fn accepted(admission: ApiAdmission) -> ApiRun {
    match admission {
        ApiAdmission::Accepted(run) => run,
        other => panic!("expected fresh accept: {other:?}"),
    }
}

pub async fn requests(db: &TestDb) -> i64 {
    db.scalar("select count(*) from bot_requests where api_endpoint is not null")
        .await
}

pub async fn runs_of_endpoint(db: &TestDb) -> i64 {
    db.scalar("select count(*) from bot_runs where api_endpoint is not null")
        .await
}

pub async fn lookup(db: &TestDb, key: &str, sub: &str) -> Option<ApiRun> {
    ApiRuns::lookup(&db.store, lookup_key(key, sub))
        .await
        .unwrap()
}

pub async fn claimed(db: &TestDb) -> RunContext {
    runs(db).claim().await.unwrap().expect("run to claim")
}

/// Handle pinned to `set-1`; required when opening API-origin approvals.
pub fn pinned(db: &TestDb) -> bot_store::Store {
    db.store.for_activation(Revision("set-1".into()))
}

pub fn api_approval(at: &Attempt, call: &str, nonce: &str, sub: &str) -> Approval {
    Approval {
        requester: principal(sub),
        ..binding(at, call, nonce)
    }
}

pub async fn open_local(db: &TestDb, at: &Attempt, call: &str, nonce: &str) {
    begin_call(db, at, call).await;
    bot_core::ledger::CallLedger::open_approval(
        &pinned(db),
        open_approval(api_approval(at, call, nonce, "build")),
    )
    .await
    .unwrap();
}

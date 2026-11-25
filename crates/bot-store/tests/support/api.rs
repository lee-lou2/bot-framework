//! 키로 이어지는 긴 요청 입구(OpenAI 호환 HTTP) 시험 도구.
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

/// 이 입구의 접수 요청. 대화·스레드는 POST마다 새 값이다(재전달에서는 다르다).
pub fn request(key: &str, sub: &str, model: &str, digest: u8, max_active: u32) -> ApiAccept {
    let conversation = format!("conv-{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let mut accept = accept_as(
        "personal",
        key,
        &principal(sub).0,
        ConversationKind::Http,
        &conversation,
        "질문",
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
        other => panic!("새로 접수해야 한다: {other:?}"),
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

/// 이 run을 집어(claim) 실행 중으로 만든다.
pub async fn claimed(db: &TestDb) -> RunContext {
    runs(db).claim().await.unwrap().expect("집을 run")
}

/// API 출처 승인은 집합을 고정한 핸들(게시한 집합 `set-1`)로만 열린다: 브로커가 승인 경로를 확인한 집합이 같은 커밋에서도 현재여야 한다.
pub fn pinned(db: &TestDb) -> bot_store::Store {
    db.store.for_activation(Revision("set-1".into()))
}

/// API 실행의 승인: 요청자는 접수 행이 인증한 HTTP 주체다(`sub`), 승인자는 같은 호출의 기본값.
pub fn api_approval(at: &Attempt, call: &str, nonce: &str, sub: &str) -> Approval {
    Approval {
        requester: principal(sub),
        ..binding(at, call, nonce)
    }
}

/// 호출을 열고 로컬 승인(전달 바인딩 없음)을 연다. 요청자는 `build` 주체다.
pub async fn open_local(db: &TestDb, at: &Attempt, call: &str, nonce: &str) {
    begin_call(db, at, call).await;
    bot_core::ledger::CallLedger::open_approval(
        &pinned(db),
        open_approval(api_approval(at, call, nonce, "build")),
    )
    .await
    .unwrap();
}

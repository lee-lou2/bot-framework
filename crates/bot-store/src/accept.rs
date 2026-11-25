//! 외부 접수와 요청 상태 조회. 접수 성공은 커밋 성공이다.
//! 키: (profile, binding, principal, request). 같은 키·같은 입력 해시는 기존 run, 다른 해시는 충돌.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store, Tx},
};
use bot_core::{
    command::{Accept, HttpRoute},
    id::*,
    intake::{RequestKey, RequestState},
    port::Error,
    profile::ResponseMode,
    state::{ConversationKind, RunOrigin},
};
use serde_json::{Value, json};
use sqlx::Row;

/// 입력 해시: 외부에서 온 것(대화·첨부·본문)만 담는다. 공백·키 순서가 달라도 같은 값이다.
/// 대상(`Accept.target`)은 접수 시점의 라우팅 결과라서 넣지 않는다: 프로필 판본이 바뀐 뒤 같은 외부 요청이 다시 와도
/// 충돌이 아니라 **최초 접수의 실행 계획**(저장된 run)으로 수렴해야 한다.
fn input_digest(a: &Accept, body: &Value) -> R<[u8; 32]> {
    let c = &a.event.conversation;
    let mut doc = json!({
        "conversation": {"kind": kind_str(&c.kind), "id": c.id, "thread": c.thread},
        "attachments": a.event.attachments,
        "body": body,
    });
    // HTTP 입구는 메서드·경로까지 같은 요청이다(다른 어댑터의 해시 바이트는 그대로다)
    if let Some(route) = &a.event.http {
        doc["http"] = json!({"method": route.method, "path": route.path});
    }
    Ok(sha256_text(&doc.to_string()))
}

/// 대문자 메서드와 `/`로 시작하는 경로(쿼리·제어 문자 없음)만 받는다.
fn route_ok(method: &str, path: &str) -> bool {
    !method.is_empty()
        && method.len() <= 16
        && method.bytes().all(|b| b.is_ascii_uppercase())
        && path.starts_with('/')
        && path.len() <= 512
        && !path.contains('?')
        && path.chars().all(|c| !c.is_control() && c != ' ')
}

fn response_str(mode: ResponseMode) -> &'static str {
    match mode {
        ResponseMode::Accepted => "accepted",
        ResponseMode::Sync => "sync",
    }
}

/// 검증을 마친 접수 명령의 정규화한 조각들.
pub(crate) struct Checked {
    pub agent: AgentId,
    pub scopes: Vec<String>,
    pub body_text: String,
    pub target: Value,
    pub digest: [u8; 32],
}

/// 접수 명령을 검증하고 정규화한다(식별자·첨부·본문·대상). 접수와 예약 발생이 같이 쓴다.
pub(crate) fn check(a: &Accept) -> R<Checked> {
    let e = &a.event;
    let agent = agent_of(&a.target).clone();
    ids_ok(&[
        &e.profile.0,
        &e.binding.0,
        &e.principal.0,
        &e.request.0,
        &e.conversation.id,
        &a.revision.0,
        &agent.0,
    ])?;
    if e.conversation.thread.as_deref().is_some_and(|t| !id_ok(t)) {
        return invalid();
    }
    // HTTP 대화는 인증된 HTTP 어댑터의 경로 정보와 함께만 온다(그 반대도 같다)
    if (e.conversation.kind == ConversationKind::Http) != e.http.is_some()
        || e.http
            .as_ref()
            .is_some_and(|h| !route_ok(&h.method, &h.path))
    {
        return invalid();
    }
    attachments_ok(&e.attachments)?;
    let scopes: Vec<String> = a.scopes.iter().cloned().collect();
    if scopes.iter().any(|s| !id_ok(s)) || a.approver.as_ref().is_some_and(|p| !id_ok(&p.0)) {
        return invalid();
    }
    let (body, body_text) = canonical(&e.body)?;
    Ok(Checked {
        digest: input_digest(a, &body)?,
        target: target_json(&a.target)?,
        agent,
        scopes,
        body_text,
    })
}

/// run 한 줄을 넣는다. 호출자가 트랜잭션을 쥐고 있다.
pub(crate) async fn insert_run(
    tx: &mut Tx<'_>,
    a: &Accept,
    c: &Checked,
    id: &str,
    origin: RunOrigin,
    trigger: Option<&str>,
    serial_key: Option<&str>,
) -> R<()> {
    let e = &a.event;
    sqlx::query(
        "insert into bot_runs (profile, id, root, agent, principal, scopes, revision, binding,
                               conv_kind, conv_id, thread, attachments, target, input, approver, trigger, serial_key, run_origin)
         values ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
    )
    .bind(&e.profile.0)
    .bind(id)
    .bind(&c.agent.0)
    .bind(&e.principal.0)
    .bind(&c.scopes)
    .bind(&a.revision.0)
    .bind(&e.binding.0)
    .bind(kind_str(&e.conversation.kind))
    .bind(&e.conversation.id)
    .bind(&e.conversation.thread)
    .bind(&e.attachments)
    .bind(&c.target)
    .bind(&c.body_text)
    .bind(a.approver.as_ref().map(|p| p.0.as_str()))
    .bind(trigger)
    .bind(serial_key)
    .bind(origin.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// 접수한다. 같은 키·같은 입력 해시는 기존 run, 다른 해시는 `Conflict`다.
    pub(crate) async fn accept_run(&self, a: Accept) -> R<RunId> {
        let c = check(&a)?;
        let e = &a.event;
        let profile = &e.profile.0;
        let mut tx = self.begin_authorized().await?;
        let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        // 응답 방식은 최초 접수에서 정해 저장하고 중복 해시에는 넣지 않는다
        let fresh = sqlx::query(
            "insert into bot_requests (profile, binding, principal, request, input_sha256, run,
                                       http_method, http_path, http_response)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict do nothing",
        )
        .bind(profile)
        .bind(&e.binding.0)
        .bind(&e.principal.0)
        .bind(&e.request.0)
        .bind(c.digest.as_slice())
        .bind(&run)
        .bind(e.http.as_ref().map(|h| h.method.as_str()))
        .bind(e.http.as_ref().map(|h| h.path.as_str()))
        .bind(e.http.as_ref().map(|h| response_str(h.response)))
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !fresh {
            let row = sqlx::query(
                "select input_sha256, run, api_endpoint from bot_requests
                 where profile = $1 and binding = $2 and principal = $3 and request = $4",
            )
            .bind(profile)
            .bind(&e.binding.0)
            .bind(&e.principal.0)
            .bind(&e.request.0)
            .fetch_one(&mut *tx)
            .await?;
            let stored: Vec<u8> = row.get("input_sha256");
            let api: Option<String> = row.get("api_endpoint");
            tx.rollback().await?;
            // 키로 이어지는 입구(OpenAI 호환)의 접수 행은 일반 접수가 읽거나 이어받지 못한다: 같은 키는 충돌이다(재배포로 입구 종류가 바뀌어도 옛 작업은 새지 않는다)
            return if api.is_none() && stored.as_slice() == c.digest.as_slice() {
                Ok(RunId(row.get("run")))
            } else {
                Err(Error::Conflict.into())
            };
        }
        // 새 외부 요청만 활성 판본과 대조한다(중복 재전달은 위에서 기존 run으로 끝났다). 어긋나면 접수 행도 남지 않는다
        crate::active::guard_active(&mut tx, profile, &a.revision).await?;
        let origin = RunOrigin::from_ingress(&e.conversation.kind, &e.principal);
        insert_run(&mut tx, &a, &c, &run, origin, None, None).await?;
        let mut entry = Entry::new(&e.principal.0, "run.accept", &run, "accepted");
        entry.args = Some(c.digest);
        audit::append(&mut tx, profile, &entry).await?;
        tx.commit().await?;
        Ok(RunId(run))
    }

    /// 접수 키로 run 상태를 조회한다. 같은 키의 재조회는 새 효과를 만들지 않는다.
    pub(crate) async fn status_of(&self, key: &RequestKey) -> R<Option<RequestState>> {
        let mut tx = self.begin_authorized().await?;
        let row = sqlx::query(
            "select r.id, r.state, r.result, q.http_method, q.http_path, q.http_response from bot_requests q
             join bot_runs r on r.profile = q.profile and r.id = q.run
             where q.profile = $1 and q.binding = $2 and q.principal = $3 and q.request = $4
               and q.api_endpoint is null",
        )
        .bind(&key.profile.0)
        .bind(&key.binding.0)
        .bind(&key.principal.0)
        .bind(&key.request.0)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        let Some(row) = row else { return Ok(None) };
        let result: Option<String> = row.get("result");
        let http = match (
            row.get::<Option<String>, _>("http_method"),
            row.get::<Option<String>, _>("http_path"),
            row.get::<Option<String>, _>("http_response"),
        ) {
            (Some(method), Some(path), Some(mode)) => Some(HttpRoute {
                method,
                path,
                response: if mode == "sync" {
                    ResponseMode::Sync
                } else {
                    ResponseMode::Accepted
                },
            }),
            _ => None, // HTTP 입구가 아니다
        };
        Ok(Some(RequestState {
            run: RunId(row.get("id")),
            state: state_from(&text(&row, "state"))?,
            result: result.map(bot_core::command::JsonPayload),
            http,
        }))
    }
}

use crate::db::text;

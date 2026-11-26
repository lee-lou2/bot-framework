//! 사전 승인 관리 API: 목록과 트리거 단위 정지·재개(운영자 전용).
//! 요청은 화면이 본 승인 ID와 정지 세대(`{grant_id, epoch}`)를 함께 보내고, 저장소가 현재와 대조해 전환과 감사를 한 커밋에 쓴다.
//! 성공 본문은 래핑 없는 새 행이다. 같은 명령의 재전달·이미 그 상태인 요청은 중복 감사 없이 같은 결과(200)이고, 교체·ABA는 409 `stale`이다.

use crate::{
    api::{INVALID, R, S, V, body, identifier, operator, profile_id},
    json,
    resp::ok,
};
use axum::{
    body::Bytes,
    extract::{Path, RawQuery},
};
use bot_core::{
    grant::{GrantCas, TriggerKey},
    id::AgentId,
};
use serde_json::{Value, json};

/// 에이전트 ID는 `main`·`main/developer`처럼 이름을 `/`로 이은 것이다(경로 한 조각으로 퍼센트 인코딩돼 온다).
fn agent_id(text: &str) -> Result<AgentId, crate::resp::Fail> {
    (!text.is_empty() && text.split('/').all(bot_core::id::is_name))
        .then(|| AgentId(text.to_owned()))
        .ok_or(INVALID)
}

pub(crate) async fn list(
    axum::extract::State(st): S,
    axum::Extension(v): V,
    RawQuery(raw): RawQuery,
) -> R {
    operator(&v)?;
    // 프로필 선택은 화면에서만 한다: 알 수 없는 쿼리는 거절한다
    if raw.as_deref().is_some_and(|q| !q.is_empty()) {
        return Err(INVALID);
    }
    let rows = st.read.preapprovals(&v).await?;
    let body = json!({"items": rows.iter().map(json::preapproval).collect::<Vec<_>>()});
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn pause(
    state: axum::extract::State<std::sync::Arc<crate::api::St>>,
    viewer: axum::Extension<bot_core::admin::Viewer>,
    path: Path<(String, String, String)>,
    bytes: Bytes,
) -> R {
    toggle(state, viewer, path, bytes, true).await
}

pub(crate) async fn resume(
    state: axum::extract::State<std::sync::Arc<crate::api::St>>,
    viewer: axum::Extension<bot_core::admin::Viewer>,
    path: Path<(String, String, String)>,
    bytes: Bytes,
) -> R {
    toggle(state, viewer, path, bytes, false).await
}

async fn toggle(
    axum::extract::State(st): S,
    axum::Extension(v): V,
    Path((profile, agent, trigger)): Path<(String, String, String)>,
    bytes: Bytes,
    pause: bool,
) -> R {
    operator(&v)?;
    let key = TriggerKey {
        profile: profile_id(&profile).ok_or(INVALID)?,
        agent: agent_id(&agent)?,
        trigger: bot_core::id::is_name(&trigger)
            .then_some(trigger)
            .ok_or(INVALID)?,
    };
    // 본문은 정확히 {grant_id, epoch} 둘뿐이다(epoch는 JSON 정수)
    let b = body(&bytes, &["grant_id", "epoch"])?;
    let cas = GrantCas {
        grant_id: identifier(b.get("grant_id").and_then(Value::as_str).ok_or(INVALID)?)?,
        epoch: b
            .get("epoch")
            .and_then(Value::as_i64)
            .filter(|n| *n >= 0)
            .ok_or(INVALID)?,
    };
    let out = st
        .act
        .set_pause(v.principal.clone(), key, cas, pause)
        .await?;
    Ok(ok(&json::preapproval(&out.row), st.leak.as_ref()))
}

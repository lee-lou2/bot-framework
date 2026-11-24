//! «같은 집합 확인» 훅. 이 프로세스가 활성화한 프로필 집합이 저장소의 활성 판본과 같은지를 응답을 내보내기 **직전에** 확인한다.
//! 다르면(다른 프로세스가 집합을 바꿨거나 옛 활성화로 남은 프로세스) 본문 없이 503 `unavailable`이다 — 옛 매핑으로 지도·카드·실행 같은
//! 조회 결과를 내주지 않는다. 변경 라우트(승인·보류 판정)는 핸들러 전에도 확인해 옛 화면에서 판정이 시작되지 않게 한다
//! (쓰기 커밋 자체는 저장소가 따로 지킨다). 정적 화면 자산(`/ui`)은 데이터가 없어 확인하지 않는다: 화면이 열려야 «사용할 수 없음»을 보인다.
//! 인증을 통과하지 못한 요청(401·421·429)은 여기까지 오지 않는다. 훅이 없으면 이 층은 아무 일도 하지 않는다.

use crate::resp::reply;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

pub type FenceFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
/// 참이면 «같은 집합»이라 통과한다. 거짓이거나 시간 안에 끝나지 않으면 닫는다.
pub type Fence = Arc<dyn Fn() -> FenceFuture + Send + Sync>;

/// 훅이 응답하지 않을 때 기다리는 기본 시간. 넘으면 «다르다»로 본다(닫힌 채).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Gate {
    pub check: Fence,
    pub timeout: Duration,
}

impl Gate {
    async fn same(&self) -> bool {
        tokio::time::timeout(self.timeout, (self.check)())
            .await
            .unwrap_or(false)
    }
}

fn unavailable() -> Response {
    reply(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
}

pub(crate) async fn layer(State(g): State<Arc<Gate>>, req: Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/v1/admin/") {
        return next.run(req).await;
    }
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD);
    if mutating && !g.same().await {
        return unavailable();
    }
    let resp = next.run(req).await;
    if g.same().await { resp } else { unavailable() }
}

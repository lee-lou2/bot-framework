//! Active-set fence middleware returning 503 `unavailable` when this process's profile set is stale.
//! Mutating `/v1/admin/*` routes check before and after the handler; reads check before returning; `/ui` assets bypass.

use crate::resp::reply;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

pub type FenceFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
/// Returns true while this process's active set matches the store; false or timeout fails closed.
pub type Fence = Arc<dyn Fn() -> FenceFuture + Send + Sync>;

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

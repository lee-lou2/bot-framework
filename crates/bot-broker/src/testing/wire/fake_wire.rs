//! `RemoteWire` test double wrapping `FakeMcp` to simulate transport-level failures.
use super::{FakeMcp, McpRequest, lock};
use crate::{
    mcp::wire::{RemoteWire, WireFail, WireReply, WireRequest, WireResponse},
    model::ChunkSource,
};
use bot_core::port::{Error, Reply};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Yields scripted chunks in order; a `HANG` sentinel blocks forever.
struct Chunks(VecDeque<Vec<u8>>);

impl ChunkSource for Chunks {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        if self.0.front().is_some_and(|c| c == b"HANG") {
            return Box::pin(std::future::pending());
        }
        let next = self.0.pop_front();
        Box::pin(async move { Ok::<_, Error>(next) })
    }
}

/// Hook invoked with the RPC method right after the fake server handles a request.
pub type AfterRequest = Box<dyn Fn(&str) + Send + Sync>;

pub struct FakeWire {
    pub server: Arc<FakeMcp>,
    /// When true, every request fails with `WireFail::Unreached`.
    pub unreachable: AtomicBool,
    /// RPC method whose response is dropped as `WireFail::Lost` after reaching the server.
    pub lose: Mutex<Option<String>>,
    /// Recorded `(tls, host, port, allow_private)` targets.
    pub targets: Mutex<Vec<(bool, String, u16, bool)>>,
    /// Recorded secret header values sent over the wire.
    pub secret_values: Mutex<Vec<String>>,
    /// Recorded `(rpc_method, timeout)` per request.
    pub timeouts: Mutex<Vec<(String, Option<std::time::Duration>)>>,
    /// Invoked immediately after server handling to simulate mid-call state changes.
    pub after_request: Mutex<Option<AfterRequest>>,
}

impl FakeWire {
    pub fn new(server: Arc<FakeMcp>) -> Self {
        Self {
            server,
            unreachable: AtomicBool::new(false),
            lose: Mutex::new(None),
            targets: Mutex::new(Vec::new()),
            secret_values: Mutex::new(Vec::new()),
            timeouts: Mutex::new(Vec::new()),
            after_request: Mutex::new(None),
        }
    }
}

impl RemoteWire for FakeWire {
    fn send(&self, request: WireRequest) -> WireReply<'_> {
        lock(&self.targets).push((
            request.tls,
            request.host.clone(),
            request.port,
            request.allow_private,
        ));
        lock(&self.secret_values).extend(
            request
                .headers
                .iter()
                .filter(|(_, _, secret)| *secret)
                .map(|(_, value, _)| value.clone()),
        );
        Box::pin(async move {
            if self.unreachable.load(Ordering::SeqCst) {
                return Err(WireFail::Unreached);
            }
            let rpc_request = McpRequest {
                method: match request.method {
                    crate::mcp::wire::WireMethod::Post => "POST".into(),
                    crate::mcp::wire::WireMethod::Delete => "DELETE".into(),
                },
                path: request.path.clone(),
                headers: request
                    .headers
                    .iter()
                    .map(|(n, v, _)| (n.clone(), v.clone()))
                    .collect(),
                body: String::from_utf8_lossy(&request.body).into_owned(),
            };
            let rpc = rpc_request.rpc();
            lock(&self.timeouts).push((rpc.clone(), request.timeout));
            let answer = self.server.handle(rpc_request);
            if let Some(hook) = lock(&self.after_request).as_ref() {
                hook(&rpc);
            }
            if lock(&self.lose).as_deref() == Some(rpc.as_str()) {
                return Err(WireFail::Lost);
            }
            Ok(WireResponse {
                status: answer.status,
                headers: answer.headers,
                body: Box::new(Chunks(answer.chunks.into())),
            })
        })
    }
}

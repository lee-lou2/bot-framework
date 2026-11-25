//! `FakeMcp`를 `RemoteWire`로 감싼 가짜 전송(연결 단계 실패를 흉내 낸다).
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

/// 미리 정한 조각들을 차례로 내주는 본문. 마지막 조각이 `HANG`이면 거기서 영영 멈춘다.
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

/// 요청을 처리한 직후 부르는 함수(인자는 RPC 메서드).
pub type AfterRequest = Box<dyn Fn(&str) + Send + Sync>;

/// `FakeMcp`를 `RemoteWire`로 감싼 가짜 전송. 연결 단계 실패를 흉내 낸다.
pub struct FakeWire {
    pub server: Arc<FakeMcp>,
    /// 켜면 모든 요청이 연결 전에 실패한다(`Unreached`)
    pub unreachable: AtomicBool,
    /// 이 RPC 메서드를 서버가 받은 뒤 응답을 잃는다(`Lost`: 받았는지는 알 수 없다)
    pub lose: Mutex<Option<String>>,
    /// 받은 전송 요청의 (tls, 호스트, 포트, allow_private)
    pub targets: Mutex<Vec<(bool, String, u16, bool)>>,
    /// 받은 요청의 비밀 머리 값들(어디에 실렸는지 검사하려고)
    pub secret_values: Mutex<Vec<String>>,
    /// 서버가 요청을 처리한 직후 부르는 함수(인자는 RPC 메서드): 호출이 진행되는 도중에 세상이 바뀌는 상황을 만든다.
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

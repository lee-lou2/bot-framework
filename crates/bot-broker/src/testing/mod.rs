//! 시험용 가짜. 다른 크레이트도 `testing` 기능으로 쓸 수 있다(운영 빌드에는 넣지 않는다).
//! `MemLedger`는 `CallLedger`가 지켜야 할 불변식을 실행 가능한 형태로 적어 둔 기준 구현이다.

mod exec;
mod fakes;
mod journal;
mod ledger;
mod memory;
mod net;
mod private;
mod replies;
mod routes;
mod sandbox;
mod schedules;
mod turns;
mod wire;
mod world;

pub use crate::effects::EffectChannel;
pub use exec::write_executable;
pub use fakes::{FakeCatalog, FakeEffects, FakeRunner, MemAudit, RecordingTracer};
pub use journal::MemJournal;
pub use ledger::MemLedger;
pub use memory::MemMemory;
pub use net::{End, FakeConnector, FakeResolver, FakeUpstream};
pub use private::MemPrivateLedger;
pub use replies::{MemReplies, Sent};
pub use routes::MemRoutes;
pub use sandbox::{
    Answer, CallCtx, CallFuture, FakeSandbox, FakeSecrets, Recorder, Server, Terminate, serve_mcp,
};
pub use schedules::MemSchedules;
pub use turns::{FakeDelegation, FakeRuns, PlanningRuns, RecordingPark, RecordingTurns};
pub use wire::{Era, FakeMcp, FakeWire, McpReply, McpRequest, McpTool, Probe};
pub use world::{SnapshotBuilder, run_context, snapshot, tool};

use crate::{
    ports::*,
    sys::{Clock, Entropy},
};
use bot_core::{
    command::JsonPayload,
    port::Error,
    profile::{Destination, Tool},
};
use std::{
    future::Future,
    sync::Mutex,
    task::{Context as TaskContext, Poll, Waker},
    time::{Duration, SystemTime},
};

/// 메모리 포트만 기다리는 미래를 끝까지 돌린다(I/O가 필요한 시험은 런타임을 쓴다).
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut cx = TaskContext::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
    panic!("미래가 끝나지 않는다: 메모리 포트가 아닌 것을 기다리고 있다");
}

/// 손으로 돌리는 시계. 초 단위 에포크로 시작한다.
pub struct ManualClock(Mutex<SystemTime>);

impl ManualClock {
    pub fn new(epoch_secs: u64) -> Self {
        Self(Mutex::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs),
        ))
    }
    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
    pub fn set(&self, epoch_secs: u64) {
        *self.0.lock().unwrap() = SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// 호출마다 다른 값을 내는 결정적 난수(첫 호출 `01…`, 다음 `02…`).
#[derive(Default)]
pub struct CountingEntropy(Mutex<u8>);

impl Entropy for CountingEntropy {
    fn fill(&self, buf: &mut [u8]) -> bool {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        buf.fill(*n);
        true
    }
}

/// 인자 글을 그대로 정규화 결과로 쓰고, 해시는 글에서 결정적으로 만든 값이다(암호용이 아니다).
/// 명시적 목적지가 있는 도구는 인자 글을 `수신자|내용`으로 읽는다. 실제 검증기는 `JcsSealer`다.
pub struct FakeSealer;

impl Sealer for FakeSealer {
    fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error> {
        if args.0.is_empty() {
            return Err(Error::Invalid);
        }
        let destination = match &tool.destination {
            Some(Destination::Explicit { .. }) => {
                let (to, body) = args.0.split_once('|').ok_or(Error::Invalid)?;
                Some((to.to_owned(), body.to_owned()))
            }
            _ => None,
        };
        let mut sha256 = [0u8; 32];
        for (i, chunk) in sha256.chunks_mut(8).enumerate() {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (i as u64);
            for b in args.0.bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
            chunk.copy_from_slice(&h.to_be_bytes());
        }
        Ok(Sealed {
            canonical: args.0.clone(),
            sha256,
            destination,
        })
    }
}

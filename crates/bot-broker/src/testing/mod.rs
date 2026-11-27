//! Test doubles exported under the `testing` feature and crate-local `cfg(test)` helpers.

#[cfg(test)]
mod exec;
mod fakes;
mod journal;
mod ledger;
mod memory;
mod net;
#[cfg(test)]
mod private;
#[cfg(test)]
mod replies;
mod routes;
mod sandbox;
#[cfg(test)]
mod schedules;
mod turns;
mod wire;
mod world;

pub use crate::effects::EffectChannel;
#[cfg(test)]
pub use exec::write_executable;
pub use fakes::{FakeCatalog, FakeEffects, FakeRunner, MemAudit, RecordingTracer};
pub use journal::MemJournal;
pub use ledger::MemLedger;
pub use memory::MemMemory;
pub use net::{End, FakeConnector, FakeResolver, FakeUpstream};
#[cfg(test)]
pub use private::MemPrivateLedger;
#[cfg(test)]
pub use replies::{MemReplies, Sent};
pub use routes::MemRoutes;
#[cfg(test)]
pub use sandbox::serve_mcp;
pub use sandbox::{
    Answer, CallCtx, CallFuture, FakeSandbox, FakeSecrets, Recorder, Server, Terminate,
};
#[cfg(test)]
pub use schedules::MemSchedules;
#[cfg(test)]
pub use turns::RecordingTurns;
pub use turns::{FakeDelegation, FakeRuns, PlanningRuns, RecordingPark};
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

pub(super) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Polls an in-memory future to completion without a Tokio runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut cx = TaskContext::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
    panic!("future did not complete: waiting on non-memory port");
}

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

/// Deterministic entropy yielding `01...`, `02...`, etc. on successive calls.
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

/// Non-cryptographic test sealer; splits explicit-destination args on `recipient|body`.
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

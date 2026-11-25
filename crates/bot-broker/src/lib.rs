//! 도구 호출 관문: 모델→도구 경계의 인가·분류·승인·격리·감사.
//!
//! 순수 판정(`scope`·`classify`·`approval`·`recovery`·`egress`·`rate`·`guard`)은 포트에 기대지 않는다.
//! `service`가 포트(`ports`)를 엮어 `bot_core::port::Broker`를 구현한다.
//! 수락 시험: requires_approval_after_private_recall, rejects_stale_approval (`service/tests`).

pub mod approval;
pub mod approver;
pub use bot_core::canon;
pub mod classify;
pub mod effects;
pub mod egress;
pub mod front;
pub mod guard;
pub mod ipc;
pub use bot_core::json;
pub mod keyed;
pub mod lines;
pub mod mcp;
pub mod model;
pub mod ports;
pub mod preapproval;
pub mod private;
pub mod proxy;
pub mod rate;
pub mod recovery;
pub mod resolve;
pub mod rfc3339;
pub mod runner;
pub mod sandbox;
pub mod scope;
pub mod seal;
pub mod service;
#[cfg(unix)]
pub mod srt;
pub mod sys;
pub mod text;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

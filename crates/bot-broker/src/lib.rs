//! 도구 호출 관문: 모델→도구 경계의 인가·분류·승인·격리·감사.
//!
//! 순수 판정(`scope`·`classify`·`approval`·`recovery`·`egress`·`rate`·`guard`)은 포트에 기대지 않는다.
//! `service`가 포트(`ports`)를 엮어 `bot_core::port::Broker`를 구현한다.
//! 수락 시험: requires_approval_after_private_recall, rejects_stale_approval (`service/tests`).
//!
//! **외부 계약**은 아래 `pub mod`뿐이다(`bot-cli`가 조립에 쓴다). `pub(crate) mod`는 크레이트 내부이고, 다른 크레이트의 시험이 쓰는 것은
//! `testing` 기능(`bot_broker::testing`)으로 다시 내보낸다. 새 모듈은 `pub(crate)`로 시작하고 다른 크레이트가 정말 쓸 때만 `pub`으로 올린다.

// 외부 계약
pub mod egress;
pub mod front;
pub mod guard;
pub mod mcp;
pub mod model;
pub mod ports;
pub mod preapproval;
pub mod private;
pub mod proxy;
pub mod rate;
pub mod resolve;
pub mod runner;
pub mod sandbox;
pub mod seal;
pub mod service;
#[cfg(unix)]
pub mod srt;
pub mod sys;

// 크레이트 내부
pub(crate) mod approval;
pub(crate) mod approver;
pub(crate) use bot_core::canon;
pub(crate) mod classify;
pub(crate) mod effects;
pub(crate) use bot_core::json;
pub(crate) mod keyed;
pub(crate) mod lines;
pub(crate) mod recovery;
pub(crate) mod text;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

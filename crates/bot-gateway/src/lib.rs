//! 인증된 접수·민감 DM 분기·원래 대화 회신. 어댑터(CLI·Slack·HTTP)는 같은 순서를 따른다:
//! verify → normalize → durable accept → ack.
//! 수락 시험: routes_private_dm_before_queue(`src/ingress/tests.rs`), replays_http_result(`tests/http.rs`).
pub mod active;
pub mod cli;
pub mod delivery;
pub mod entrance;
pub mod host;
pub mod http;
pub mod identity;
pub mod inbound;
pub mod ingress;
pub mod limits;
pub mod mapping;
pub mod observe;
pub mod reply;
pub mod routes;
pub mod schedule_routes;
pub mod slack;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod trigger;

//! 역할 사이의 데이터와 포트 계약. 구현은 각 담당 크레이트가 제공하고, 이 크레이트는 다른 `bot-*` 크레이트에 의존하지 않는다.
//!
//! 모듈은 네 분류 중 하나다. 새 모듈은 어느 쪽인지 먼저 정한다. 데이터와 포트가 함께 있는 모듈은 큰 쪽에 둔다.
//!
//! | 분류 | 모듈 |
//! |---|---|
//! | 데이터 어휘(타입·값) | `id` · `command` · `state` · `profile` · `identity` · `grant` · `pass` · `mcp` · `install` · `attachments` |
//! | 포트(트레이트) | `port`(핵심) · `ledger` · `intake` · `schedule` · `admin` · `trace` · `sandbox`(모델 VM) |
//! | 어댑터 고유 계약 | `slack` · `cards`(소유자 DM 승인 카드) · `api`(OpenAI 호환 입구) · `cli`(Claude CLI 호출 위치, CLI 입구가 아니다) |
//! | 비트가 같아야 하는 알고리즘 | `cron` · `canon`(RFC 8785) · `json`(엄격한 읽기) · `scope`(인가 교집합) |
//!
//! `tool_env`는 비공개 모듈이라 표에 없다. 도구 환경 이름 검사(`forbidden_env_name`)는 `profile`에서 다시 내보낸다.
//!
//! 같은 말이 다른 뜻으로 쓰이는 곳은 `docs/glossary.md`에 모았다(예: `port::Reply<'a, T>`는 회신이 아니라 비동기 반환형 별칭).
pub mod admin;
pub mod api;
pub mod attachments;
pub mod canon;
pub mod cards;
pub mod cli;
pub mod command;
pub mod cron;
pub mod grant;
pub mod id;
pub mod identity;
pub mod install;
pub mod intake;
pub mod json;
pub mod ledger;
pub mod mcp;
pub mod pass;
pub mod port;
pub mod profile;
pub mod sandbox;
pub mod schedule;
pub mod scope;
pub mod slack;
pub mod state;
mod tool_env;
pub mod trace;

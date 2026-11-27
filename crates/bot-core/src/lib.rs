//! Shared data vocabulary, port traits, and deterministic algorithms across all bot crates.
//!
//! Implementations live in their owning crates; `bot-core` has no `bot-*` dependencies.
//!
//! | Category | Modules |
//! |---|---|
//! | Data vocabulary | `id`, `command`, `state`, `profile`, `identity`, `grant`, `pass`, `mcp`, `install`, `attachments`, `builtin` |
//! | Port traits | `port` (core), `active`, `ledger`, `intake`, `schedule`, `manage`, `admin`, `trace`, `issue`, `sandbox` (model VM) |
//! | Adapter contracts | `slack`, `cards` (owner-DM approval cards), `api` (OpenAI-compatible ingress), `cli` (Claude CLI invocation, not CLI ingress) |
//! | Bit-exact algorithms | `cron`, `canon` (RFC 8785), `json` (strict parser), `scope`, `random`, `redact` |
//!
//! `tool_env` is private; `forbidden_env_name` is re-exported from `profile`.
//! See `docs/glossary.md` for overloaded terms (e.g., `port::Reply<'a, T>` is an async return alias, not a chat reply).
pub mod active;
pub mod admin;
pub mod api;
pub mod attachments;
pub mod builtin;
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
pub mod issue;
pub mod json;
pub mod ledger;
pub mod manage;
pub mod mcp;
pub mod pass;
pub mod port;
pub mod profile;
pub mod random;
pub mod redact;
pub mod sandbox;
pub mod schedule;
pub mod scope;
pub mod slack;
pub mod state;
mod tool_env;
pub mod trace;

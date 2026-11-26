//! `bot` 바이너리의 부품: 설정·호스트 등록부·SecretStore 구현과 명령·역할 조립.
//! 수락 시험: runs_personal_demo, starts_split_roles(`tests/split_roles.rs`).
pub mod chat;
pub mod cli;
pub mod doctor;
pub mod execution;
pub mod host;
pub mod install;
pub mod load;
pub mod lock;
pub mod logging;
pub mod map;
pub mod mcp_sync;
pub mod nonce;
pub mod profile_bundle;
pub mod scaffold;
pub mod secrets;
pub mod serve;
pub mod settings;
pub mod term;
pub mod tool_limits;
pub mod validate;

//! `bot` 바이너리의 부품: 설정·호스트 등록부·SecretStore 구현과 명령·역할 조립.
//! 예정 수락 시험: runs_personal_demo, starts_split_roles.
pub mod chat;
pub mod cli;
pub mod execution;
pub mod host;
pub mod install;
pub mod load;
pub mod lock;
pub mod logging;
pub mod map;
pub mod nonce;
pub mod profile_bundle;
pub mod secrets;
pub mod serve;
pub mod settings;
pub mod term;
pub mod validate;

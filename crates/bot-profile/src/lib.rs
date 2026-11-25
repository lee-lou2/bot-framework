//! 프로필 폴더 탐색·검증·불변 판본.
//!
//! 흐름: `scan`(링크·특수 파일을 거절하고 바이트를 한 번 읽는다) → `layout`(알려진 이름만) →
//! `parse`(텍스트·YAML·스키마) → `assemble`(상속) → `resolve`(참조·충돌) → `convert`(core 스냅샷) → SHA-256 판본.
//! 호스트 등록부 대조(`activate`)와 한꺼번에 교체(`registry`)가 그 위에 있다.
//! 바깥에 내놓는 것: `load`/`Registry`(검증·교체·`ProfileCatalog`), `HostCatalog`, `Limits`, `Report`,
//! `SchemaValidator`(`InputValidator`), `from_bundle`, `Registry::restore`(저장한 묶음에서 옛 판본 복원), `FsProfiles`(`ProfileSource`), `Cron`.
mod activate;
mod assemble;
mod build;
mod convert;
mod frontmatter;
mod hash;
mod hint;
pub mod host;
pub mod issue;
mod layout;
pub mod limits;
mod model;
mod names;
mod openai;
mod parse;
mod profile;
pub mod registry;
mod resolve;
mod restore;
mod scan;
pub mod schema;
mod schemas;
mod snapshot;
pub mod source;
#[cfg(test)]
mod testrand;
mod text;
mod tools;
mod triggers;
pub mod validator;
mod vocab;
mod yaml;

pub use bot_core::cron::Cron;
pub use host::{HostCatalog, SlackWorkspace};
pub use issue::{Issue, Severity};
pub use limits::{KNOWLEDGE_MAX_BYTES, KNOWLEDGE_MAX_DOCS, Limits};
pub use registry::{ProfileSet, Registry, Report, load};
pub use restore::{Problem, ProblemKind, RestoreFailure, Restored, read_bundle};
pub use schema::{Schema, Violation};
pub use snapshot::{from_bundle, revision_of};
pub use source::FsProfiles;
pub use validator::SchemaValidator;

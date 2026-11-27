//! Profile directory discovery, strict validation, immutable snapshots, and atomic registry reloads.
//!
//! Pipeline: `scan` -> `layout` -> `parse` -> `assemble` -> `resolve` -> `convert` -> SHA-256 revision,
//! followed by host catalog validation (`activate`) and atomic swaps (`registry`).
mod activate;
mod approval;
mod assemble;
mod build;
mod convert;
mod frontmatter;
mod hash;
mod hint;
mod host;
pub mod issue;
mod layout;
mod limits;
pub mod locate;
pub mod lock;
mod model;
mod names;
mod openai;
mod parse;
mod profile;
mod refs;
mod registry;
mod resolve;
mod restore;
mod scan;
mod schema;
mod schemas;
mod snapshot;
mod source;
#[cfg(test)]
mod testrand;
mod text;
mod tools;
mod triggers;
mod validator;
mod vocab;
mod yaml;

pub use bot_core::cron::Cron;
pub use host::{HostCatalog, SlackWorkspace};
pub use issue::{Issue, Severity};
pub use limits::{KNOWLEDGE_MAX_BYTES, KNOWLEDGE_MAX_DOCS, Limits, SKILL_MAX_BYTES};
pub use registry::{ProfileSet, Registry, Report, load};
pub use restore::{
    Problem, ProblemKind, RestoreFailure, Restored, read_bundle, read_legacy_bundle,
};
pub use schema::{Schema, Violation};
pub use snapshot::{from_bundle, revision_of};
pub use source::FsProfiles;
pub use validator::SchemaValidator;
pub use vocab::Problem as SchemaProblem;
pub use yaml::parse as parse_yaml;

//! Live skill and MCP manager implementing `bot_core::manage::ProfileManager` for super-admins.
//!
//! File mutations are wrapped in `DiskTxn` so any validation, install, or DB failure rolls back disk
//! state; on success `bot_active` updates via `replace_live` and in-process state swaps atomically.

mod activate;
mod inspect;
mod mcp;
mod mcp_remove;
mod mcp_stdio;
mod skill;
pub(crate) mod yaml_edit;

use super::{decide::LiveInstalled, restore, runner::LiveRunner, watched::Watched};
use crate::{host::HostFile, secrets::admin::SecretAdmin, settings::Settings};
use bot_core::{
    builtin::{
        MCP_ADD_TOOL, MCP_REMOVE_TOOL, SKILL_CREATE_TOOL, SKILL_DELETE_TOOL, SKILL_INSPECT_TOOL,
        SKILL_UPDATE_TOOL,
    },
    id::{ProfileId, Revision},
    manage::{ManageCall, ManageError, ManageOutcome, ProfileManager},
    port::Reply,
    profile::Snapshot,
};
use bot_profile::Registry;
use bot_store::Store;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

pub(super) struct LiveManagerParts {
    pub settings: Settings,
    pub profiles_root: PathBuf,
    pub host_path: Option<PathBuf>,
    pub host: HostFile,
    pub registry: Arc<Registry>,
    pub raw_store: Arc<Store>,
    pub live_revision: Arc<RwLock<Revision>>,
    pub live_installed: LiveInstalled,
    pub live_runner: LiveRunner,
    pub secrets_admin: Arc<SecretAdmin>,
    pub watched: Arc<Watched>,
    pub uid: u32,
}

pub(super) struct LiveManager {
    pub(super) settings: Settings,
    pub(super) profiles_root: PathBuf,
    pub(super) host_path: Option<PathBuf>,
    pub(super) host: Mutex<HostFile>,
    pub(super) registry: Arc<Registry>,
    pub(super) raw_store: Arc<Store>,
    pub(super) live_revision: Arc<RwLock<Revision>>,
    pub(super) live_installed: LiveInstalled,
    pub(super) live_runner: LiveRunner,
    pub(super) secrets_admin: Arc<SecretAdmin>,
    pub(super) watched: Arc<Watched>,
    pub(super) uid: u32,
    /// Retains strong references to displaced snapshots so in-flight `Registry::lookup` calls stay valid.
    pub(super) held_snapshots: Mutex<Vec<Arc<Snapshot>>>,
    pub(super) held_restored: Mutex<Vec<restore::Prepared>>,
    /// Serializes disk edits, validation, and activation commits.
    pub(super) tx_lock: tokio::sync::Mutex<()>,
}

impl LiveManager {
    pub(super) fn new(parts: LiveManagerParts) -> Self {
        Self {
            settings: parts.settings,
            profiles_root: parts.profiles_root,
            host_path: parts.host_path,
            host: Mutex::new(parts.host),
            registry: parts.registry,
            raw_store: parts.raw_store,
            live_revision: parts.live_revision,
            live_installed: parts.live_installed,
            live_runner: parts.live_runner,
            secrets_admin: parts.secrets_admin,
            watched: parts.watched,
            uid: parts.uid,
            held_snapshots: Mutex::new(Vec::new()),
            held_restored: Mutex::new(Vec::new()),
            tx_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub(super) fn profile_dir(&self, profile: &ProfileId) -> PathBuf {
        self.profiles_root.join(&profile.0)
    }

    pub(super) fn current_snapshot(
        &self,
        profile: &ProfileId,
    ) -> Result<Arc<Snapshot>, ManageError> {
        self.registry
            .set()
            .profile(profile)
            .cloned()
            .ok_or_else(|| ManageError::Rejected {
                code: "unknown_profile",
                message: "active profile not found".into(),
            })
    }

    async fn dispatch(&self, call: ManageCall) -> Result<ManageOutcome, ManageError> {
        match call.tool.as_str() {
            SKILL_INSPECT_TOOL => self.inspect(&call).await,
            SKILL_CREATE_TOOL => self.create_skill(&call).await,
            SKILL_UPDATE_TOOL => self.update_skill(&call).await,
            SKILL_DELETE_TOOL => self.delete_skill(&call).await,
            MCP_ADD_TOOL => self.add_mcp(&call).await,
            MCP_REMOVE_TOOL => self.remove_mcp(&call).await,
            _ => Err(ManageError::Rejected {
                code: "unknown_tool",
                message: "unsupported management tool".into(),
            }),
        }
    }
}

impl ProfileManager for LiveManager {
    fn execute(&self, call: ManageCall) -> Reply<'_, Result<ManageOutcome, ManageError>> {
        Box::pin(async move { Ok(self.dispatch(call).await) })
    }
}

//! 슈퍼 관리자 전용 런타임 스킬·MCP 관리기(`bot_core::manage::ProfileManager` 구현).
//!
//! - `skill.inspect` · `skill.create` · `skill.update` · `skill.delete` · `mcp.add` · `mcp.remove`를 처리한다.
//! - 모든 파일 변경은 `DiskTxn`으로 묶여 검증·설치·DB 커밋 중 하나라도 실패하면 원래 상태로 되돌린다.
//! - 성공하면 DB `bot_active`를 `replace_live`로 갱신하고 같은 프로세스의 `Registry`·`live_revision`·`McpRunner`·`SrtSandbox`·`Decisions`를 즉시 교체한다.
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
    /// 교체된 옛 스냅숏의 강한 참조를 유지해 진행 중인 실행의 `lookup`이 깨지지 않게 한다.
    pub(super) held_snapshots: Mutex<Vec<Arc<Snapshot>>>,
    pub(super) held_restored: Mutex<Vec<restore::Prepared>>,
    /// 디스크 편집·검증·활성화를 직렬화한다.
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
                message: "활성 프로필을 찾지 못했어요".into(),
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
                message: "지원하지 않는 관리 도구예요".into(),
            }),
        }
    }
}

impl ProfileManager for LiveManager {
    fn execute(&self, call: ManageCall) -> Reply<'_, Result<ManageOutcome, ManageError>> {
        Box::pin(async move { Ok(self.dispatch(call).await) })
    }
}

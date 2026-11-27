//! Alternate `Rig` launch modes: FakeSlack wiring, custom store config, and real Apple VM adapter.
use super::{PathBuf, Rig, Role, Schema, model_key_store};
use bot_broker::testing::FakeSecrets;
use std::sync::Arc;

/// Apple VM settings for production-shaped tests (digest-pinned image, host-only network, worker instance).
pub struct Vm {
    pub image: String,
    pub network: String,
    pub instance: String,
    pub relay_bind: Option<String>,
}

pub fn slack_key_store() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[
        ("personal", "model-key", "provider-key-for-personal"),
        ("personal", "bot-token", "xoxb-test-token"),
        ("personal", "app-token", "xapp-test-token"),
    ]))
}

impl Rig {
    pub async fn start_slack(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        gateway: &str,
        roles: &[Role],
        slack_base: &str,
    ) -> Rig {
        let mut rig = Self::launch(
            schema,
            claude,
            profile_yaml,
            &[],
            gateway,
            roles,
            slack_key_store(),
            "",
            Some(slack_base),
        )
        .await;
        rig.until("serve did not publish an active revision", 30, |rig| {
            !rig.schema.active().is_empty()
        })
        .await;
        rig
    }

    pub async fn start_with_store(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        gateway: &str,
        roles: &[Role],
        store_config: bot_store::Config,
    ) -> Rig {
        let mut rig = Self::launch_inner(
            schema,
            claude,
            profile_yaml,
            &[],
            gateway,
            roles,
            model_key_store(),
            "",
            None,
            None,
            Some(store_config),
        )
        .await;
        rig.until("serve did not publish an active revision", 30, |rig| {
            !rig.schema.active().is_empty()
        })
        .await;
        rig
    }

    /// Launches `serve` using the real Apple VM adapter with a fake upstream and waits for activation.
    pub async fn start_vm(
        schema: Schema,
        profile_yaml: &str,
        gateway: &str,
        roles: &[Role],
        vm: Vm,
    ) -> Rig {
        let mut rig = Self::launch_inner(
            schema,
            PathBuf::from("/nonexistent/claude"),
            profile_yaml,
            &[],
            gateway,
            roles,
            model_key_store(),
            "",
            None,
            Some(vm),
            None,
        )
        .await;
        rig.until("serve did not publish an active revision", 180, |rig| {
            !rig.schema.active().is_empty()
        })
        .await;
        rig
    }

    /// Like `start_vm` without waiting for activation (use `failure` to inspect startup errors).
    pub async fn launch_vm(
        schema: Schema,
        profile_yaml: &str,
        gateway: &str,
        roles: &[Role],
        vm: Vm,
    ) -> Rig {
        Self::launch_inner(
            schema,
            PathBuf::from("/nonexistent/claude"),
            profile_yaml,
            &[],
            gateway,
            roles,
            model_key_store(),
            "",
            None,
            Some(vm),
            None,
        )
        .await
    }
}

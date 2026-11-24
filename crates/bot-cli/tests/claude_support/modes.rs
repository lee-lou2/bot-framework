//! `Rig`를 다른 모양으로 띄우는 방법들: 가짜 Slack을 붙이거나, 저장소 설정을 바꾸거나, 합성 샌드박스 대신 실제 Apple VM 어댑터(운영 조립)로 돌린다.
use super::{PathBuf, Rig, Role, Schema, model_key_store};
use bot_broker::testing::FakeSecrets;
use std::sync::Arc;

/// 운영 모양(실제 Apple VM)으로 돌릴 때의 모델 VM 설정: 다이제스트로 고정한 이미지·호스트 전용 망·이 워커의 이름
pub struct Vm {
    pub image: String,
    pub network: String,
    pub instance: String,
    /// 호스트 설정의 `sandbox.apple.relay_bind`(없으면 닻이 확인한 게이트웨이 주소)
    pub relay_bind: Option<String>,
}

/// 모델 서비스 키에 Slack 토큰(`bot-token`·`app-token`)을 더한 보관소(가짜 Slack을 붙이는 시험용)
pub fn slack_key_store() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[
        ("personal", "model-key", "provider-key-for-personal"),
        ("personal", "bot-token", "xoxb-test-token"),
        ("personal", "app-token", "xapp-test-token"),
    ]))
}

impl Rig {
    /// 가짜 Slack(`slack_base`)을 붙여 `serve`를 돌린다. 보관소에는 Slack 토큰이 있다(`slack_key_store`). 활성 판본이 게시될 때까지 기다린다.
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
        rig.until(
            "serve가 활성 판본을 게시하지 않았어요",
            30,
            |rig| !rig.schema.active().is_empty(),
        )
        .await;
        rig
    }

    /// `start`와 같되 저장소 설정을 바꾼다(예: 시도 한도). 활성 판본이 게시될 때까지 기다린다.
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
        rig.until(
            "serve가 활성 판본을 게시하지 않았어요",
            30,
            |rig| !rig.schema.active().is_empty(),
        )
        .await;
        rig
    }

    /// 운영 모양: 모델 CLI를 합성 샌드박스가 아니라 실제 Apple VM 어댑터(닻 → 프런트 → 릴레이)로 돌린다. 상류는 가짜다. 활성 판본이 게시될 때까지 기다린다
    /// (VM을 띄우는 시간까지 넉넉히).
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
        rig.until(
            "serve가 활성 판본을 게시하지 않았어요",
            180,
            |rig| !rig.schema.active().is_empty(),
        )
        .await;
        rig
    }

    /// `start_vm`과 같되 활성 판본을 기다리지 않는다(시작이 실패하는 경우를 볼 때는 `failure`).
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

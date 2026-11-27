//! 운영자가 프로필을 고쳐 `--activate`로 다시 배포한 상황: 같은 DB·호스트 설정·가짜 상류·가짜 Slack으로 `serve`를 새 판본으로 다시 띄운다.
use super::{ClaudeParts, Overrides, Rig, Role, Settings};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

impl Rig {
    /// 이 장치의 가짜 상류·합성 샌드박스·보관소·가짜 Slack을 넣는 대체(새로 띄우는 serve가 같은 것을 쓴다)
    fn overrides(&self, stop: CancellationToken) -> Overrides {
        Overrides {
            claude: self.launch.clone().map(|launch| ClaudeParts {
                launch,
                upstream: self.upstream.clone(),
                workspace: self.dir.path().join("models"),
                park: Some(self.park.clone()),
            }),
            upstream: self.launch.is_none().then(|| self.upstream.clone() as _),
            secrets: Some(self.secrets.clone()),
            secret_admin: None,
            stop: Some(stop),
            slack_base: self.slack_base.clone(),
            store_config: self.store_config.clone(),
        }
    }

    /// 도는 serve를 멈추고(오류 없이 끝나야 한다) 프로필(`personal/profile.yaml`)을 바꾼 뒤 같은 DB에 `--activate`로 새 serve를 띄운다.
    /// 새 활성 판본이 게시될 때까지 기다린다. 대기 중이던 승인·run·예약은 DB에 그대로 남아 새 serve가 이어받는다.
    pub async fn reactivate(&mut self, profile_yaml: &str, roles: &[Role]) {
        let before = self.schema.active();
        self.stop.cancel();
        let old = self.task.take().expect("serve가 이미 끝났어요");
        let ended = tokio::time::timeout(Duration::from_secs(30), old)
            .await
            .expect("옛 serve가 멈추지 않았어요")
            .unwrap();
        assert_eq!(ended, Ok(()), "옛 serve가 오류로 끝났어요");
        let profile = self.dir.path().join("profiles/personal/profile.yaml");
        std::fs::write(profile, profile_yaml).unwrap();
        let settings = Settings::read(&|name| {
            self.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let stop = CancellationToken::new();
        let overrides = self.overrides(stop.clone());
        let roles = roles.to_vec();
        self.stop = stop;
        self.task = Some(tokio::spawn(async move {
            bot_cli::serve::run_with(&settings, &roles, true, overrides).await
        }));
        self.until("새 판본이 게시되지 않았어요", 30, |rig| {
            rig.schema.active() != before
        })
        .await;
    }

    /// 종료 신호(SIGTERM과 같은 경로의 `stop` 토큰)를 보내고 serve가 정리를 마치고 끝나기를 기다린다. 끝난 뒤에도 `Rig`는 DB·상류를 그대로 쓴다.
    pub async fn signal_stop(&mut self) -> Result<(), String> {
        self.stop.cancel();
        let task = self.task.take().expect("serve가 이미 끝났어요");
        tokio::time::timeout(Duration::from_secs(60), task)
            .await
            .expect("serve가 정리를 마치지 못했어요")
            .unwrap()
    }

    /// 도는 serve를 멈추고(오류 없이 끝나야 한다) 프로필을 바꿔 **`--activate` 없이** 새 serve를 시작한다 — 다른 판본이 이미 게시돼 있으므로 시작은 거절돼야 한다.
    /// 그 오류를 돌려준다(시작에 성공했으면 실패한다). 거절된 serve는 띄웠던 것을 모두 치우고 끝났어야 한다.
    pub async fn fail_to_start(&mut self, profile_yaml: &str, roles: &[Role]) -> String {
        self.stop.cancel();
        let old = self.task.take().expect("serve가 이미 끝났어요");
        let ended = tokio::time::timeout(Duration::from_secs(60), old)
            .await
            .expect("옛 serve가 멈추지 않았어요")
            .unwrap();
        assert_eq!(ended, Ok(()), "옛 serve가 오류로 끝났어요");
        let profile = self.dir.path().join("profiles/personal/profile.yaml");
        std::fs::write(profile, profile_yaml).unwrap();
        let settings = Settings::read(&|name| {
            self.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let overrides = self.overrides(CancellationToken::new());
        let started = tokio::time::timeout(
            Duration::from_secs(180),
            bot_cli::serve::run_with(&settings, roles, false, overrides),
        )
        .await
        .expect("serve가 끝나지 않았어요");
        started.expect_err("serve가 시작에 성공했어요")
    }

    /// 도는 serve를 멈추고(오류 없이 끝나야 한다) **같은 프로필·같은 활성 판본**으로 새 serve를 띄운다(`--activate` 없이 합류한다). 이미 멈춘 serve여도 된다.
    /// 종료 신호로 돌려준 run을 새 serve가 이어받는 것을 볼 때 쓴다.
    pub async fn restart_same(&mut self, roles: &[Role]) {
        if let Some(old) = self.task.take() {
            self.stop.cancel();
            let ended = tokio::time::timeout(Duration::from_secs(60), old)
                .await
                .expect("옛 serve가 멈추지 않았어요")
                .unwrap();
            assert_eq!(ended, Ok(()), "옛 serve가 오류로 끝났어요");
        }
        let settings = Settings::read(&|name| {
            self.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let stop = CancellationToken::new();
        let overrides = self.overrides(stop.clone());
        let roles = roles.to_vec();
        self.stop = stop;
        self.task = Some(tokio::spawn(async move {
            bot_cli::serve::run_with(&settings, &roles, false, overrides).await
        }));
    }
}

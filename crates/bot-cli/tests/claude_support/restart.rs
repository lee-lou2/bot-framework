//! Restart and reactivation helpers for `Rig` sharing the same DB, host config, FakeUpstream, and FakeSlack.
use super::{ClaudeParts, Overrides, Rig, Role, Settings};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

impl Rig {
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
            telegram_base: None,
            store_config: self.store_config.clone(),
        }
    }

    /// Stops serve cleanly, rewrites `personal/profile.yaml`, restarts with `--activate`, and waits for the new revision.
    pub async fn reactivate(&mut self, profile_yaml: &str, roles: &[Role]) {
        let before = self.schema.active();
        self.stop.cancel();
        let old = self.task.take().expect("serve already exited");
        let ended = tokio::time::timeout(Duration::from_secs(30), old)
            .await
            .expect("old serve did not stop")
            .unwrap();
        assert_eq!(ended, Ok(()), "old serve exited with error");
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
        self.until("new revision was not published", 30, |rig| {
            rig.schema.active() != before
        })
        .await;
    }

    /// Cancels the stop token (same path as SIGTERM) and awaits clean shutdown while keeping DB and upstream intact.
    pub async fn signal_stop(&mut self) -> Result<(), String> {
        self.stop.cancel();
        let task = self.task.take().expect("serve already exited");
        tokio::time::timeout(Duration::from_secs(60), task)
            .await
            .expect("serve did not finish shutdown")
            .unwrap()
    }

    /// Stops serve, rewrites `personal/profile.yaml`, and starts without `--activate`; expects startup rejection.
    pub async fn fail_to_start(&mut self, profile_yaml: &str, roles: &[Role]) -> String {
        self.stop.cancel();
        let old = self.task.take().expect("serve already exited");
        let ended = tokio::time::timeout(Duration::from_secs(60), old)
            .await
            .expect("old serve did not stop")
            .unwrap();
        assert_eq!(ended, Ok(()), "old serve exited with error");
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
        .expect("serve did not finish");
        started.expect_err("serve unexpectedly succeeded")
    }

    /// Restarts serve on the same profile and active revision without `--activate`.
    pub async fn restart_same(&mut self, roles: &[Role]) {
        if let Some(old) = self.task.take() {
            self.stop.cancel();
            let ended = tokio::time::timeout(Duration::from_secs(60), old)
                .await
                .expect("old serve did not stop")
                .unwrap();
            assert_eq!(ended, Ok(()), "old serve exited with error");
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

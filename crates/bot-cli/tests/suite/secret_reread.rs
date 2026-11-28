//! Admin-only process secret re-read test: periodic re-reading of declared secret names updates
//! the leak redactor so keys rotated after startup are blocked from Admin HTTP responses.
use crate::process_support::*;
use bot_cli::{cli::Role, serve::Overrides, settings::Settings};
use bot_core::{
    command::AuditEntry,
    id::{ProfileId, SecretName},
    port::{AuditLog, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use bot_store::{Config, Store};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const OLD_VALUE: &str = "inbox-key-OLD-71c3d9ab0123456789abcdef0123";
const NEW_VALUE: &str = "inbox-key-NEW-5be8029f0123456789abcdef0123";

struct Rotating(Arc<Mutex<String>>);

struct Bound(Arc<Mutex<String>>);

impl SecretStore for Rotating {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let value = self.0.clone();
        Box::pin(async move { Ok(Box::new(Bound(value)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Bound {
    fn resolve(&self, _: SecretName) -> Reply<'_, SecretValue> {
        let value = self.0.lock().unwrap().clone().into_bytes();
        Box::pin(async move { Ok(SecretValue(value)) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_only_process_masks_a_key_rotated_after_it_started_within_the_reread_period() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    with_inbox(&profiles);
    let port = free_port();
    let admin_port = free_port();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            &format!(
                r#", "http": {{"listen": "127.0.0.1:{port}", "endpoints": {{"inbox": {{"issuer": "i", "audience": "a", "key": "inbox-key"}}}}}}"#
            ),
            &format!(r#", "admin": {{"bind": "127.0.0.1:{admin_port}"}}"#),
        ),
    )
    .unwrap();
    let token_path = dir.path().join("admin.token");
    let (profiles_path, host_path, url) = (
        profiles.display().to_string(),
        host.display().to_string(),
        schema.url(),
    );
    let token_arg = token_path.display().to_string();
    let settings = Settings::read(&|name| match name {
        "BOT_PROFILES" => Some(profiles_path.clone()),
        "BOT_HOST_CONFIG" => Some(host_path.clone()),
        "BOT_DATABASE_URL" => Some(url.clone()),
        "BOT_ADMIN_TOKEN_FILE" => Some(token_arg.clone()),
        "BOT_SECRETS_REREAD_SECS" => Some("1".to_owned()),
        _ => None,
    })
    .unwrap();
    let shared = Arc::new(Mutex::new(OLD_VALUE.to_owned()));
    let stop = CancellationToken::new();
    let overrides = Overrides {
        claude: None,
        secrets: Some(Arc::new(Rotating(shared.clone()))),
        secret_admin: None,
        stop: Some(stop.clone()),
        slack_base: None,
        telegram_base: None,
        upstream: None,
        store_config: None,
    };
    let task = tokio::spawn(async move {
        bot_cli::serve::run_with(&settings, &[Role::Admin], false, overrides).await
    });
    let addr = format!("127.0.0.1:{admin_port}");
    wait_for("Admin UI did not open", || {
        !schema.active().is_empty()
            && std::fs::read_to_string(&token_path).is_ok_and(|text| !text.trim().is_empty())
            && std::net::TcpStream::connect(&addr).is_ok()
    });
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_owned();

    // Control: before rotation is observed, `NEW_VALUE` in the store is not yet in the redactor.
    let store = Store::connect(&schema.url(), Config::default())
        .await
        .unwrap();
    store
        .append(AuditEntry {
            profile: ProfileId("personal".into()),
            actor: "local:owner".into(),
            action: "memo".into(),
            target: format!("note {NEW_VALUE} end"),
            args_sha256: None,
            verdict: "allow".into(),
        })
        .await
        .unwrap();
    let (status, body) = http_get(&addr, "/v1/admin/audit", Some(&token));
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(NEW_VALUE),
        "control: unknown value should pass through before rotation — {body}"
    );

    *shared.lock().unwrap() = NEW_VALUE.to_owned();
    let mut blocked = None;
    for _ in 0..60 {
        let (status, body) = http_get(&addr, "/v1/admin/audit", Some(&token));
        if status != 200 {
            blocked = Some((status, body));
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let (status, body) =
        blocked.expect("rotated secret was not blocked within the re-read interval");
    assert_eq!(status, 500, "{body}");
    assert!(
        !body.contains(NEW_VALUE),
        "blocked response must not contain the secret: {body}"
    );
    assert_eq!(http_get(&addr, "/v1/admin/me", Some(&token)).0, 200);

    stop.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("serve did not exit")
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
}

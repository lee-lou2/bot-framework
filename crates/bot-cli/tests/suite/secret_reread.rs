//! 역할을 나눠 띄운 관리 프로세스(실제 `serve` 조립·팀 시험 DB·관리 화면 HTTP): 모델 키·도구 자격은 다른 프로세스가 읽으므로 이 프로세스는 시작 뒤에 교체된 새 값을
//! 스스로 읽을 일이 없다. 선언된 비밀 이름을 주기적으로 다시 읽어 가림 목록이 따라잡으면, 그 값이 저장소를 거쳐 관리 응답에 섞여 들어와도 응답을 내보내지 않는다.
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

/// 값을 바꿀 수 있는 보관소(실행 중인 키 교체를 흉내 낸다). 어떤 이름을 읽어도 지금 값을 준다.
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
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    // 프로필이 HTTP 입구 키(`inbox-key`)를 선언한 호스트: 관리 프로세스가 «선언된 이름»으로 아는 비밀이다
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
    // 다시 읽는 주기를 1초로 줄인다(운영 기본은 60초)
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
        upstream: None,
        store_config: None,
    };
    // 관리 역할만(게이트웨이·워커는 다른 프로세스라고 본다)
    let task = tokio::spawn(async move {
        bot_cli::serve::run_with(&settings, &[Role::Admin], false, overrides).await
    });
    let addr = format!("127.0.0.1:{admin_port}");
    wait_for("관리 화면이 열리지 않았어요", || {
        !schema.active().is_empty()
            && std::fs::read_to_string(&token_path).is_ok_and(|text| !text.trim().is_empty())
            && std::net::TcpStream::connect(&addr).is_ok()
    });
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_owned();

    // 다른 프로세스가 새 키를 흘려 저장소에 감사 기록 하나가 생겼다고 하자(이 프로세스는 아직 새 키를 모른다)
    let store = Store::connect(&schema.url(), Config::default())
        .await
        .unwrap();
    store
        .append(AuditEntry {
            profile: ProfileId("personal".into()),
            actor: "local:owner".into(),
            action: "memo".into(),
            target: format!("노트 {NEW_VALUE} 끝"),
            args_sha256: None,
            verdict: "allow".into(),
        })
        .await
        .unwrap();
    let (status, body) = http_get(&addr, "/v1/admin/audit", Some(&token));
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(NEW_VALUE),
        "대조: 아직 모르는 값은 응답에 그대로 나간다 — {body}"
    );

    // 운영자가 키를 교체했다. 이 프로세스는 그 값을 읽어 쓸 일이 없지만 주기 안에 다시 읽어 목록에 더한다
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
        blocked.expect("다시 읽기 주기가 지나도 새 값이 섞인 응답을 막지 못했어요");
    assert_eq!(status, 500, "{body}");
    assert!(
        !body.contains(NEW_VALUE),
        "막은 응답에도 값이 없어야 해요: {body}"
    );
    // 값이 없는 응답은 그대로 나간다
    assert_eq!(http_get(&addr, "/v1/admin/me", Some(&token)).0, 200);

    stop.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("serve가 끝나지 않았어요")
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
}

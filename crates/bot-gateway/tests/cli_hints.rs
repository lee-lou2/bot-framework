//! `bot chat`이 입구가 없다고 돌아올 때(M6): 이 OS 사용자가 쓸 수 있는 프로필을 알려 주고 고치는 곳(호스트 설정 `gateway.cli.accounts`)을 말한다.
//! 와이어의 `code`는 그대로(`no_entrance`·`profile_required`)이고 `message`만 길어진다. 다른 사용자의 프로필 이름은 싣지 않는다.
use bot_core::profile::Adapter;
use bot_gateway::{
    cli::{
        CliGateway,
        client::{self, Chat, Outcome},
        socket,
    },
    host::HostConfig,
    ingress::Ingress,
    testing::*,
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

struct Rig {
    socket: PathBuf,
    stop: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn desk(profile_name: &str, account: &str) -> Fixture {
    profile(profile_name)
        .binding(
            "desk",
            Adapter::Cli {
                account: account.into(),
            },
            "main",
        )
        .member(&format!("local:{account}"), &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
}

/// 계정 별칭마다 접속 uid를 따로 정한다(`mine`은 접속한 사용자, `other`는 다른 OS 사용자).
async fn rig(accounts: &[(&str, bool)], profiles: Vec<Fixture>) -> Rig {
    let me = socket::current_uid().unwrap();
    let mut map = serde_json::Map::new();
    for (name, is_mine) in accounts {
        map.insert(
            (*name).into(),
            json!({"uid": if *is_mine { me } else { me + 1 }}),
        );
    }
    let host = HostConfig::from_value(json!({"cli": {"accounts": map}})).unwrap();
    let store = MemoryStore::new();
    let catalog = TestCatalog::new(profiles.into_iter().map(Fixture::build));
    let ingress = Ingress::new(catalog, Arc::new(host), store.clone(), MemoryPrivate::new());
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("run").join(socket::FILE_NAME);
    let listener = socket::bind(&path, me).unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(Arc::new(CliGateway::new(Arc::new(ingress), store)).serve(listener, stop.clone()));
    Rig {
        socket: path,
        stop,
        _dir: dir,
    }
}

fn chat(profile: Option<&str>) -> Chat {
    Chat {
        profile: profile.map(str::to_owned),
        request: "r1".into(),
        thread: None,
        text: "안녕".into(),
        wait: true,
    }
}

async fn failure(rig: &Rig, request: &Chat) -> (String, String) {
    match client::chat(&rig.socket, request, |_| {}).await.unwrap() {
        Outcome::Failed { code, message, .. } => (code, message),
        other => panic!("실패를 기대했는데 {other:?}"),
    }
}

#[tokio::test]
async fn a_profile_the_user_cannot_reach_names_the_profiles_the_user_can() {
    let rig = rig(
        &[("owner", true), ("third", true), ("second", false)],
        vec![
            desk("personal", "owner"),
            desk("work", "third"),
            desk("other", "second"),
        ],
    )
    .await;
    // 없는 프로필
    let (code, message) = failure(&rig, &chat(Some("nosuch"))).await;
    assert_eq!(code, "no_entrance");
    assert!(
        message.contains("프로필 `nosuch`에는 이 사용자에게 연결된 입구가 없어요"),
        "{message}"
    );
    assert!(
        message.contains("쓸 수 있는 프로필: personal, work"),
        "{message}"
    );
    assert!(message.contains("--profile personal"), "{message}");
    // 있지만 다른 OS 사용자의 프로필: 같은 문구이고, 다른 사용자의 프로필 이름은 목록에 없다
    let (code, message) = failure(&rig, &chat(Some("other"))).await;
    assert_eq!(code, "no_entrance");
    assert!(
        message.contains("쓸 수 있는 프로필: personal, work"),
        "{message}"
    );
    assert!(
        !message.contains("other,") && !message.ends_with("other"),
        "{message}"
    );
    // 프로필을 안 골랐는데 여러 개: 고를 수 있는 이름을 보여 준다
    let (code, message) = failure(&rig, &chat(None)).await;
    assert_eq!(code, "profile_required");
    assert!(message.contains("personal, work"), "{message}");
    assert!(message.contains("--profile"), "{message}");
    assert!(!message.contains("other"), "{message}");
}

#[tokio::test]
async fn a_user_with_no_entrance_at_all_is_told_where_the_account_is_connected() {
    // 모든 계정이 다른 OS 사용자에게 연결돼 있다
    let rig = rig(&[("owner", false)], vec![desk("personal", "owner")]).await;
    for request in [chat(None), chat(Some("personal"))] {
        let (code, message) = failure(&rig, &request).await;
        assert_eq!(code, "no_entrance");
        assert!(
            message.contains("이 사용자에게 연결된 입구가 없어요"),
            "{message}"
        );
        assert!(
            message.contains("`account`") && message.contains("`gateway.cli.accounts`"),
            "{message}"
        );
        assert!(message.contains("OS 사용자"), "{message}");
        assert!(
            !message.contains("personal,"),
            "프로필 이름을 늘어놓지 않는다: {message}"
        );
    }
    // 결과 조회도 같은 문구다
    let outcome = client::result(&rig.socket, Some("nosuch"), "r1", false, |_| {})
        .await
        .unwrap();
    let Outcome::Failed { code, message, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(code, "no_entrance");
    assert!(message.contains("`gateway.cli.accounts`"), "{message}");
}

#[tokio::test]
async fn the_one_reachable_profile_is_still_picked_without_a_choice() {
    let rig = rig(
        &[("owner", true), ("second", false)],
        vec![desk("personal", "owner"), desk("other", "second")],
    )
    .await;
    // 입구가 하나뿐이면 고르지 않아도 그 프로필로 간다(문구가 길어져도 이 동작은 그대로다). 요청이 없으니 `not_found`까지 간다.
    let outcome = client::result(&rig.socket, None, "r1", false, |_| {})
        .await
        .unwrap();
    let Outcome::Failed { code, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(code, "not_found");
}

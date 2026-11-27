//! CLI entrance error hints: reachable profiles and `gateway.cli.accounts` guidance without leaking other users' profiles.
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

/// Maps each account alias to the caller's uid (`true`) or another OS user (`false`).
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
        text: "hello".into(),
        wait: true,
    }
}

async fn failure(rig: &Rig, request: &Chat) -> (String, String) {
    match client::chat(&rig.socket, request, |_| {}).await.unwrap() {
        Outcome::Failed { code, message, .. } => (code, message),
        other => panic!("expected failure, got {other:?}"),
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
    let (code, message) = failure(&rig, &chat(Some("nosuch"))).await;
    assert_eq!(code, "no_entrance");
    assert!(
        message.contains("No entrance is mapped to this user for profile `nosuch`"),
        "{message}"
    );
    assert!(
        message.contains("Available profiles: personal, work"),
        "{message}"
    );
    assert!(message.contains("--profile personal"), "{message}");
    // Another OS user's profile gives the same hint and never lists that profile.
    let (code, message) = failure(&rig, &chat(Some("other"))).await;
    assert_eq!(code, "no_entrance");
    assert!(
        message.contains("Available profiles: personal, work"),
        "{message}"
    );
    assert!(
        !message.contains("other,") && !message.ends_with("other"),
        "{message}"
    );
    let (code, message) = failure(&rig, &chat(None)).await;
    assert_eq!(code, "profile_required");
    assert!(message.contains("personal, work"), "{message}");
    assert!(message.contains("--profile"), "{message}");
    assert!(!message.contains("other"), "{message}");
}

#[tokio::test]
async fn a_user_with_no_entrance_at_all_is_told_where_the_account_is_connected() {
    let rig = rig(&[("owner", false)], vec![desk("personal", "owner")]).await;
    for request in [chat(None), chat(Some("personal"))] {
        let (code, message) = failure(&rig, &request).await;
        assert_eq!(code, "no_entrance");
        assert!(
            message.contains("No entrance is mapped to this user"),
            "{message}"
        );
        assert!(
            message.contains("`account`") && message.contains("`gateway.cli.accounts`"),
            "{message}"
        );
        assert!(message.contains("OS user"), "{message}");
        assert!(
            !message.contains("personal,"),
            "must not enumerate other users' profiles: {message}"
        );
    }
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
    // A single reachable profile is auto-selected; a missing request reaches `not_found`.
    let outcome = client::result(&rig.socket, None, "r1", false, |_| {})
        .await
        .unwrap();
    let Outcome::Failed { code, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(code, "not_found");
}

//! Servers requiring persistent state (`state: required`) are refused by `srt` before resolving secrets or spawning.

use super::*;
use bot_core::install::ServerState;

fn required(dir: &Path) -> SrtConfig {
    let mut cfg = config(dir, fake_srt(dir, PINNED_VERSION));
    cfg.servers.get_mut("demo-agenda").unwrap().state = ServerState::Required;
    cfg
}

async fn open_required(dir: &Path) -> SrtSandbox {
    SrtSandbox::open(required(dir), Arc::new(CountingEntropy::default()))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_server_that_needs_a_state_volume_is_refused_before_any_credential_or_process() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = open_required(dir.path()).await;
    // Pre-check refuses before resolving secrets.
    assert!(matches!(
        sandbox.admit("demo-agenda", "img1"),
        Err(Error::Denied)
    ));
    // Direct spawn also refuses without creating config, pid, work dir, or process.
    let result = sandbox.spawn(spec("r1.1.c1", false)).await;
    assert!(matches!(result, Err(Error::Denied)));
    assert!(
        !dir.path().join("work/.srt").exists()
            || std::fs::read_dir(dir.path().join("work/.srt"))
                .unwrap()
                .count()
                == 0,
        "must create nothing before refusal"
    );
    let work: Vec<_> = std::fs::read_dir(dir.path().join("work"))
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        work.iter().all(|name| name == ".srt"),
        "must not create per-call work dir: {work:?}"
    );
}

#[tokio::test]
async fn the_default_state_is_ephemeral_and_still_runs_and_unknown_servers_stay_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = open(dir.path()).await;
    assert!(sandbox.admit("demo-agenda", "img1").is_ok());
    // Unknown servers and mismatched revisions remain refused.
    assert!(matches!(sandbox.admit("other", "img1"), Err(Error::Denied)));
    assert!(matches!(
        sandbox.admit("demo-agenda", "img2"),
        Err(Error::Denied)
    ));
    let process = sandbox.spawn(spec("r1.1.c2", false)).await.unwrap();
    process.handle.terminate().await.unwrap();
}

/// `admit` and `supports_state` share the same predicate.
#[tokio::test]
async fn the_per_call_admission_and_the_pre_start_predicate_give_the_same_answer() {
    assert!(supports_state(ServerState::Ephemeral));
    assert!(!supports_state(ServerState::Required));
    for state in [ServerState::Ephemeral, ServerState::Required] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), fake_srt(dir.path(), PINNED_VERSION));
        cfg.servers.get_mut("demo-agenda").unwrap().state = state;
        let sandbox = SrtSandbox::open(cfg, Arc::new(CountingEntropy::default()))
            .await
            .unwrap();
        assert_eq!(
            sandbox.admit("demo-agenda", "img1").is_ok(),
            supports_state(state),
            "{state:?}"
        );
    }
}

#[test]
fn the_default_of_the_core_enum_is_ephemeral() {
    assert_eq!(ServerState::default(), ServerState::Ephemeral);
}

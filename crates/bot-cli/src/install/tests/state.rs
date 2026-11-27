use super::*;
use bot_core::install::ServerState;

fn with_state(spec: &ServerSpec, state: ServerState) -> ServerSpec {
    ServerSpec {
        state,
        ..spec.clone()
    }
}

#[test]
fn the_state_key_is_ephemeral_by_default_and_only_accepts_the_two_words() {
    let parsed = |state: serde_json::Value| {
        let mut entry = json!({"root": "/opt/bot/servers/agenda", "command": ["bin/server"]});
        if !state.is_null() {
            entry["state"] = state;
        }
        parse_servers(&json!({"agenda": entry}))
    };
    assert_eq!(
        parsed(json!(null)).unwrap()["agenda"].state,
        ServerState::Ephemeral
    );
    assert_eq!(
        parsed(json!("ephemeral")).unwrap()["agenda"].state,
        ServerState::Ephemeral
    );
    assert_eq!(
        parsed(json!("required")).unwrap()["agenda"].state,
        ServerState::Required
    );
    for bad in [
        json!("Required"),
        json!(""),
        json!(true),
        json!(1),
        json!({}),
    ] {
        let message = parsed(bad.clone()).unwrap_err();
        assert!(
            message.contains("servers.agenda.state") && message.contains("required"),
            "{bad} -> {message}"
        );
    }
}

#[test]
fn a_required_state_is_a_different_install_revision_and_ephemeral_keeps_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let spec = package(&dir.path().join("src"));
    let ephemeral = revision_of_spec(&spec);
    assert_eq!(
        revision_of_spec(&with_state(&spec, ServerState::Ephemeral)),
        ephemeral
    );
    let required_spec = with_state(&spec, ServerState::Required);
    let required = revision_of_spec(&required_spec);
    assert_ne!(required, ephemeral);

    let base = dir.path().join("installs");
    let plain = install(&specs(spec.clone()), &base, uid()).unwrap();
    let stateful = install(&specs(required_spec.clone()), &base, uid()).unwrap();
    let (plain, stateful) = (
        plain.get("agenda").unwrap().clone(),
        stateful.get("agenda").unwrap().clone(),
    );
    assert_eq!(plain.revision, ephemeral);
    assert_eq!(stateful.revision, required);
    assert_ne!(plain.dir, stateful.dir);
    assert_eq!(
        (plain.state, stateful.state),
        (ServerState::Ephemeral, ServerState::Required)
    );
    assert!(plain.dir.is_dir() && stateful.dir.is_dir());
}

#[test]
fn a_changed_state_changes_the_activation_authority_without_touching_anything_else() {
    let dir = tempfile::tempdir().unwrap();
    let spec = package(&dir.path().join("src"));
    let mut catalog = bot_profile::HostCatalog::default();
    catalog.servers.insert("agenda".to_owned());
    let fingerprint = |state| {
        let revisions = measure(&specs(with_state(&spec, state))).unwrap();
        crate::execution::authority_fingerprint(
            &[0u8; 32],
            &catalog,
            &BTreeMap::new(),
            &Default::default(),
            &revisions,
        )
    };
    assert_eq!(
        fingerprint(ServerState::Ephemeral),
        fingerprint(ServerState::Ephemeral)
    );
    assert_ne!(
        fingerprint(ServerState::Ephemeral),
        fingerprint(ServerState::Required)
    );
}

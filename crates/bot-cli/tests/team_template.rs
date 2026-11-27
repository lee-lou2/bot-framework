//! Verifies `examples/profiles/team` using the built-in demo host (no `BOT_HOST_CONFIG`): `validate`, `map`,
//! and live demo-model chat (chat test requires PG, `srt`, and `python3`).
mod process_support;
use process_support::*;
use std::path::{Path, PathBuf};

/// Full example profiles root (`personal` + `team`), both mapped by the built-in demo host.
fn all_examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

#[test]
fn the_team_template_validates_and_maps_with_the_built_in_demo_host() {
    let profiles = all_examples();
    let env = [("BOT_PROFILES", profiles.to_str().unwrap())];
    let validate = bot(&env, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    let shown = text(&validate);
    assert!(shown.contains("Validation passed: 2 profile(s)"), "{shown}");
    assert!(
        shown.contains("team rev ") && shown.contains("agents 5 tools 2 triggers 1 bindings 1"),
        "{shown}"
    );
    let map = bot(&env, &["map", "--profile", "team"]);
    assert!(map.status.success(), "{}", text(&map));
    let tree = text(&map);
    for agent in [
        "main",
        "main/developer",
        "main/designer",
        "main/pm",
        "main/ops",
    ] {
        assert!(tree.contains(&format!("**{agent}**")), "{agent}\n{tree}");
    }
    for child in ["designer", "developer", "ops", "pm"] {
        assert!(
            tree.contains(&format!("delegates → agent:main/{child}")),
            "{child}\n{tree}"
        );
    }
    assert!(
        tree.contains("class=irreversible") && tree.contains("approval=always"),
        "{tree}"
    );
    assert!(tree.contains("cron 0 9 * * 1-5"), "{tree}");
}

#[test]
fn the_team_lead_answers_through_the_demo_model() {
    let Some(schema) = Schema::create() else {
        return;
    };
    if !have_srt_and_python() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("t")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let installs = dir.path().join("installs");
    let profiles = all_examples();
    let url = schema.url();
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[("BOT_INSTALLS", installs.to_str().unwrap())],
    );
    let env = refs(&env);
    let _server = start_gateway_roles(&env, "gateway,worker", &socket);
    let chat = bot(
        &env,
        &[
            "chat",
            "--profile",
            "team",
            "--request-id",
            "team-list",
            "Show today's agenda",
        ],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert!(text(&chat).contains("Walk"), "{}", text(&chat));
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_runs where profile = 'team' and state = 'succeeded'"
        ),
        "1"
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_runs where profile = 'personal'"),
        "0"
    );
    let start = bot(
        &env,
        &[
            "chat",
            "--profile",
            "team",
            "--no-wait",
            "--request-id",
            "team-del",
            "Delete the walk event",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let pending =
        "select count(*) from {s}.bot_approvals where profile = 'team' and state = 'pending'";
    wait_for("approval card did not open", || {
        schema.query(pending) == "1"
    });
}

//! PostgreSQL + real `bot` process tests for legacy `BOTPROFILE1` bundle restoration,
//! V1/V2 revision separation, and missing-bundle backfill (requires PG only, no `srt`).

use crate::process_support::*;
use bot_core::id::{AgentId, Revision};
use bot_profile::{Limits, Registry, read_bundle, read_legacy_bundle};
use bot_store::{Config, Store};
use std::{collections::BTreeSet, fs, path::Path};

const VALID_FM: &str =
    "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n";

fn write_legacy_skills(personal: &Path) {
    for (dir, content) in [
        ("skills/memo", VALID_FM.to_owned()),
        (
            "skills/legacy-unclosed",
            "---\nlegacy unclosed skill\n".to_owned(),
        ),
        (
            "skills/legacy-authority",
            "---\nallowed-tools: noop\n---\nbody\n".to_owned(),
        ),
        (
            "skills/legacy-big",
            "x".repeat(bot_profile::SKILL_MAX_BYTES + 1),
        ),
    ] {
        let target = personal.join(dir);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), content).unwrap();
    }
}

fn write_v2_front_matter(personal: &Path) {
    let target = personal.join("skills/memo");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("SKILL.md"), VALID_FM).unwrap();
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn host_file(dir: &Path) -> std::path::PathBuf {
    let pkg = package(dir, "#!/bin/sh\nexit 0\n");
    let host = dir.join("host.json");
    fs::write(&host, host_json(my_uid(), &pkg, "", "")).unwrap();
    host
}

/// Seeds one in-flight run and one pending approval card pinned to `legacy_rev`.
fn seed_legacy_state(
    schema: &Schema,
    dir: &Path,
    host: &Path,
    legacy_rev: &str,
    legacy_bundle: Option<&[u8]>,
) {
    let seed_profiles = dir.join("seed_profiles");
    copy_dir(&examples(), &seed_profiles);
    let sock = dir.join("run/seed.sock");
    let env = env_for(
        &seed_profiles,
        &schema.url(),
        &sock,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let gw = start_gateway(&refs(&env), &["--activate"], &sock);
    wait_for("initial active revision publish", || {
        !schema.active().is_empty()
    });
    let chat = bot(
        &refs(&env),
        &[
            "chat",
            "--profile",
            "personal",
            "--no-wait",
            "--request-id",
            "legacy-req",
            "Delete the walk event",
        ],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    drop(gw);
    let _ = fs::remove_file(&sock);

    let run_id = schema.query("select id from {s}.bot_runs limit 1");
    assert!(!run_id.is_empty());
    schema.query("truncate {s}.bot_profile_bundles");
    if let Some(bytes) = legacy_bundle {
        let hex = hex_of(bytes);
        let done = schema.exec(&format!(
            "insert into {{s}}.bot_profile_bundles (profile, revision, bundle) values ('personal', '{legacy_rev}', decode('{hex}', 'hex'))"
        ));
        assert!(done.starts_with("INSERT 0 1"), "{done}");
    }
    for stmt in [
        format!("update {{s}}.bot_runs set revision = '{legacy_rev}' where id = '{run_id}'"),
        format!(
            "update {{s}}.bot_active_profiles set revision = '{legacy_rev}' where profile = 'personal'"
        ),
        format!(
            "update {{s}}.bot_schedule_segments set revision = '{legacy_rev}' where profile = 'personal'"
        ),
        format!(
            "insert into {{s}}.bot_calls (profile, run, call, tool, args_sha256, state) \
             values ('personal', '{run_id}', 'c1', 'agenda.delete', decode('0000000000000000000000000000000000000000000000000000000000000000', 'hex'), 'open')"
        ),
        format!(
            "insert into {{s}}.bot_approvals (nonce, profile, run, generation, call, requester, approver, risk, policy_revision, tool, tool_revision, args_sha256, card, expires_ms, state) \
             values ('legacy-nonce-1', 'personal', '{run_id}', 1, 'c1', 'local:owner', 'local:owner', 'irreversible', 'p1', 'agenda.delete', 't1', decode('0000000000000000000000000000000000000000000000000000000000000000', 'hex'), '{{}}', 4102444800000, 'pending')"
        ),
    ] {
        let done = schema.exec(&stmt);
        assert!(
            done.starts_with("UPDATE ") || done.starts_with("INSERT 0 1"),
            "{stmt}: {done}"
        );
    }
}

#[test]
fn pg_upgrade_restores_legacy_v1_bundle_and_separates_v2_revision() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("lg")
        .tempdir_in("/tmp")
        .unwrap();
    let host = host_file(dir.path());

    let legacy_profiles = dir.path().join("legacy_profiles");
    copy_dir(&examples(), &legacy_profiles);
    write_legacy_skills(&legacy_profiles.join("personal"));
    let (id, legacy_rev, legacy_bytes) =
        read_legacy_bundle(&legacy_profiles.join("personal"), &Limits::default()).unwrap();
    assert!(legacy_bytes.starts_with(b"BOTPROFILE1\n"));

    seed_legacy_state(
        &schema,
        dir.path(),
        &host,
        &legacy_rev.0,
        Some(&legacy_bytes),
    );

    let v2_profiles = dir.path().join("v2_profiles");
    copy_dir(&examples(), &v2_profiles);
    write_v2_front_matter(&v2_profiles.join("personal"));
    let sock = dir.path().join("run/v2.sock");
    let env2 = env_for(
        &v2_profiles,
        &schema.url(),
        &sock,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let gw = start_gateway(&refs(&env2), &["--activate"], &sock);
    wait_for("V2 active revision publish", || {
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
            != legacy_rev.0
    });
    let v2_rev =
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'");
    assert_ne!(
        v2_rev, legacy_rev.0,
        "V2 revision must differ from legacy V1"
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_profile_bundles"),
        "2",
        "both legacy V1 and V2 bundles must be stored"
    );
    assert_eq!(
        schema.query("select state from {s}.bot_approvals where nonce = 'legacy-nonce-1'"),
        "pending",
        "pending legacy approval card must be preserved"
    );
    drop(gw);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let store = Store::connect(&schema.url(), Config::default())
            .await
            .unwrap();
        let registry = Registry::new();
        let wanted = BTreeSet::from([
            (id.clone(), legacy_rev.clone()),
            (id.clone(), Revision(v2_rev.clone())),
        ]);
        let restored = registry
            .restore(&store, &wanted, &Limits::default())
            .await
            .expect("restore both V1 and V2 bundles from PG");
        assert_eq!(restored.len(), 2);
        let main = AgentId("main".into());
        use bot_core::profile::ProfileCatalog;
        let old_snap = registry.lookup(&id, &legacy_rev).unwrap();
        let new_snap = registry.lookup(&id, &Revision(v2_rev)).unwrap();
        assert_eq!(
            old_snap.agents[&main].skills["memo"].instructions.text,
            VALID_FM.trim_end(),
            "legacy V1 restore must keep raw SKILL.md bytes"
        );
        assert_eq!(
            old_snap.agents[&main].skills["legacy-big"]
                .instructions
                .text
                .len(),
            bot_profile::SKILL_MAX_BYTES + 1
        );
        assert_eq!(
            new_snap.agents[&main].skills["memo"].instructions.text, "Visible stable paragraph.",
            "V2 restore must strip front matter"
        );
        assert_eq!(
            new_snap.agents[&main].skills["memo"].description.as_deref(),
            Some("HEADER_SENTINEL")
        );
    });
}

#[test]
fn pg_profile_bundle_put_backfills_missing_legacy_v1_bundle_and_refuses_unpinned_invalid() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("lp")
        .tempdir_in("/tmp")
        .unwrap();
    let host = host_file(dir.path());

    let legacy_profiles = dir.path().join("legacy_profiles");
    copy_dir(&examples(), &legacy_profiles);
    let legacy_dir = legacy_profiles.join("personal");
    write_legacy_skills(&legacy_dir);
    let (_, legacy_rev, _) = read_legacy_bundle(&legacy_dir, &Limits::default()).unwrap();

    seed_legacy_state(&schema, dir.path(), &host, &legacy_rev.0, None);

    let v2_profiles = dir.path().join("v2_profiles");
    copy_dir(&examples(), &v2_profiles);
    write_v2_front_matter(&v2_profiles.join("personal"));
    let sock = dir.path().join("run/v2.sock");
    let env2 = env_for(
        &v2_profiles,
        &schema.url(),
        &sock,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );

    let blocked = bot(&refs(&env2), &["serve", "--role", "gateway", "--activate"]);
    assert_eq!(blocked.status.code(), Some(1), "{}", text(&blocked));
    assert!(
        text(&blocked).contains(&format!(
            "profile personal revision {}: stored bundle is missing",
            legacy_rev.0
        )),
        "{}",
        text(&blocked)
    );

    let before_put: i64 = schema
        .query("select count(*) from {s}.bot_profile_bundles")
        .parse()
        .unwrap();
    let args = ["profile-bundle", "put", legacy_dir.to_str().unwrap()];
    for run in 0..2 {
        let filled = bot(&refs(&env2), &args);
        assert!(filled.status.success(), "run {run}: {}", text(&filled));
        let out = text(&filled);
        assert!(
            out.contains("personal") && out.contains(&legacy_rev.0),
            "run {run}: {out}"
        );
    }
    let after_put: i64 = schema
        .query("select count(*) from {s}.bot_profile_bundles")
        .parse()
        .unwrap();
    assert_eq!(
        after_put,
        before_put + 1,
        "repeated backfill must insert only one legacy bundle"
    );
    assert_eq!(
        schema.query(&format!(
            "select count(*) from {{s}}.bot_profile_bundles where profile = 'personal' and revision = '{}'",
            legacy_rev.0
        )),
        "1"
    );

    let gw = start_gateway(&refs(&env2), &["--activate"], &sock);
    wait_for("V2 publish after backfill", || {
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
            != legacy_rev.0
    });
    drop(gw);

    // Unpinned directories that fail V2 rules must still be rejected by `profile-bundle put`.
    let unrelated = dir.path().join("unrelated/personal");
    copy_dir(&examples().join("personal"), &unrelated);
    fs::create_dir_all(unrelated.join("skills/bad")).unwrap();
    fs::write(
        unrelated.join("skills/bad/SKILL.md"),
        "---\nallowed-tools: Bash\n---\nbody\n",
    )
    .unwrap();
    let refused = bot(
        &refs(&env2),
        &["profile-bundle", "put", unrelated.to_str().unwrap()],
    );
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("skill-allowed-tools"),
        "{}",
        text(&refused)
    );
}

#[test]
fn pg_profile_bundle_put_backfills_missing_v2_when_both_v1_and_v2_are_pinned() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("dp")
        .tempdir_in("/tmp")
        .unwrap();
    let host = host_file(dir.path());

    // The same front-matter directory produces distinct V1 and V2 revisions.
    let valid_profiles = dir.path().join("valid_profiles");
    copy_dir(&examples(), &valid_profiles);
    let valid_dir = valid_profiles.join("personal");
    write_v2_front_matter(&valid_dir);
    let (_, v1_rev, v1_bytes) = read_legacy_bundle(&valid_dir, &Limits::default()).unwrap();
    let (_, v2_rev, _) = read_bundle(&valid_dir, &Limits::default()).unwrap();
    assert_ne!(v1_rev, v2_rev);

    seed_legacy_state(&schema, dir.path(), &host, &v1_rev.0, Some(&v1_bytes));

    let sock = dir.path().join("run/dual.sock");
    let env_valid = env_for(
        &valid_profiles,
        &schema.url(),
        &sock,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let gw = start_gateway(&refs(&env_valid), &["--activate"], &sock);
    wait_for("V2 active revision publish", || {
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
            == v2_rev.0
    });
    let chat2 = bot(
        &refs(&env_valid),
        &[
            "chat",
            "--profile",
            "personal",
            "--no-wait",
            "--request-id",
            "v2-req",
            "Show today's agenda",
        ],
    );
    assert!(chat2.status.success(), "{}", text(&chat2));
    drop(gw);
    let _ = fs::remove_file(&sock);

    // Keep only the V1 bundle so the pinned V2 bundle is missing.
    schema.query("truncate {s}.bot_profile_bundles");
    let hex_v1 = hex_of(&v1_bytes);
    let inserted = schema.exec(&format!(
        "insert into {{s}}.bot_profile_bundles (profile, revision, bundle) values ('personal', '{}', decode('{hex_v1}', 'hex'))",
        v1_rev.0
    ));
    assert!(inserted.starts_with("INSERT 0 1"), "{inserted}");

    let clean_profiles = dir.path().join("clean_profiles");
    copy_dir(&examples(), &clean_profiles);
    let env_clean = env_for(
        &clean_profiles,
        &schema.url(),
        &sock,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let blocked = bot(
        &refs(&env_clean),
        &["serve", "--role", "gateway", "--activate"],
    );
    assert_eq!(blocked.status.code(), Some(1), "{}", text(&blocked));
    assert!(
        text(&blocked).contains(&format!(
            "profile personal revision {}: stored bundle is missing",
            v2_rev.0
        )),
        "{}",
        text(&blocked)
    );

    let before_put: i64 = schema
        .query("select count(*) from {s}.bot_profile_bundles")
        .parse()
        .unwrap();
    let args = ["profile-bundle", "put", valid_dir.to_str().unwrap()];
    for run in 0..2 {
        let filled = bot(&refs(&env_clean), &args);
        assert!(filled.status.success(), "run {run}: {}", text(&filled));
        let out = text(&filled);
        assert!(
            out.contains(&v1_rev.0) && out.contains(&v2_rev.0),
            "run {run}: both V1 and V2 revisions must be printed: {out}"
        );
    }
    let after_put: i64 = schema
        .query("select count(*) from {s}.bot_profile_bundles")
        .parse()
        .unwrap();
    assert_eq!(
        after_put,
        before_put + 1,
        "only the missing V2 bundle should be added across repeated puts"
    );
    for rev in [&v1_rev.0, &v2_rev.0] {
        assert_eq!(
            schema.query(&format!(
                "select count(*) from {{s}}.bot_profile_bundles where profile = 'personal' and revision = '{rev}'"
            )),
            "1"
        );
    }

    let gw = start_gateway(&refs(&env_clean), &["--activate"], &sock);
    wait_for("clean active revision publish after backfill", || {
        let cur =
            schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'");
        cur != v1_rev.0 && cur != v2_rev.0
    });
    drop(gw);
}

#[test]
fn pg_upgrade_restores_legacy_agent_bare_and_unicode_cron_bundles() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("ac")
        .tempdir_in("/tmp")
        .unwrap();
    let host = host_file(dir.path());

    // Legacy V1 directory with bare `---` AGENT.md and NBSP unicode cron in triggers.yaml.
    let legacy_profiles = dir.path().join("legacy_profiles");
    copy_dir(&examples(), &legacy_profiles);
    let legacy_dir = legacy_profiles.join("personal");
    fs::write(legacy_dir.join("agents/main/AGENT.md"), b"---").unwrap();
    fs::write(
        legacy_dir.join("agents/main/triggers.yaml"),
        "version: 1\ntriggers:\n  - id: morning\n    \"on\": {cron: \"0\\u00a09 * * *\"}\n    principal: \"service:morning\"\n    target: {tool: agenda.list, args: {}}\n",
    )
    .unwrap();
    let (_, legacy_rev, legacy_bytes) =
        read_legacy_bundle(&legacy_dir, &Limits::default()).unwrap();

    seed_legacy_state(&schema, dir.path(), &host, &legacy_rev.0, None);
    let next_profiles = dir.path().join("next_profiles");
    copy_dir(&examples(), &next_profiles);
    write_v2_front_matter(&next_profiles.join("personal"));
    let sock = dir.path().join("run/ac.sock");
    let env_next = env_for(
        &next_profiles,
        &schema.url(),
        &sock,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let put = bot(
        &refs(&env_next),
        &["profile-bundle", "put", legacy_dir.to_str().unwrap()],
    );
    assert!(put.status.success(), "{}", text(&put));
    assert!(text(&put).contains(&legacy_rev.0), "{}", text(&put));
    assert_eq!(
        schema.query(&format!(
            "select encode(bundle, 'hex') from {{s}}.bot_profile_bundles where revision = '{}'",
            legacy_rev.0
        )),
        hex_of(&legacy_bytes)
    );
    let gw = start_gateway(&refs(&env_next), &["--activate"], &sock);
    wait_for(
        "activation after restoring bare AGENT.md + unicode cron bundle",
        || {
            schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
                != legacy_rev.0
        },
    );
    drop(gw);
}

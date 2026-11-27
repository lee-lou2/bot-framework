//! Process-level tests for redeploy bundle storage and restoration: missing/corrupted bundles halt
//! activation with zero state mutation, `bot profile-bundle put` backfills from the matching source
//! directory (including legacy `BOTPROFILE1`), and joining the active set backfills current bundles.
mod process_support;
mod restore_support;
use process_support::*;
use restore_support::*;
use std::path::PathBuf;

fn parked_with_a_pending_card() -> Option<(Cluster, PathBuf, PathBuf, String, String)> {
    let c = Cluster::new()?;
    let (v1, v2) = (
        c.profiles("v1", |_| {}),
        c.profiles("v2", instructions_changed),
    );
    let first = c.start(&v1, false, &["worker", "admin"]);
    let start = bot(
        &refs(&c.env(&v1)),
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "parked-del",
            "Delete the walk event",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    wait_card("approval card did not open", first.admin.as_ref().unwrap());
    let (r1, active) = (c.profile_revision(), c.schema.active());
    drop(first);
    Some((c, v1, v2, r1, active))
}

#[test]
fn a_missing_or_damaged_bundle_stops_the_start_and_changes_nothing_until_the_original_folder_fills_it()
 {
    let Some((c, v1, v2, r1, active)) = parked_with_a_pending_card() else {
        return;
    };
    let before = fingerprint(&c);
    assert!(before.contains("pending"), "{before}");
    c.q("truncate {s}.bot_profile_bundles");
    let env2 = c.env(&v2);
    let (code, out) = serve_that_must_stop(&refs(&env2), &["--role", "gateway", "--activate"])
        .expect("startup must fail when a pinned bundle is missing");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "profile personal revision {r1}: stored bundle is missing"
        )),
        "{out}"
    );
    assert!(out.contains("bot profile-bundle put"), "{out}");
    assert_eq!(
        fingerprint(&c),
        before,
        "active set, attempts, runs, and pending cards must remain unchanged"
    );
    assert_eq!(c.schema.active(), active);
    // Simulate out-of-band corruption by bypassing the immutability check/trigger.
    for statement in [
        "alter table {s}.bot_profile_bundles drop constraint bot_profile_bundles_check".to_owned(),
        "alter table {s}.bot_profile_bundles disable trigger bot_profile_bundles_frozen".to_owned(),
        format!(
            "insert into {{s}}.bot_profile_bundles (profile, revision, bundle) values ('personal', '{r1}', convert_to('BOTPROFILE1 broken', 'UTF8'))"
        ),
    ] {
        let done = c.schema.exec(&statement);
        assert!(
            done.starts_with("ALTER TABLE") || done.starts_with("INSERT 0 1"),
            "{statement}: {done}"
        );
    }
    let (code, out) = serve_that_must_stop(&refs(&env2), &["--role", "gateway", "--activate"])
        .expect("startup must fail when a pinned bundle is corrupted");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "profile personal revision {r1}: stored bundle is unreadable"
        )),
        "{out}"
    );
    assert_eq!(fingerprint(&c), before);
    // Bundles are immutable: `put` refuses to overwrite a conflicting row until removed.
    let folder = v1.join("personal");
    let args = ["profile-bundle", "put", folder.to_str().unwrap()];
    let blocked = bot(&refs(&env2), &args);
    assert!(!blocked.status.success(), "{}", text(&blocked));
    let blocked_msg = text(&blocked);
    let blocked_low = blocked_msg.to_ascii_lowercase();
    assert!(
        blocked_low.contains("different")
            || blocked_low.contains("already exists")
            || blocked_low.contains("corrupt")
            || blocked_low.contains("conflict"),
        "{blocked_msg}"
    );
    c.q(&format!(
        "delete from {{s}}.bot_profile_bundles where revision = '{r1}'"
    ));
    let filled = bot(&refs(&env2), &args);
    assert!(filled.status.success(), "{}", text(&filled));
    let filled_msg = text(&filled);
    assert!(
        filled_msg.contains("personal") && filled_msg.contains(&r1),
        "{filled_msg}"
    );
    assert_eq!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_profile_bundles where revision = '{r1}'"
        )),
        "1"
    );
    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.schema.active(), active, "new revision must be published");
    let admin = second.admin.as_ref().unwrap();
    let card = wait_card("pending card missing after redeploy", admin);
    let (status, body) = decide(admin, &card, true);
    assert_eq!(status, 200, "{body}");
    wait_until("approved call did not finish", || {
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state = 'done'")
            == "1"
    });
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0"
    );
}

#[test]
fn joining_the_same_set_fills_in_the_current_bundles_an_older_version_never_stored() {
    let Some(c) = Cluster::new() else {
        return;
    };
    let v1 = c.profiles("v1", |_| {});
    drop(c.start(&v1, false, &[]));
    wait_until("initial publish did not store current bundle", || {
        c.q("select count(*) from {s}.bot_profile_bundles") == "1"
    });
    c.q("truncate {s}.bot_profile_bundles");
    let active = c.schema.active();
    let _joined = c.start(&v1, false, &[]);
    assert_eq!(
        c.schema.active(),
        active,
        "joining the same set must not replace activation"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles"),
        "1",
        "joining must backfill the current bundle"
    );
}

/// Source directory and `BOTPROFILE1` bundle violating V2 `SKILL.md` rules (unclosed `---`, >64 KiB).
fn legacy_v1_folder_and_bundle(c: &Cluster) -> (PathBuf, String, Vec<u8>) {
    let legacy_root = c.profiles("legacy_v1", |personal| {
        for (dir, body) in [
            (
                "skills/memo",
                "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n"
                    .to_owned(),
            ),
            ("skills/legacy-unclosed", "---\nlegacy skill\n".to_owned()),
            (
                "skills/legacy-big",
                "x".repeat(bot_profile::SKILL_MAX_BYTES + 1),
            ),
        ] {
            let folder = personal.join(dir);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("SKILL.md"), body).unwrap();
        }
    });
    let dir = legacy_root.join("personal");
    let (_, rev, bytes) =
        bot_profile::read_legacy_bundle(&dir, &bot_profile::Limits::default()).unwrap();
    assert!(bytes.starts_with(b"BOTPROFILE1\n"));
    (dir, rev.0, bytes)
}

fn v2_front_matter_profiles(c: &Cluster) -> PathBuf {
    c.profiles("v2_fm", |personal| {
        instructions_changed(personal);
        let folder = personal.join("skills/memo");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n",
        )
        .unwrap();
    })
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn pin_parked_run_to_legacy(c: &Cluster, legacy_rev: &str, bundle: Option<&[u8]>) {
    c.q("truncate {s}.bot_profile_bundles");
    if let Some(bytes) = bundle {
        let hex = hex_of(bytes);
        let done = c.schema.exec(&format!(
            "insert into {{s}}.bot_profile_bundles (profile, revision, bundle) values ('personal', '{legacy_rev}', decode('{hex}', 'hex'))"
        ));
        assert!(done.starts_with("INSERT 0 1"), "{done}");
    }
    for stmt in [
        format!("update {{s}}.bot_runs set revision = '{legacy_rev}' where profile = 'personal'"),
        format!(
            "update {{s}}.bot_active_profiles set revision = '{legacy_rev}' where profile = 'personal'"
        ),
        format!(
            "update {{s}}.bot_schedule_segments set revision = '{legacy_rev}' where profile = 'personal'"
        ),
    ] {
        let done = c.schema.exec(&stmt);
        assert!(done.starts_with("UPDATE "), "{stmt}: {done}");
    }
}

#[test]
fn a_legacy_v1_bundle_with_unclosed_or_oversized_skill_restores_and_resumes_a_pending_card() {
    let Some((c, _, _, _, active)) = parked_with_a_pending_card() else {
        return;
    };
    let (_, legacy_rev, legacy_bytes) = legacy_v1_folder_and_bundle(&c);
    let v2 = v2_front_matter_profiles(&c);
    pin_parked_run_to_legacy(&c, &legacy_rev, Some(&legacy_bytes));

    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.schema.active(), active, "new V2 revision must publish");
    assert_ne!(
        c.profile_revision(),
        legacy_rev,
        "V2 revision must differ from legacy V1"
    );
    let admin = second.admin.as_ref().unwrap();
    let card = wait_card("legacy pending card missing after redeploy", admin);
    let (status, body) = decide(admin, &card, true);
    assert_eq!(status, 200, "{body}");
    wait_until("legacy approved call did not finish", || {
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state = 'done'")
            == "1"
    });
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0"
    );
}

#[test]
fn profile_bundle_put_backfills_a_missing_legacy_v1_bundle_for_upgrade() {
    let Some((c, _, _, _, active)) = parked_with_a_pending_card() else {
        return;
    };
    let (legacy_dir, legacy_rev, _) = legacy_v1_folder_and_bundle(&c);
    let v2 = v2_front_matter_profiles(&c);
    pin_parked_run_to_legacy(&c, &legacy_rev, None);

    let env2 = c.env(&v2);
    let (code, out) = serve_that_must_stop(&refs(&env2), &["--role", "gateway", "--activate"])
        .expect("startup must fail when a legacy V1 bundle is missing");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "profile personal revision {legacy_rev}: stored bundle is missing"
        )),
        "{out}"
    );
    let args = ["profile-bundle", "put", legacy_dir.to_str().unwrap()];
    for run in 0..2 {
        let filled = bot(&refs(&env2), &args);
        assert!(filled.status.success(), "run {run}: {}", text(&filled));
        let filled_msg = text(&filled);
        assert!(
            filled_msg.contains("personal") && filled_msg.contains(&legacy_rev),
            "run {run}: {filled_msg}"
        );
    }
    assert_eq!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_profile_bundles where revision = '{legacy_rev}'"
        )),
        "1",
        "idempotent backfill must leave only one bundle row"
    );

    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.schema.active(), active);
    let admin = second.admin.as_ref().unwrap();
    let card = wait_card("pending card missing after backfill", admin);
    let (status, body) = decide(admin, &card, true);
    assert_eq!(status, 200, "{body}");
    wait_until("approved call did not finish after backfill", || {
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state = 'done'")
            == "1"
    });
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0"
    );
}

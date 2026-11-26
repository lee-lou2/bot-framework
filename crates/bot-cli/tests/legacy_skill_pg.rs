//! 실제 PostgreSQL + 실제 `bot` 프로세스로 구판(`BOTPROFILE1`) 번들 복원·판본 분리·누락 번들 보충을 검증한다.
//! (`srt` 없이 DB만으로 매번 실행한다.)
mod process_support;

use bot_core::id::{AgentId, Revision};
use bot_profile::{Limits, Registry, read_legacy_bundle};
use bot_store::{Config, Store};
use process_support::*;
use std::{collections::BTreeSet, fs, path::Path};

const VALID_FM: &str =
    "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n";

fn write_legacy_skills(personal: &Path) {
    for (dir, content) in [
        ("skills/memo", VALID_FM.to_owned()),
        (
            "skills/legacy-unclosed",
            "---\n구판 미닫힘 스킬\n".to_owned(),
        ),
        (
            "skills/legacy-authority",
            "---\nallowed-tools: noop\n---\n본문\n".to_owned(),
        ),
        ("skills/legacy-big", "x".repeat(32_769)),
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

/// 초기 실행 1건과 승인 대기 카드 1건을 구판 `legacy_rev`에 고정해 구판 업그레이드 직전 DB 상태를 만든다.
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
    wait_for("초기 활성 판본 게시", || {
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
            "산책 일정을 삭제해줘",
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
        // 승인 대기 카드 1건도 같은 구판 실행에 묶는다
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
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("lg")
        .tempdir_in("/tmp")
        .unwrap();
    let host = host_file(dir.path());

    // 구판에서 유효했던 원본 폴더(미닫힘 `---`·`allowed-tools`·32 KiB 초과·front matter 포함)
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

    // 신판 폴더(정상 front matter → BOTPROFILE2)로 --activate 기동
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
    wait_for("신판 V2 활성 판본 게시", || {
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
            != legacy_rev.0
    });
    let v2_rev =
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'");
    assert_ne!(v2_rev, legacy_rev.0, "신판 V2 판본은 구판 V1과 달라야 한다");
    assert_eq!(
        schema.query("select count(*) from {s}.bot_profile_bundles"),
        "2",
        "구판 V1과 신판 V2 묶음이 함께 보관된다"
    );
    assert_eq!(
        schema.query("select state from {s}.bot_approvals where nonce = 'legacy-nonce-1'"),
        "pending",
        "대기 중이던 구판 승인 카드는 보존된다"
    );
    drop(gw);

    // 실제 PG Store에서 Registry::restore로 구판 V1·신판 V2를 둘 다 복원해 지침 차이를 확인한다
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
            .expect("실제 PG에서 V1·V2 묶음 모두 복원");
        assert_eq!(restored.len(), 2);
        let main = AgentId("main".into());
        use bot_core::profile::ProfileCatalog;
        let old_snap = registry.lookup(&id, &legacy_rev).unwrap();
        let new_snap = registry.lookup(&id, &Revision(v2_rev)).unwrap();
        assert_eq!(
            old_snap.agents[&main].skills["memo"].instructions.text,
            VALID_FM.trim_end(),
            "구판 V1 복원은 원문 그대로"
        );
        assert_eq!(
            old_snap.agents[&main].skills["legacy-big"]
                .instructions
                .text
                .len(),
            32_769
        );
        assert_eq!(
            new_snap.agents[&main].skills["memo"].instructions.text, "Visible stable paragraph.",
            "신판 V2 복원은 front matter 제거"
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
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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

    // 구판 번들이 누락된 상태로 시드한다
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

    // 1) 누락 상태에서는 기동이 멈춘다
    let blocked = bot(&refs(&env2), &["serve", "--role", "gateway", "--activate"]);
    assert_eq!(blocked.status.code(), Some(1), "{}", text(&blocked));
    assert!(
        text(&blocked).contains(&format!(
            "프로필 personal 판본 {}: 보관한 묶음이 없어요",
            legacy_rev.0
        )),
        "{}",
        text(&blocked)
    );

    // 2) `bot profile-bundle put`을 두 번 실행해도 V1 SHA-256으로 1건만 보관된다
    let before_put: i64 = schema
        .query("select count(*) from {s}.bot_profile_bundles")
        .parse()
        .unwrap();
    let args = ["profile-bundle", "put", legacy_dir.to_str().unwrap()];
    for run in 0..2 {
        let filled = bot(&refs(&env2), &args);
        assert!(filled.status.success(), "run {run}: {}", text(&filled));
        assert!(
            text(&filled).contains(&format!("프로필 personal 판본 {}", legacy_rev.0)),
            "run {run}: {}",
            text(&filled)
        );
    }
    let after_put: i64 = schema
        .query("select count(*) from {s}.bot_profile_bundles")
        .parse()
        .unwrap();
    assert_eq!(
        after_put,
        before_put + 1,
        "같은 구판 폴더를 두 번 보충해도 구판 번들 1건만 더해진다"
    );
    assert_eq!(
        schema.query(&format!(
            "select count(*) from {{s}}.bot_profile_bundles where profile = 'personal' and revision = '{}'",
            legacy_rev.0
        )),
        "1"
    );

    // 3) 보충 뒤 신판이 정상 기동·활성화된다
    let gw = start_gateway(&refs(&env2), &["--activate"], &sock);
    wait_for("보충 뒤 신판 V2 게시", || {
        schema.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
            != legacy_rev.0
    });
    drop(gw);

    // 4) DB에 고정되지 않은 임의의 미닫힘/과대 폴더는 `profile-bundle put`이 새 규칙으로 거절한다
    let unrelated = dir.path().join("unrelated/personal");
    copy_dir(&examples().join("personal"), &unrelated);
    fs::create_dir_all(unrelated.join("skills/bad")).unwrap();
    fs::write(
        unrelated.join("skills/bad/SKILL.md"),
        "---\nallowed-tools: Bash\n---\n본문\n",
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

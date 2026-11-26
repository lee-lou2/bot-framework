//! 재배포 복원의 번들 보관(실제 프로세스·팀 시험 DB·srt·python3): 필요한 번들이 저장소에 없거나 손상됐으면 기동이 멈추고 활성 판본·실행·대기 카드는 그대로이며,
//! `bot profile-bundle put`으로 원본 폴더를 채우면 시작한다. 같은 집합에 합류하는 기동은 이전 버전이 보관하지 않은 현재 번들을 채운다.
//! 팀 시험 DB·`srt`·`python3`가 없으면 건너뛴다.
mod process_support;
mod restore_support;
use process_support::*;
use restore_support::*;
use std::path::PathBuf;

/// 승인 카드가 열려 있는 상태에서 정지한 저장소: (클러스터, 옛 판본, 새 판본, 옛 판본의 프로필 판본, 옛 활성 판본)
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
            "산책 일정을 삭제해줘",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    wait_card("승인 카드가 열리지 않았어요", first.admin.as_ref().unwrap());
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
    // 이전 버전이 보관하지 않은 상태: 보관한 묶음을 모두 비운다
    c.q("truncate {s}.bot_profile_bundles");
    let env2 = c.env(&v2);
    let (code, out) = serve_that_must_stop(&refs(&env2), &["--role", "gateway", "--activate"])
        .expect("묶음이 없으면 시작하지 않아야 한다");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!("프로필 personal 판본 {r1}: 보관한 묶음이 없어요")),
        "{out}"
    );
    assert!(out.contains("bot profile-bundle put"), "{out}");
    assert_eq!(
        fingerprint(&c),
        before,
        "활성 집합·시도 횟수·실행·대기 카드는 그대로다"
    );
    assert_eq!(c.schema.active(), active);
    // 손상: 같은 키에 다른 바이트(저장소 코드를 거치지 않은 변조 흉내)
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
        .expect("묶음이 손상됐으면 시작하지 않아야 한다");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "프로필 personal 판본 {r1}: 보관한 묶음을 읽지 못했어요"
        )),
        "{out}"
    );
    assert_eq!(fingerprint(&c), before);
    // 원본 폴더로 채운다: 보관한 묶음은 불변이라 `put`이 손상된 행을 덮어쓰지 않는다 — 지운 뒤 같은 프로필 ID·해시의 원본으로 다시 보관한다
    let folder = v1.join("personal");
    let args = ["profile-bundle", "put", folder.to_str().unwrap()];
    let blocked = bot(&refs(&env2), &args);
    assert!(!blocked.status.success(), "{}", text(&blocked));
    assert!(
        text(&blocked).contains("다른 묶음이 이미 있어요"),
        "{}",
        text(&blocked)
    );
    c.q(&format!(
        "delete from {{s}}.bot_profile_bundles where revision = '{r1}'"
    ));
    let filled = bot(&refs(&env2), &args);
    assert!(filled.status.success(), "{}", text(&filled));
    assert!(
        text(&filled).contains(&format!("프로필 personal 판본 {r1}")),
        "{}",
        text(&filled)
    );
    // 다른 판본의 폴더는 그 판본을 채우지 못한다(해시가 키다)
    assert_eq!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_profile_bundles where revision = '{r1}'"
        )),
        "1"
    );
    // 이제 시작하고, 기다리던 카드를 승인하면 같은 호출이 한 번 실행된다
    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.schema.active(), active, "새 판본이 게시됐다");
    let admin = second.admin.as_ref().unwrap();
    let card = wait_card("재배포 뒤 승인 카드가 사라졌어요", admin);
    let (status, body) = decide(admin, &card, true);
    assert_eq!(status, 200, "{body}");
    wait_until("승인한 호출이 끝나지 않았어요", || {
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
    wait_until(
        "첫 게시가 현재 묶음을 보관하지 않았어요",
        || c.q("select count(*) from {s}.bot_profile_bundles") == "1",
    );
    // 이전 버전이 같은 집합을 게시한 상태(묶음 없음)에 새 프로세스가 합류한다
    c.q("truncate {s}.bot_profile_bundles");
    let active = c.schema.active();
    let _joined = c.start(&v1, false, &[]);
    assert_eq!(
        c.schema.active(),
        active,
        "같은 집합이라 게시를 바꾸지 않는다"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles"),
        "1",
        "합류가 현재 묶음을 채운다"
    );
}

/// 구판(`417162e`)에서는 유효했으나 신판 SKILL.md 규칙(미닫힘 `---`·32 KiB 초과·front matter)에 걸리는 원본 폴더와 그 `BOTPROFILE1` 묶음.
fn legacy_v1_folder_and_bundle(c: &Cluster) -> (PathBuf, String, Vec<u8>) {
    let legacy_root =
        c.profiles("legacy_v1", |personal| {
            for (dir, body) in [
            (
                "skills/memo",
                "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n"
                    .to_owned(),
            ),
            ("skills/legacy-unclosed", "---\n구판에서 쓰던 스킬\n".to_owned()),
            ("skills/legacy-big", "x".repeat(32_769)),
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

/// `v2` 폴더: 정상 Agent Skills front matter가 든 `memo` 스킬을 포함해 신판(`BOTPROFILE2`)으로 활성화한다.
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

/// 대기 중인 승인 카드의 실행·활성 프로필·예약 구간을 구판 `legacy_rev`에 묶어 구판에서 접수한 상태를 재현한다.
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

    // 신판은 새 V2 프로필을 활성화하면서 구판 V1 묶음(미닫힘 `---` + 32 KiB 초과)을 오류 없이 복원하고 대기 카드를 이어서 완료한다
    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.schema.active(), active, "새 V2 판본이 게시됐다");
    assert_ne!(
        c.profile_revision(),
        legacy_rev,
        "V2 판본은 구판 V1과 다르다"
    );
    let admin = second.admin.as_ref().unwrap();
    let card = wait_card("재배포 뒤 구판 대기 카드가 사라졌어요", admin);
    let (status, body) = decide(admin, &card, true);
    assert_eq!(status, 200, "{body}");
    wait_until(
        "구판 승인 카드의 호출이 끝나지 않았어요",
        || {
            c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state = 'done'")
            == "1"
        },
    );
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

    // 구판 V1 묶음이 누락돼 있으면 기동이 멈춘다
    let env2 = c.env(&v2);
    let (code, out) = serve_that_must_stop(&refs(&env2), &["--role", "gateway", "--activate"])
        .expect("누락된 구판 V1 묶음 없이 시작하지 않아야 한다");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "프로필 personal 판본 {legacy_rev}: 보관한 묶음이 없어요"
        )),
        "{out}"
    );
    // `bot profile-bundle put`은 신판 규칙을 통과하지 못하는 옛 원본 폴더에서도 고정된 V1 SHA-256을 재생성해 보충한다(두 번 넣어도 1건)
    let args = ["profile-bundle", "put", legacy_dir.to_str().unwrap()];
    for run in 0..2 {
        let filled = bot(&refs(&env2), &args);
        assert!(filled.status.success(), "run {run}: {}", text(&filled));
        assert!(
            text(&filled).contains(&format!("프로필 personal 판본 {legacy_rev}")),
            "run {run}: {}",
            text(&filled)
        );
    }
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles"),
        "1",
        "같은 구판 폴더를 두 번 보충해도 묶음은 한 건만 남는다"
    );

    // 이제 신판이 기동하고 대기 카드를 승인해 완료한다
    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.schema.active(), active);
    let admin = second.admin.as_ref().unwrap();
    let card = wait_card("보충 뒤 대기 카드가 사라졌어요", admin);
    let (status, body) = decide(admin, &card, true);
    assert_eq!(status, 200, "{body}");
    wait_until(
        "보충 뒤 승인한 호출이 끝나지 않았어요",
        || {
            c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state = 'done'")
            == "1"
        },
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0"
    );
}

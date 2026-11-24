//! 호출별 폴더: 매번 새로·배타적으로·0700으로 만들고, 이미 있는 폴더(다른 프로세스·다른 claim의 것)는 읽지도 지우지도 않는다.

use super::*;
use bot_core::id::{ClaimEpoch, Generation, ProfileId, RunId};
use std::os::unix::fs::PermissionsExt;

fn attempt(claim: u64) -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("run-1".into()),
        claim: Some(ClaimEpoch(claim)),
        generation: Generation(1),
    }
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bot-call-dir-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn fixed(words: &'static [&'static str]) -> impl FnMut() -> String {
    let mut next = 0;
    move || {
        let word = words[next.min(words.len() - 1)];
        next += 1;
        word.to_owned()
    }
}

#[test]
fn every_call_gets_its_own_new_private_folder() {
    let root = workspace("fresh");
    let first = CallDir::create(&root, &attempt(1)).unwrap();
    let second = CallDir::create(&root, &attempt(1)).unwrap();
    assert_ne!(
        first.root, second.root,
        "같은 시도라도 호출마다 다른 폴더다"
    );
    for dir in [&first, &second] {
        assert!(dir.root.starts_with(&root));
        for path in [&dir.root, &dir.input, &dir.work] {
            assert_eq!(mode(path), 0o700, "{path:?}");
        }
    }
    let (kept, gone) = (second.root.clone(), first.root.clone());
    drop(first);
    assert!(!gone.exists() && kept.exists(), "자기 폴더만 지운다");
    drop(second);
    assert!(!kept.exists());
}

#[test]
fn a_folder_that_already_exists_is_never_reused_or_cleaned() {
    let root = workspace("taken");
    // 다른 프로세스(재시작 전의 같은 run·세대·claim)가 남겨 둔 폴더와 이름이 겹친다
    let taken = root.join("personal-run-1-g1-c1-aaaa");
    fs::create_dir_all(taken.join("work")).unwrap();
    fs::write(taken.join("work/session.jsonl"), "남의 파일").unwrap();
    let ours = CallDir::create_with(&root, &attempt(1), fixed(&["aaaa", "aaaa", "bbbb"])).unwrap();
    assert_eq!(ours.root, root.join("personal-run-1-g1-c1-bbbb"));
    ours.write_work("session.jsonl", "내 파일").unwrap();
    assert_eq!(
        fs::read_to_string(taken.join("work/session.jsonl")).unwrap(),
        "남의 파일"
    );
    drop(ours);
    assert_eq!(
        fs::read_to_string(taken.join("work/session.jsonl")).unwrap(),
        "남의 파일",
        "자기 폴더만 정리한다"
    );
}

#[test]
fn calls_that_differ_only_in_claim_never_share_a_folder() {
    let root = workspace("claims");
    let a = CallDir::create_with(&root, &attempt(1), fixed(&["same"])).unwrap();
    let b = CallDir::create_with(&root, &attempt(2), fixed(&["same"])).unwrap();
    assert_ne!(a.root, b.root);
    a.write_work("x", "a").unwrap();
    b.write_work("x", "b").unwrap();
    assert_eq!(fs::read_to_string(a.work.join("x")).unwrap(), "a");
    drop(b);
    assert_eq!(fs::read_to_string(a.work.join("x")).unwrap(), "a");
}

#[test]
fn when_every_name_is_taken_it_fails_instead_of_reusing_one() {
    let root = workspace("full");
    let taken = root.join("personal-run-1-g1-c1-zzzz");
    fs::create_dir_all(&taken).unwrap();
    fs::write(taken.join("keep"), "x").unwrap();
    let error = CallDir::create_with(&root, &attempt(1), fixed(&["zzzz"]))
        .err()
        .expect("같은 이름만 나오면 실패한다");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(taken.join("keep").exists());
}

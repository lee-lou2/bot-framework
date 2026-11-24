//! 화면의 순수 규칙(`ui/holdpin.js` 등)을 노드로 시험한다. 노드가 없으면 건너뛴다(`BOT_REQUIRE_NODE=1`이면 없을 때 실패).
//! 브라우저 없이 돌 수 있도록 규칙은 화면 요소를 모르는 순수 함수로 두었다.

use std::{io::ErrorKind, path::Path, process::Command};

#[test]
fn the_pure_ui_rules_pass_their_node_tests() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/js");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/js 폴더")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mjs"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "노드 시험 파일이 있어야 한다");
    // TAP 형식으로 받아 «몇 건 통과·실패 0건»을 확인한다(출력 형식이 노드 판본마다 달라지지 않게 고정).
    let run = Command::new("node")
        .args(["--test", "--test-reporter=tap"])
        .args(&files)
        .output();
    match run {
        Ok(out) => {
            let (stdout, stderr) = (
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            assert!(out.status.success(), "노드 시험 실패:\n{stdout}\n{stderr}");
            assert!(stdout.contains("# fail 0"), "실패가 있다:\n{stdout}");
            let passed = stdout
                .lines()
                .find_map(|l| l.strip_prefix("# pass "))
                .and_then(|n| n.trim().parse::<u32>().ok());
            assert!(
                passed.is_some_and(|n| n >= 1),
                "통과한 시험이 없다:\n{stdout}"
            );
        }
        Err(e)
            if e.kind() == ErrorKind::NotFound
                && std::env::var_os("BOT_REQUIRE_NODE").is_none() =>
        {
            eprintln!("node가 없어 화면 규칙 시험을 건너뛴다(BOT_REQUIRE_NODE=1이면 실패로 본다)");
        }
        Err(e) => panic!("node를 실행할 수 없다: {e}"),
    }
}

//! Runs pure UI rule tests (`tests/js/*.mjs`) under Node.js; skips if `node` is absent unless `BOT_REQUIRE_NODE=1`.

use std::{io::ErrorKind, path::Path, process::Command};

#[test]
fn the_pure_ui_rules_pass_their_node_tests() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/js");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/js dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mjs"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected at least one Node test file");
    // Pin TAP output format so pass/fail parsing stays stable across Node versions.
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
            assert!(
                out.status.success(),
                "Node tests failed:\n{stdout}\n{stderr}"
            );
            assert!(stdout.contains("# fail 0"), "failures present:\n{stdout}");
            let passed = stdout
                .lines()
                .find_map(|l| l.strip_prefix("# pass "))
                .and_then(|n| n.trim().parse::<u32>().ok());
            assert!(
                passed.is_some_and(|n| n >= 1),
                "no passing tests reported:\n{stdout}"
            );
            // Assert key test titles so deleted or renamed files cannot silently stop running.
            for must in [
                "the first confirmation pins the step",
                "pause sends exactly the approval id and epoch the screen saw",
                "resume needs a second press",
                "a second click while the first request is in flight sends nothing",
                "the tab exists only for operators",
                "the audit tab names the pre-approval pause and resume actions",
                "every button variant that paints its own background has a hover rule that keeps it",
                "runs URL hash stays consistent across revisit, bare tab switch, and hashchange back",
            ] {
                assert!(stdout.contains(must), "missing UI test: {must}");
            }
        }
        Err(e)
            if e.kind() == ErrorKind::NotFound
                && std::env::var_os("BOT_REQUIRE_NODE").is_none() =>
        {
            eprintln!(
                "skipping UI rule tests because node is unavailable (set BOT_REQUIRE_NODE=1 to fail)"
            );
        }
        Err(e) => panic!("failed to run node: {e}"),
    }
}

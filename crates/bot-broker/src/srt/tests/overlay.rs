//! Linux `bwrap` overlay launcher tests verifying `/tmp/claude` tmpfs injection and cleanup.

use super::*;
use crate::{
    srt::overlay::{SHARED_TMP, find_bwrap, locate, script, write_launcher},
    testing::write_executable,
};

fn fake_bwrap(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("bwrap");
    write_executable(
        &path,
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        0o755,
    );
    path
}

fn run(real: &Path, hide: &str, args: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let launcher = dir.path().join("launcher");
    write_executable(&launcher, &script(real, hide), 0o700);
    let output = std::process::Command::new(&launcher)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn folder(root: &Path) -> String {
    let hide = root.join("shared");
    std::fs::create_dir_all(&hide).unwrap();
    hide.display().to_string()
}

#[test]
fn the_shared_tmp_is_overlaid_just_before_the_first_command_separator_and_only_there() {
    let tmp = tempfile::tempdir().unwrap();
    let real = fake_bwrap(tmp.path());
    let hide = folder(tmp.path());
    let got = run(
        &real,
        &hide,
        &[
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--",
            "/bin/sh",
            "-c",
            "echo -- hi",
            "--",
            "x",
        ],
    );
    let expected: Vec<&str> = vec![
        "--new-session",
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        &hide,
        "--remount-ro",
        &hide,
        "--",
        "/bin/sh",
        "-c",
        "echo -- hi",
        "--",
        "x",
    ];
    assert_eq!(got, expected, "overlay must appear only before first `--`");
}

#[test]
fn calls_without_a_separator_or_without_the_folder_pass_through_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let real = fake_bwrap(tmp.path());
    let hide = folder(tmp.path());
    assert_eq!(run(&real, &hide, &["--version"]), ["--version"]);
    assert_eq!(run(&real, &hide, &[]), Vec::<String>::new());
    // Skip overlaying when the folder does not exist so bwrap does not fail on a read-only root.
    let missing = tmp.path().join("absent").display().to_string();
    assert_eq!(
        run(&real, &missing, &["--ro-bind", "/", "/", "--", "true"]),
        ["--ro-bind", "/", "/", "--", "true"]
    );
}

#[test]
fn awkward_arguments_reach_bwrap_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let real = fake_bwrap(tmp.path());
    let hide = folder(tmp.path());
    let got = run(
        &real,
        &hide,
        &[
            "--setenv",
            "A",
            "it's $HOME `x` * \"q\" a b",
            "--setenv",
            "B",
            "",
            "--",
            "x y",
        ],
    );
    let expected: Vec<&str> = vec![
        "--setenv",
        "A",
        "it's $HOME `x` * \"q\" a b",
        "--setenv",
        "B",
        "",
        "--tmpfs",
        &hide,
        "--remount-ro",
        &hide,
        "--",
        "x y",
    ];
    assert_eq!(got, expected);
}

#[test]
fn a_real_bwrap_whose_path_has_quotes_and_spaces_is_still_found_by_the_launcher() {
    let tmp = tempfile::tempdir().unwrap();
    let real = fake_bwrap(&tmp.path().join("it's a dir"));
    let hide = folder(tmp.path());
    assert_eq!(
        run(&real, &hide, &["--version"]),
        ["--version"],
        "launcher must preserve quotes and spaces in real bwrap path"
    );
}

#[test]
fn bwrap_is_the_first_executable_file_named_so_on_the_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (first, second, third) = (
        tmp.path().join("a"),
        tmp.path().join("b"),
        tmp.path().join("c"),
    );
    std::fs::create_dir_all(&first).unwrap();
    std::fs::write(first.join("bwrap"), "x").unwrap();
    std::fs::create_dir_all(second.join("bwrap")).unwrap();
    let real = fake_bwrap(&third);
    let path = std::env::join_paths([&first, &second, &third])
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(find_bwrap(&path), Some(real));
    assert_eq!(find_bwrap(&first.display().to_string()), None);
    assert_eq!(find_bwrap(""), None);
}

#[test]
fn the_launcher_is_wanted_only_where_srt_cannot_hide_the_folder_and_only_when_bwrap_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let real = fake_bwrap(&tmp.path().join("bin"));
    let path = tmp.path().join("bin").display().to_string();
    assert_eq!(locate("macos", &path), None);
    assert_eq!(
        locate("linux", &tmp.path().join("none").display().to_string()),
        None
    );
    assert_eq!(locate("linux", &path), Some(real.clone()));
    let target = tmp.path().join("call.bwrap");
    write_launcher(&target, &real).unwrap();
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        script(&real, SHARED_TMP)
    );
    assert!(write_launcher(&target, &real).is_err());
}

#[tokio::test]
async fn the_launcher_is_named_in_the_srt_settings_only_on_linux_and_goes_with_the_call() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fake_bwrap(&bin);
    let mut cfg = config(tmp.path(), fake_srt(tmp.path(), PINNED_VERSION));
    cfg.path = format!("{}:/usr/bin:/bin", bin.display());
    let sandbox = SrtSandbox::open(cfg, Arc::new(CountingEntropy::default()))
        .await
        .unwrap();
    let process = sandbox.spawn(spec("r1.1.c9", false)).await.unwrap();
    let meta = tmp.path().join("work/.srt");
    let value: Value =
        serde_json::from_str(&std::fs::read_to_string(one_file(&meta, "json")).unwrap()).unwrap();
    if cfg!(target_os = "linux") {
        let launcher = one_file(&meta, "bwrap");
        assert_eq!(value["bwrapPath"], json!(launcher.display().to_string()));
        assert_eq!(
            std::fs::metadata(&launcher).unwrap().permissions().mode() & 0o777,
            0o700
        );
    } else {
        assert!(value.get("bwrapPath").is_none(), "{value}");
    }
    assert_eq!(
        value["filesystem"]["denyWrite"],
        json!([SHARED_TMP, "/private/tmp/claude"])
    );
    Box::new(process.handle).terminate().await.unwrap();
    let left: Vec<String> = leftovers(tmp.path());
    assert!(left.is_empty(), "{left:?}");
}

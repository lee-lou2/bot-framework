use super::*;
use std::os::unix::net::UnixListener;

struct Temp(PathBuf);
impl Temp {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bot-profile-scan-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn codes(issues: &Issues) -> Vec<&'static str> {
    issues.items().iter().map(|i| i.code).collect()
}

fn scan(dir: &Path, limits: &Limits) -> (Tree, Issues) {
    let mut issues = Issues::new(100);
    let tree = read_tree(dir, "p", limits, &mut issues);
    (tree, issues)
}

#[test]
fn reads_regular_files_and_empty_dirs_and_skips_cruft() {
    let t = Temp::new("ok");
    fs::create_dir_all(t.0.join("a/b")).unwrap();
    fs::create_dir_all(t.0.join(".git")).unwrap();
    fs::write(t.0.join("a/x.md"), "hi").unwrap();
    fs::write(t.0.join(".DS_Store"), "junk").unwrap();
    let (tree, issues) = scan(&t.0, &Limits::default());
    assert!(!issues.has_errors(), "{:?}", issues.items());
    assert_eq!(tree.files.keys().collect::<Vec<_>>(), ["a/x.md"]);
    assert!(tree.dirs.contains("a/b") && !tree.dirs.contains(".git"));
    assert_eq!(tree.child_dirs("a"), ["b"]);
    assert_eq!(tree.child_files("a"), ["x.md"]);
}

#[test]
fn rejects_symlinks_without_following() {
    let t = Temp::new("link");
    fs::write(t.0.join("real.md"), "x").unwrap();
    std::os::unix::fs::symlink("real.md", t.0.join("alias.md")).unwrap();
    std::os::unix::fs::symlink("/etc", t.0.join("etc")).unwrap();
    std::os::unix::fs::symlink(".", t.0.join("loop")).unwrap();
    let (tree, issues) = scan(&t.0, &Limits::default());
    assert_eq!(codes(&issues), ["symlink", "symlink", "symlink"]);
    assert_eq!(tree.files.keys().collect::<Vec<_>>(), ["real.md"]);
    assert!(issues.items()[0].path.starts_with("p/"));
}

#[test]
fn rejects_special_files_and_hardlinks() {
    let t = Temp::new("special");
    let _sock = UnixListener::bind(t.0.join("s.sock")).unwrap();
    fs::write(t.0.join("a.md"), "x").unwrap();
    fs::hard_link(t.0.join("a.md"), t.0.join("b.md")).unwrap();
    let (_, issues) = scan(&t.0, &Limits::default());
    let mut got = codes(&issues);
    got.sort_unstable();
    assert_eq!(got, ["hardlink", "hardlink", "special-file"]);
}

#[test]
fn enforces_limits() {
    let t = Temp::new("limits");
    fs::write(t.0.join("big.md"), vec![b'a'; 20]).unwrap();
    let tight = Limits {
        max_file_bytes: 10,
        ..Limits::default()
    };
    assert_eq!(codes(&scan(&t.0, &tight).1), ["too-large"]);

    let t = Temp::new("count");
    for i in 0..5 {
        fs::write(t.0.join(format!("f{i}.md")), "x").unwrap();
    }
    let few = Limits {
        max_files: 3,
        ..Limits::default()
    };
    assert!(codes(&scan(&t.0, &few).1).contains(&"too-many-files"));

    let t = Temp::new("deep");
    fs::create_dir_all(t.0.join("a/b/c/d")).unwrap();
    fs::write(t.0.join("a/b/c/d/x.md"), "x").unwrap();
    let shallow = Limits {
        max_depth: 3,
        ..Limits::default()
    };
    assert!(codes(&scan(&t.0, &shallow).1).contains(&"too-deep"));

    let t = Temp::new("total");
    fs::write(t.0.join("a.md"), vec![b'a'; 8]).unwrap();
    fs::write(t.0.join("b.md"), vec![b'a'; 8]).unwrap();
    let small = Limits {
        max_profile_bytes: 10,
        ..Limits::default()
    };
    assert!(codes(&scan(&t.0, &small).1).contains(&"too-large-total"));
}

#[test]
fn rejects_bad_names() {
    let t = Temp::new("names");
    fs::write(t.0.join("a\u{202e}b.md"), "x").unwrap();
    fs::write(t.0.join(" lead.md"), "x").unwrap();
    let (_, issues) = scan(&t.0, &Limits::default());
    assert_eq!(codes(&issues), ["bad-name", "bad-name"]);
}

#[test]
fn lists_profiles_and_rejects_stray_entries() {
    let t = Temp::new("root");
    fs::create_dir_all(t.0.join("personal")).unwrap();
    fs::create_dir_all(t.0.join("Bad_Name")).unwrap();
    fs::write(t.0.join("README.md"), "x").unwrap();
    let target = Temp::new("elsewhere");
    std::os::unix::fs::symlink(&target.0, t.0.join("linked")).unwrap();
    let mut issues = Issues::new(100);
    let found = list_profiles(&t.0, &Limits::default(), &mut issues);
    assert_eq!(
        found.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
        ["personal"]
    );
    let mut got = codes(&issues);
    got.sort_unstable();
    assert_eq!(got, ["bad-profile-name", "stray-file", "symlink"]);
}

#[test]
fn root_may_be_a_link_but_a_profile_dir_as_root_is_refused() {
    let t = Temp::new("rootlink");
    fs::create_dir_all(t.0.join("real/personal")).unwrap();
    std::os::unix::fs::symlink(t.0.join("real"), t.0.join("current")).unwrap();
    let mut issues = Issues::new(100);
    let found = list_profiles(&t.0.join("current"), &Limits::default(), &mut issues);
    assert_eq!(found.len(), 1, "{:?}", issues.items());

    fs::write(t.0.join("real/personal/profile.yaml"), "x").unwrap();
    let mut issues = Issues::new(100);
    list_profiles(&t.0.join("real/personal"), &Limits::default(), &mut issues);
    assert_eq!(codes(&issues), ["root-is-profile"]);
}

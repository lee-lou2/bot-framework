//! Pins the served admin UI asset list so additions/removals are explicit and unapproved or temp files are never exposed.

use crate::support::*;
use bot_admin::fake::Fake;
use std::{collections::BTreeSet, fs, path::Path};

/// Served filenames (`index.html` first at `/ui`, remaining entries sorted).
const EXPECTED: &[&str] = &[
    "index.html",
    "access.js",
    "api.js",
    "app.css",
    "app.js",
    "audit.js",
    "camera.js",
    "decide.css",
    "decide.js",
    "dom.js",
    "holdpin.js",
    "labels.js",
    "layout.js",
    "map.css",
    "map.js",
    "needs.js",
    "panel.js",
    "preapproval.js",
    "preapprovals.css",
    "preapprovals.js",
    "runctl.js",
    "runs.css",
    "runs.js",
    "status.js",
    "store.js",
    "structure.js",
    "views.css",
];

fn served() -> Vec<&'static str> {
    bot_admin::assets::files()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn the_served_file_list_is_pinned() {
    assert_eq!(
        served(),
        EXPECTED,
        "served asset list changed: update EXPECTED if intentional"
    );
}

#[test]
fn only_allowed_kinds_with_plain_names_are_ever_served() {
    for name in served() {
        let ext = name.rsplit_once('.').map(|(_, e)| e);
        assert!(
            matches!(ext, Some("html" | "css" | "js" | "svg")),
            "disallowed extension: {name}"
        );
        assert!(
            !name.starts_with('.') && !name.contains('/') && !name.contains(".."),
            "{name}"
        );
        assert!(
            name.bytes().all(|c| c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, b'.' | b'_' | b'-')),
            "{name}"
        );
        for temp in [".bak", ".orig", ".rej", ".swp", ".swx", ".tmp"] {
            assert!(
                !name.ends_with(temp) && !name.ends_with('~'),
                "temporary or backup file: {name}"
            );
        }
    }
    assert_eq!(served().first(), Some(&"index.html"));
    let unique: BTreeSet<_> = served().into_iter().collect();
    assert_eq!(unique.len(), served().len(), "duplicate asset name");
}

/// Cross-checks `build.rs` by re-applying the serving rules directly to `ui/` on disk.
#[test]
fn the_folder_and_the_served_list_agree_under_the_serving_rules() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut on_disk: Vec<String> = fs::read_dir(&dir)
        .expect("ui dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .filter(|n| {
            n.rsplit_once('.')
                .is_some_and(|(_, e)| matches!(e, "html" | "css" | "js" | "svg"))
        })
        .filter(|n| {
            n.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
            })
        })
        .collect();
    on_disk.sort_by_key(|n| (n != "index.html", n.clone()));
    assert_eq!(on_disk, served());
}

#[tokio::test]
async fn nothing_outside_the_list_is_reachable_and_traversal_is_closed() {
    let s = start(Fake::demo()).await;
    for name in EXPECTED.iter().skip(1) {
        let r = get(&s, OPERATOR, &format!("/ui/{name}")).await;
        assert_eq!(r.status, 200, "{name}");
    }
    assert_eq!(get(&s, OPERATOR, "/ui").await.status, 200);
    for path in [
        "/ui/index.html",
        "/ui/.env",
        "/ui/.hidden.js",
        "/ui/build.rs",
        "/ui/notes.txt",
        "/ui/sub/inner.js",
        "/ui/..%2Fsrc%2Flib.rs",
        "/ui/%2e%2e/Cargo.toml",
        "/ui/..%2f..%2fCargo.toml",
        "/ui//etc/passwd",
        "/ui/app.js%00.png",
        "/ui/App.JS",
    ] {
        assert_eq!(get(&s, OPERATOR, path).await.status, 404, "{path}");
    }
    // Unauthenticated requests must not distinguish existing from missing paths.
    assert_eq!(
        raw(s.addr, "GET", "/ui/app.js", None, &[], None)
            .await
            .status,
        401
    );
    assert_eq!(
        raw(s.addr, "GET", "/ui/build.rs", None, &[], None)
            .await
            .status,
        401
    );
}

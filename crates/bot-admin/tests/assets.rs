//! 서빙하는 화면 자산 목록을 고정한다. 파일이 늘거나 줄면 이 시험이 알려 준다 — `EXPECTED` 갱신은 의식적인 변경이다.
//! 자산 표는 빌드 때 `ui/` 폴더에서 만들어지지만(`build.rs`), 의도하지 않은 파일이 서빙되면 안 된다:
//! 허용 확장자(html·css·js·svg)와 이름 규칙을 지키는 최상위 파일만 나가고 숨김·임시·백업 파일은 나가지 않는다.

mod support;
use bot_admin::fake::Fake;
use std::{collections::BTreeSet, fs, path::Path};
use support::*;

/// 서빙하는 파일 이름. 첫 항목은 `/ui`(index.html)이고 나머지는 이름순이다.
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
        "서빙 목록이 바뀌었다: 의도한 변경이면 EXPECTED를 갱신하라"
    );
}

#[test]
fn only_allowed_kinds_with_plain_names_are_ever_served() {
    for name in served() {
        let ext = name.rsplit_once('.').map(|(_, e)| e);
        assert!(
            matches!(ext, Some("html" | "css" | "js" | "svg")),
            "허용하지 않는 형식: {name}"
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
                "임시·백업 파일: {name}"
            );
        }
    }
    assert_eq!(served().first(), Some(&"index.html"));
    let unique: BTreeSet<_> = served().into_iter().collect();
    assert_eq!(unique.len(), served().len(), "이름이 겹친다");
}

/// 디스크의 `ui/`에서 서빙 규칙을 만족하는 파일만 모으면 서빙 목록과 같다(규칙을 build.rs와 따로 한 번 더 적용해 대조한다).
#[test]
fn the_folder_and_the_served_list_agree_under_the_serving_rules() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut on_disk: Vec<String> = fs::read_dir(&dir)
        .expect("ui 폴더")
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
    // 인증 없이는 어느 경로도 목록을 알려 주지 않는다.
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

//! 관리 화면 자산 규칙: 줄 수·금지 문자열·참조 일치·인증과 보안 헤더·캐시·폴더와의 일치.
//! 화면은 빌드 단계 없는 정적 HTML·CSS·JS이고 바이너리에 내장된다. 공개 저장소의 규칙을 시험으로 잠근다.

mod support;
use bot_admin::{assets, fake::Fake};
use std::{collections::BTreeSet, path::Path};
use support::*;

/// 공개 CI(`check_contracts.py`)와 같은 상한이다.
const MAX_LINES: usize = 600;
/// 어느 줄에도 있으면 안 되는 문자열(주석 포함): 외부 주소와 HTML 문자열 삽입·동적 실행 API.
const BANNED: [&str; 7] = [
    "http://",
    "https://",
    "innerHTML",
    "eval(",
    "document.write",
    "outerHTML",
    "insertAdjacentHTML",
];
// base64("anyone:operator-secret-1"): 브라우저 로그인 창이 보내는 Basic 자격(사용자 이름은 무엇이든 된다)
const BASIC_OPERATOR: &str = "Basic YW55b25lOm9wZXJhdG9yLXNlY3JldC0x";
// base64("anyone:wrong-secret")
const BASIC_WRONG: &str = "Basic YW55b25lOndyb25nLXNlY3JldA==";

fn text<'a>(path: &str, bytes: &'a [u8]) -> &'a str {
    std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("{path}: UTF-8이 아니다"))
}

fn index_html() -> &'static str {
    let (path, bytes) = assets::files()
        .into_iter()
        .next()
        .expect("자산이 하나 이상 있어야 한다");
    assert_eq!(path, "index.html", "첫 항목이 /ui여야 한다");
    text(path, bytes)
}

/// 주소: 첫 항목(`index.html`)은 `/ui`, 나머지는 `/ui/<경로>`.
fn url_of(path: &str) -> String {
    if path == "index.html" {
        "/ui".to_owned()
    } else {
        format!("/ui/{path}")
    }
}

fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        other => panic!("{path}: 알 수 없는 형식 {other:?}"),
    }
}

fn assert_secure(r: &Resp, what: &str) {
    assert_eq!(
        r.header("content-security-policy"),
        Some(assets::CSP),
        "{what}: CSP"
    );
    assert_eq!(
        r.header("x-content-type-options"),
        Some("nosniff"),
        "{what}: nosniff"
    );
    assert_eq!(
        r.header("referrer-policy"),
        Some("no-referrer"),
        "{what}: Referrer-Policy"
    );
    assert_eq!(
        r.header("x-frame-options"),
        Some("DENY"),
        "{what}: X-Frame-Options"
    );
}

/// `<이름 ...>` 여는 태그(닫는 `>`까지) 모음.
fn open_tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let needle = format!("<{name}");
    html.match_indices(&needle)
        .filter(|(i, _)| {
            html[i + needle.len()..].starts_with(|c: char| c.is_whitespace() || c == '>')
        })
        .map(|(i, _)| {
            let rest = &html[i..];
            &rest[..rest.find('>').map_or(rest.len(), |e| e + 1)]
        })
        .collect()
}

/// 여는 태그 안의 `이름="값"` 속성 값(앞에 공백이 있는 속성만).
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)?;
    tag[at + key.len()..].split('"').next()
}

#[test]
fn every_asset_stays_within_the_public_line_limit() {
    for (path, bytes) in assets::files() {
        // 파일을 한 줄씩 세는 공개 CI와 같은 방식: 줄바꿈 수 + 줄바꿈 없는 마지막 줄.
        let newlines = bytes.iter().filter(|b| **b == b'\n').count();
        let lines = newlines + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
        assert!(lines <= MAX_LINES, "{path}: {lines}줄 (상한 {MAX_LINES})");
    }
}

#[test]
fn assets_never_contain_remote_urls_or_unsafe_dom_apis() {
    for (path, bytes) in assets::files() {
        for (n, line) in text(path, bytes).lines().enumerate() {
            for word in BANNED {
                assert!(!line.contains(word), "{path}:{}: `{word}`", n + 1);
            }
        }
    }
}

#[test]
fn assets_carry_no_personal_paths() {
    for (path, bytes) in assets::files() {
        let body = text(path, bytes);
        for marker in ["/Users/", "/home/", "C:\\Users"] {
            assert!(!body.contains(marker), "{path}: 개인 경로 `{marker}`");
        }
    }
}

#[test]
fn scripts_never_build_markup_or_style_attributes_from_strings() {
    for (path, bytes) in assets::files() {
        if !path.ends_with(".js") {
            continue;
        }
        for (n, line) in text(path, bytes).lines().enumerate() {
            for quote in ['"', '\'', '`'] {
                for tag in line.match_indices(&format!("{quote}<")) {
                    let next = line[tag.0 + 2..].chars().next();
                    assert!(
                        !next.is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!'),
                        "{path}:{}: HTML 문자열을 만들면 안 된다",
                        n + 1
                    );
                }
            }
            for word in ["setAttribute(\"style\"", "setAttribute('style'", "style=\""] {
                assert!(
                    !line.contains(word),
                    "{path}:{}: 인라인 style은 CSP가 막는다(`{word}`)",
                    n + 1
                );
            }
        }
    }
}

/// 보류 카드는 같은 호출 ID 아래 도구·회신 보류를 따로 보여야 하고(키에 종류), 식별자 없는 대상은 판정을 보내지 않는다.
/// 화면 동작은 `tests/browser/hold_pair.mjs`(수동)와 `tests/js`(순수 규칙)가 확인하고, 여기서는 그 연결이 끊기지 않게 잠근다.
#[test]
fn the_hold_screen_keys_cards_by_kind_and_never_sends_a_judgment_without_a_hold_id() {
    let decide = assets::files()
        .into_iter()
        .find(|(path, _)| *path == "decide.js")
        .map(|(path, bytes)| text(path, bytes))
        .expect("decide.js");
    assert!(
        decide.contains("const holdKey = P.keyOf;"),
        "카드·폼 키는 종류까지 넣는 공유 규칙(holdpin.js의 keyOf)이어야 한다"
    );
    assert!(
        !decide.contains(r#"x.profile + "|" + x.run + "|" + x.call"#),
        "호출 ID만으로 만든 키는 같은 호출의 도구·회신 보류를 한 카드로 합친다"
    );
    for guard in [
        "p.btn.disabled = f.busy || blocked ||",
        "f.holdId === null || f.holdId === undefined",
        "P.WHY.noid",
    ] {
        assert!(
            decide.contains(guard),
            "식별자 없는 대상 규칙이 빠졌다: {guard}"
        );
    }
}

/// 예약 전송 카드(`schedule.deliver`)의 초안은 한 글자도 고치지 않은 원문으로(줄만 접어) 보이고, 볼 수 없으면 승인 단추가 꺼진다.
/// 화면 동작은 브라우저 점검이, 서버 규칙은 `tests/delivery_card.rs`가 맡고 여기서는 그 연결이 끊기지 않게 잠근다.
#[test]
fn the_delivery_card_shows_the_draft_verbatim_and_explains_why_a_hidden_one_cannot_be_approved() {
    let file = |name: &str| {
        assets::files()
            .into_iter()
            .find(|(path, _)| *path == name)
            .map(|(path, bytes)| text(path, bytes))
            .unwrap_or_else(|| panic!("{name}"))
    };
    let (decide, css, api) = (file("decide.js"), file("decide.css"), file("api.js"));
    for needle in [
        r#"const DRAFT = "전송 초안";"#,
        r#"l.label === DRAFT ? h("pre", { class: "ac-draft""#,
        r#""전송 초안 전체""#,
    ] {
        assert!(decide.contains(needle), "초안 표시 연결이 빠졌다: {needle}");
    }
    assert!(
        !decide.contains("JSON.stringify(JSON.parse"),
        "초안을 다시 서식화하면 승인 지문의 입력과 달라진다"
    );
    assert!(
        css.contains(".ac-draft") && css.contains("white-space: pre-wrap"),
        "원문을 접어 보이는 규칙"
    );
    assert!(
        !decide.contains("이 기억의 소유자"),
        "소유자 전용 안내는 도구에 맞춰 일반 문구여야 한다"
    );
    assert!(
        api.contains("승인 지문과 일치하는 내용 전체"),
        "서버 거절 안내도 일반 문구여야 한다"
    );
}

#[test]
fn the_page_carries_no_inline_script_style_or_handler_attributes() {
    let html = index_html();
    assert!(
        open_tags(html, "style").is_empty(),
        "<style> 태그가 있다(CSP가 막는다)"
    );
    for (i, _) in html.match_indices("<script") {
        let rest = &html[i..];
        let (open, close) = (rest.find('>').expect("여는 태그"), rest.find("</script>"));
        assert_eq!(
            close,
            Some(open + 1),
            "스크립트 본문이 있다(CSP가 막는다): {}",
            &rest[..open]
        );
        assert!(attr(&rest[..=open], "src").is_some(), "src 없는 script");
    }
    assert!(!html.contains(" style="), "style 속성이 있다");
    assert!(!html.contains("javascript:"), "javascript: 주소가 있다");
    // 이벤트 처리 속성(` onclick=` 꼴)이 없다.
    let bytes = html.as_bytes();
    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i].is_ascii_whitespace() && &bytes[i + 1..i + 3] == b"on" {
            let name: String = bytes[i + 3..]
                .iter()
                .take_while(|b| b.is_ascii_lowercase())
                .map(|b| char::from(*b))
                .collect();
            let after = bytes.get(i + 3 + name.len());
            assert!(
                name.is_empty() || after != Some(&b'='),
                "이벤트 속성 on{name}= 이 있다"
            );
        }
    }
}

#[test]
fn the_page_loads_every_asset_once_and_nothing_else() {
    let html = index_html();
    let table: Vec<&str> = assets::files().into_iter().map(|(p, _)| p).collect();
    let unique: BTreeSet<&str> = table.iter().copied().collect();
    assert_eq!(unique.len(), table.len(), "자산 경로가 겹친다: {table:?}");
    let mut used: Vec<String> = Vec::new();
    let mut local = |url: &str, tag: &str| {
        used.push(
            url.strip_prefix("/ui/")
                .unwrap_or_else(|| panic!("자산이 아닌 주소 `{url}`: {tag}"))
                .to_owned(),
        );
    };
    for tag in open_tags(html, "script") {
        local(attr(tag, "src").expect("src"), tag);
    }
    for tag in open_tags(html, "link") {
        match attr(tag, "rel") {
            Some("stylesheet") => local(attr(tag, "href").expect("href"), tag),
            // 아이콘은 빈 data 주소다(요청을 만들지 않는다).
            Some("icon") => assert!(
                attr(tag, "href").is_some_and(|h| h.starts_with("data:")),
                "아이콘이 외부 주소다: {tag}"
            ),
            _ => panic!("알 수 없는 link: {tag}"),
        }
    }
    for url in &used {
        assert!(unique.contains(url.as_str()), "{url}: 자산 표에 없다");
    }
    let once: BTreeSet<&str> = used.iter().map(String::as_str).collect();
    assert_eq!(
        once.len(),
        used.len(),
        "같은 자산을 두 번 불러온다: {used:?}"
    );
    for path in table.iter().skip(1) {
        assert!(once.contains(path), "{path}: index.html이 쓰지 않는 자산");
    }
}

#[test]
fn the_csp_allows_only_same_origin_code_and_requests() {
    let csp = assets::CSP;
    for need in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "img-src 'self' data:",
        "connect-src 'self'",
    ] {
        assert!(csp.contains(need), "CSP에 `{need}`가 있어야 한다: {csp}");
    }
    for banned in ["unsafe-inline", "unsafe-eval", "http:", "https:", "*"] {
        assert!(!csp.contains(banned), "CSP에 `{banned}`가 있다: {csp}");
    }
}

#[tokio::test]
async fn the_page_and_every_asset_answer_after_login_with_security_headers() {
    let s = start(Fake::demo()).await;
    // 화면은 로그인한 모든 주체에게 열린다(운영자 전용 탭은 화면 안에서 숨긴다).
    for token in [OPERATOR, GUEST] {
        for (path, bytes) in assets::files() {
            let url = url_of(path);
            let r = get(&s, token, &url).await;
            assert_eq!(r.status, 200, "{url}");
            assert_secure(&r, &url);
            assert_eq!(r.header("content-type"), Some(mime_of(path)), "{url}");
            assert!(
                r.header("etag").is_some_and(|t| t.starts_with('"')),
                "{url}: ETag"
            );
            assert_eq!(
                r.body,
                text(path, bytes),
                "{url}: 내려준 내용이 자산과 같아야 한다"
            );
        }
    }
}

/// 운영자로 로그인해 GET한다(조건부 요청 머리글을 더할 수 있다).
async fn get_with(s: &Server, path: &str, extra: &[(&str, &str)]) -> Resp {
    let auth = format!("Bearer {OPERATOR}");
    let mut headers = vec![("Authorization", auth.as_str())];
    headers.extend_from_slice(extra);
    raw(s.addr, "GET", path, None, &headers, None).await
}

#[tokio::test]
async fn a_matching_etag_gets_not_modified_and_a_stale_one_gets_the_page() {
    let s = start(Fake::demo()).await;
    for (path, _) in assets::files() {
        let url = url_of(path);
        let tag = get(&s, OPERATOR, &url)
            .await
            .header("etag")
            .expect("ETag")
            .to_owned();
        let same = get_with(&s, &url, &[("If-None-Match", &tag)]).await;
        assert_eq!(same.status, 304, "{url}");
        assert!(same.body.is_empty(), "{url}: 304는 본문이 없다");
        assert_secure(&same, &url);
        assert_eq!(same.header("etag"), Some(tag.as_str()), "{url}");
        let weak = get_with(&s, &url, &[("If-None-Match", &format!("W/{tag}"))]).await;
        assert_eq!(weak.status, 304, "{url}: 약한 표시도 같은 내용이다");
        let stale = get_with(&s, &url, &[("If-None-Match", "\"0000\"")]).await;
        assert_eq!(stale.status, 200, "{url}: 다른 표시면 새로 내려준다");
    }
}

#[tokio::test]
async fn login_is_required_and_unknown_paths_are_not_found() {
    let s = start(Fake::demo()).await;
    for url in ["/ui", "/ui/app.js", "/ui/missing.js"] {
        let none = raw(s.addr, "GET", url, None, &[], None).await;
        assert_eq!(none.status, 401, "{url}: 로그인 없이는 열리지 않는다");
        assert!(
            none.header("www-authenticate")
                .is_some_and(|v| v.starts_with("Basic")),
            "{url}: 브라우저가 로그인 창을 띄울 수 있어야 한다"
        );
        assert!(
            !none.body.contains("<html"),
            "{url}: 화면 내용이 새지 않는다"
        );
    }
    let wrong = [("Authorization", BASIC_WRONG)];
    assert_eq!(
        raw(s.addr, "GET", "/ui", None, &wrong, None).await.status,
        401
    );
    // 브라우저 로그인 창과 같은 Basic 자격으로 화면과 자산이 열린다.
    let basic = [("Authorization", BASIC_OPERATOR)];
    for url in ["/ui", "/ui/app.js"] {
        assert_eq!(
            raw(s.addr, "GET", url, None, &basic, None).await.status,
            200,
            "{url}"
        );
    }
    // 표에 없는 경로는 모두 404다(폴더 밖을 읽는 경로·숨김 파일·없는 파일).
    for url in [
        "/ui/missing.js",
        "/ui/../src/lib.rs",
        "/ui/..%2Fsrc%2Flib.rs",
        "/ui/dir/app.js",
        "/ui/app.js.map",
        "/ui/.env",
    ] {
        let r = get(&s, OPERATOR, url).await;
        assert_eq!(r.status, 404, "{url}");
        assert_eq!(r.json()["error"], "not_found", "{url}");
    }
}

#[test]
fn the_asset_table_matches_the_files_on_disk() {
    // 표는 빌드 때 폴더에서 만든다(`build.rs`). 숨김 파일은 표에 넣지 않는다.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut disk = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).expect("ui 폴더") {
        let entry = entry.expect("항목");
        let name = entry.file_name().into_string().expect("이름");
        if name.starts_with('.') {
            continue;
        }
        assert!(
            entry.path().is_file(),
            "ui 안의 하위 폴더 `{name}`은 낼 수 없다"
        );
        disk.insert(name);
    }
    let table: BTreeSet<String> = assets::files()
        .iter()
        .map(|(p, _)| (*p).to_owned())
        .collect();
    let missing: Vec<_> = disk.difference(&table).collect();
    let stale: Vec<_> = table.difference(&disk).collect();
    assert!(missing.is_empty(), "표에 빠진 파일: {missing:?}");
    assert!(stale.is_empty(), "폴더에 없는 자산: {stale:?}");
    assert_eq!(assets::files().len(), table.len(), "자산 경로가 겹친다");
}

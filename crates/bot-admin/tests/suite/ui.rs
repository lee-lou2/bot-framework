//! Admin UI embedded asset contracts: line limits, CSP/DOM safety, auth, ETag caching, and disk parity.

use crate::support::*;
use bot_admin::{assets, fake::Fake};
use std::{collections::BTreeSet, path::Path};

const MAX_LINES: usize = 600;
/// Forbidden substrings anywhere in `ui/` assets (including comments): remote URLs and unsafe DOM APIs.
const BANNED: [&str; 7] = [
    "http://",
    "https://",
    "innerHTML",
    "eval(",
    "document.write",
    "outerHTML",
    "insertAdjacentHTML",
];
// base64("anyone:operator-secret-1")
const BASIC_OPERATOR: &str = "Basic YW55b25lOm9wZXJhdG9yLXNlY3JldC0x";
// base64("anyone:wrong-secret")
const BASIC_WRONG: &str = "Basic YW55b25lOndyb25nLXNlY3JldA==";

fn text<'a>(path: &str, bytes: &'a [u8]) -> &'a str {
    std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("{path}: invalid UTF-8"))
}

fn index_html() -> &'static str {
    let (path, bytes) = assets::files()
        .into_iter()
        .next()
        .expect("at least one asset");
    assert_eq!(path, "index.html", "first asset must serve /ui");
    text(path, bytes)
}

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
        other => panic!("{path}: unknown extension {other:?}"),
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

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)?;
    tag[at + key.len()..].split('"').next()
}

#[test]
fn every_asset_stays_within_the_public_line_limit() {
    for (path, bytes) in assets::files() {
        // Match check_contracts.py line counting: newline count plus any unterminated trailing line.
        let newlines = bytes.iter().filter(|b| **b == b'\n').count();
        let lines = newlines + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
        assert!(
            lines <= MAX_LINES,
            "{path}: {lines} lines (max {MAX_LINES})"
        );
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
            assert!(!body.contains(marker), "{path}: personal path `{marker}`");
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
                        "{path}:{}: must not construct HTML strings",
                        n + 1
                    );
                }
            }
            for word in ["setAttribute(\"style\"", "setAttribute('style'", "style=\""] {
                assert!(
                    !line.contains(word),
                    "{path}:{}: inline style is blocked by CSP (`{word}`)",
                    n + 1
                );
            }
        }
    }
}

/// Locks `decide.js` to kind-scoped card keys (`BotHoldPin.keyOf`) and the missing-`hold_id` submit guard.
#[test]
fn the_hold_screen_keys_cards_by_kind_and_never_sends_a_judgment_without_a_hold_id() {
    let decide = assets::files()
        .into_iter()
        .find(|(path, _)| *path == "decide.js")
        .map(|(path, bytes)| text(path, bytes))
        .expect("decide.js");
    assert!(
        decide.contains("const holdKey = P.keyOf;"),
        "hold cards must key by kind via holdpin.js keyOf"
    );
    assert!(
        !decide.contains(r#"x.profile + "|" + x.run + "|" + x.call"#),
        "omitting hold kind collapses paired tool and reply holds into one card"
    );
    for guard in [
        "p.btn.disabled = f.busy || blocked ||",
        "f.holdId === null || f.holdId === undefined",
        "P.WHY.noid",
    ] {
        assert!(
            decide.contains(guard),
            "missing hold_id guard removed: {guard}"
        );
    }
}

/// Locks `schedule.deliver` verbatim draft rendering and generic owner-only/blocked copy across UI assets.
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
    let draft_const = format!(
        r#"const DRAFT = "{}";"#,
        bot_core::schedule::DELIVERY_CARD_LABEL
    );
    for needle in [
        draft_const.as_str(),
        r#"l.label === DRAFT ? h("pre", { class: "ac-draft""#,
    ] {
        assert!(
            decide.contains(needle),
            "missing draft rendering wiring: {needle}"
        );
    }
    let body_why = decide
        .lines()
        .find(|l| l.contains("a.approve_blocked"))
        .expect("approve_blocked explanation in decide.js");
    assert!(
        body_why.contains(r#"a.tool === "schedule.deliver""#)
            && body_why.to_ascii_lowercase().contains("draft"),
        "approve_blocked explanation must cover delivery drafts: {body_why}"
    );
    assert!(
        !decide.contains("JSON.stringify(JSON.parse"),
        "reformatting the draft would break digest equivalence"
    );
    assert!(
        css.contains(".ac-draft") && css.contains("white-space: pre-wrap"),
        "CSS must wrap verbatim draft text"
    );
    let hidden_line = decide
        .lines()
        .find(|l| l.contains("a.render_hidden"))
        .expect("render_hidden notice in decide.js");
    assert!(
        !hidden_line.to_ascii_lowercase().contains("memory"),
        "owner-only notice must stay generic across tools: {hidden_line}"
    );
    let no_body = api
        .split_once("no_body:")
        .and_then(|(_, rest)| rest.split('"').nth(1))
        .expect("DECIDE.no_body message in api.js");
    assert!(
        !no_body.is_empty() && !no_body.to_ascii_lowercase().contains("memory"),
        "server no_body error copy must stay generic: {no_body}"
    );
}

#[test]
fn the_page_carries_no_inline_script_style_or_handler_attributes() {
    let html = index_html();
    assert!(
        open_tags(html, "style").is_empty(),
        "inline <style> is blocked by CSP"
    );
    for (i, _) in html.match_indices("<script") {
        let rest = &html[i..];
        let (open, close) = (rest.find('>').expect("open tag"), rest.find("</script>"));
        assert_eq!(
            close,
            Some(open + 1),
            "inline script body is blocked by CSP: {}",
            &rest[..open]
        );
        assert!(attr(&rest[..=open], "src").is_some(), "script without src");
    }
    assert!(!html.contains(" style="), "inline style attribute");
    assert!(!html.contains("javascript:"), "javascript: URL");
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
                "inline event attribute on{name}="
            );
        }
    }
}

#[test]
fn the_page_loads_every_asset_once_and_nothing_else() {
    let html = index_html();
    let table: Vec<&str> = assets::files().into_iter().map(|(p, _)| p).collect();
    let unique: BTreeSet<&str> = table.iter().copied().collect();
    assert_eq!(
        unique.len(),
        table.len(),
        "duplicate asset paths: {table:?}"
    );
    let mut used: Vec<String> = Vec::new();
    let mut local = |url: &str, tag: &str| {
        used.push(
            url.strip_prefix("/ui/")
                .unwrap_or_else(|| panic!("non-asset URL `{url}`: {tag}"))
                .to_owned(),
        );
    };
    for tag in open_tags(html, "script") {
        local(attr(tag, "src").expect("src"), tag);
    }
    for tag in open_tags(html, "link") {
        match attr(tag, "rel") {
            Some("stylesheet") => local(attr(tag, "href").expect("href"), tag),
            Some("icon") => assert!(
                attr(tag, "href").is_some_and(|h| h.starts_with("data:")),
                "icon must be a data: URL: {tag}"
            ),
            _ => panic!("unknown link tag: {tag}"),
        }
    }
    for url in &used {
        assert!(
            unique.contains(url.as_str()),
            "{url}: missing from asset table"
        );
    }
    let once: BTreeSet<&str> = used.iter().map(String::as_str).collect();
    assert_eq!(once.len(), used.len(), "asset loaded twice: {used:?}");
    for path in table.iter().skip(1) {
        assert!(once.contains(path), "{path}: unused by index.html");
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
        assert!(csp.contains(need), "CSP missing `{need}`: {csp}");
    }
    for banned in ["unsafe-inline", "unsafe-eval", "http:", "https:", "*"] {
        assert!(!csp.contains(banned), "CSP contains `{banned}`: {csp}");
    }
}

#[tokio::test]
async fn the_page_and_every_asset_answer_after_login_with_security_headers() {
    let s = start(Fake::demo()).await;
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
            assert_eq!(r.body, text(path, bytes), "{url}: body mismatch");
        }
    }
}

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
        assert!(same.body.is_empty(), "{url}: 304 must have empty body");
        assert_secure(&same, &url);
        assert_eq!(same.header("etag"), Some(tag.as_str()), "{url}");
        let weak = get_with(&s, &url, &[("If-None-Match", &format!("W/{tag}"))]).await;
        assert_eq!(weak.status, 304, "{url}: weak ETag must match");
        let stale = get_with(&s, &url, &[("If-None-Match", "\"0000\"")]).await;
        assert_eq!(stale.status, 200, "{url}: mismatched ETag must return 200");
    }
}

#[tokio::test]
async fn login_is_required_and_unknown_paths_are_not_found() {
    let s = start(Fake::demo()).await;
    for url in ["/ui", "/ui/app.js", "/ui/missing.js"] {
        let none = raw(s.addr, "GET", url, None, &[], None).await;
        assert_eq!(none.status, 401, "{url}: auth required");
        assert!(
            none.header("www-authenticate")
                .is_some_and(|v| v.starts_with("Basic")),
            "{url}: must advertise Basic auth"
        );
        assert!(!none.body.contains("<html"), "{url}: must not leak markup");
    }
    let wrong = [("Authorization", BASIC_WRONG)];
    assert_eq!(
        raw(s.addr, "GET", "/ui", None, &wrong, None).await.status,
        401
    );
    let basic = [("Authorization", BASIC_OPERATOR)];
    for url in ["/ui", "/ui/app.js"] {
        assert_eq!(
            raw(s.addr, "GET", url, None, &basic, None).await.status,
            200,
            "{url}"
        );
    }
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
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut disk = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).expect("read ui dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().into_string().expect("utf-8 filename");
        if name.starts_with('.') {
            continue;
        }
        assert!(
            entry.path().is_file(),
            "subdirectories under ui/ are not allowed: `{name}`"
        );
        disk.insert(name);
    }
    let table: BTreeSet<String> = assets::files()
        .iter()
        .map(|(p, _)| (*p).to_owned())
        .collect();
    let missing: Vec<_> = disk.difference(&table).collect();
    let stale: Vec<_> = table.difference(&disk).collect();
    assert!(missing.is_empty(), "missing from asset table: {missing:?}");
    assert!(stale.is_empty(), "stale entries in asset table: {stale:?}");
    assert_eq!(assets::files().len(), table.len(), "duplicate asset paths");
}

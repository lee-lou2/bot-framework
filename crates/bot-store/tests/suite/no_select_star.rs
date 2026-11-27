//! 저장소 질의는 `select *`·`select x.*`·`returning *`를 쓰지 않는다(열 목록은 `src/cols.rs`). 준비된 문은 연결마다 캐시되는데 결과 형식이 열 순서까지 포함하므로,
//! 마이그레이션이 열을 더하거나 열을 지웠다 다시 만들면 PostgreSQL이 `cached plan must not change result type`(0A000)으로 거절한다.
//! 시험이 병렬로 도는 동안 한 연결이 옛 준비문을 다시 쓰면 가끔 터지는 흔들림이 이것이었다(`run_origin`).

use std::{fs, path::Path};

/// 이 줄에 `select *`·`select x.*`·`returning *`·`x.*` 별표 열 선택이 있는가(주석 줄은 설명이니 건너뛴다).
fn selects_by_star(line: &str) -> bool {
    let line = line.trim_start().to_lowercase();
    if line.starts_with("//") || line.starts_with("--") {
        return false;
    }
    if has_keyword_star(&line, "returning") || has_keyword_star(&line, "select") {
        return true;
    }
    has_qualified_star(&line)
}

fn has_keyword_star(line: &str, keyword: &str) -> bool {
    let bytes = line.as_bytes();
    let needle = format!("{keyword} ");
    let mut start = 0;
    while let Some(offset) = line[start..].find(&needle) {
        let at = start + offset;
        let boundary_ok =
            at == 0 || !bytes[at - 1].is_ascii_alphanumeric() && bytes[at - 1] != b'_';
        if boundary_ok {
            let after = line[at + needle.len()..].trim_start();
            if after.starts_with('*') {
                return true;
            }
        }
        start = at + needle.len();
    }
    false
}

/// 문자열 상수(`const RUN_COLUMNS: &str = "r.*,`)까지 포함해 SQL 별칭 별표(`ident.*`)를 잡는다.
fn has_qualified_star(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut pos = 0;
    while let Some(offset) = line[pos..].find(".*") {
        let dot = pos + offset;
        let star_end = dot + 2;
        let after_ok = star_end == bytes.len()
            || matches!(
                bytes[star_end],
                b' ' | b'\t' | b',' | b'"' | b'\'' | b';' | b')'
            );
        if dot > 0 && after_ok && (bytes[dot - 1].is_ascii_alphanumeric() || bytes[dot - 1] == b'_')
        {
            let mut ident_start = dot - 1;
            while ident_start > 0
                && (bytes[ident_start - 1].is_ascii_alphanumeric()
                    || bytes[ident_start - 1] == b'_')
            {
                ident_start -= 1;
            }
            let starts_ident =
                bytes[ident_start].is_ascii_alphabetic() || bytes[ident_start] == b'_';
            let before_ok = ident_start == 0
                || matches!(
                    bytes[ident_start - 1],
                    b' ' | b'\t' | b'"' | b'\'' | b',' | b'(' | b'{'
                );
            if starts_ident && before_ok {
                return true;
            }
        }
        pos = star_end;
    }
    false
}

fn collect_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext == "rs" || ext == "sql")
        {
            out.push(path);
        }
    }
}

#[test]
fn the_matcher_sees_every_star_form_and_nothing_else() {
    for hit in [
        r#"sqlx::query("select * from bot_calls where run = $1")"#,
        r#"    "SELECT * FROM bot_calls""#,
        r#""select r.*, x.agent from bot_runs r""#,
        r#""select c.* from bot_calls c""#,
        r#""insert into t values (1) returning *""#,
        r#""insert into t values (1) returning t.*""#,
        r#""select   *  from t""#,
        r#"const RUN_COLUMNS: &str = "r.*,"#,
        r#"    "  r.*, x.id""#,
    ] {
        assert!(selects_by_star(hit), "{hit}");
    }
    for miss in [
        r#""select count(*) from bot_runs""#,
        r#""select profile, id from bot_runs""#,
        r#""select {} from bot_runs""#,
        "// select * from 은 주석이다",
        "-- select * from 은 SQL 주석이다",
        r#""select 1 from bot_runs r where r.id = $1""#,
        r#""select exists(select 1 from t)""#,
        r#""select a * b from t""#,
        "for i in 0..*n {}",
    ] {
        assert!(!selects_by_star(miss), "{miss}");
    }
}

#[test]
fn no_store_query_selects_every_column_by_star() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_sources(&src, &mut files);
    files.sort();
    // 하위 폴더의 .rs와 .sql 파일도 실제로 검사 대상에 들어가는지 확인한다
    assert!(
        files.iter().any(|p| p.ends_with("cards/closing.rs")),
        "하위 폴더 .rs 파일을 재귀 순회해야 한다"
    );
    assert!(
        files.iter().any(|p| p.ends_with("queue/pick.sql")),
        "하위 폴더 .sql 파일을 재귀 순회해야 한다"
    );
    let mut found = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for (number, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
            if selects_by_star(line) {
                found.push(format!("{rel}:{}", number + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "`select *` 또는 `x.*`를 쓰는 곳이 있다 — 열을 명시한다(`src/cols.rs`): {found:?}"
    );
}

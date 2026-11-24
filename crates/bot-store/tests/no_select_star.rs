//! 저장소 질의는 `select *`·`select x.*`·`returning *`를 쓰지 않는다(열 목록은 `src/cols.rs`). 준비된 문은 연결마다 캐시되는데 결과 형식이 열 순서까지 포함하므로,
//! 마이그레이션이 열을 더하거나 열을 지웠다 다시 만들면 PostgreSQL이 `cached plan must not change result type`(0A000)으로 거절한다.
//! 시험이 병렬로 도는 동안 한 연결이 옛 준비문을 다시 쓰면 가끔 터지는 흔들림이 이것이었다(`run_origin`).

use std::{fs, path::Path};

/// 이 줄에 `select *`·`select x.*`·`returning *`가 있는가(주석 줄은 설명이니 건너뛴다).
fn selects_by_star(line: &str) -> bool {
    let line = line.trim_start().to_lowercase();
    if line.starts_with("//") {
        return false;
    }
    if line.contains("returning *") {
        return true;
    }
    let mut rest = line.as_str();
    while let Some(at) = rest.find("select ") {
        let after = rest[at + "select ".len()..].trim_start();
        if after.starts_with('*') {
            return true;
        }
        let ident: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !ident.is_empty() && after[ident.len()..].starts_with(".*") {
            return true;
        }
        rest = &rest[at + "select ".len()..];
    }
    false
}

#[test]
fn the_matcher_sees_every_star_form_and_nothing_else() {
    for hit in [
        r#"sqlx::query("select * from bot_calls where run = $1")"#,
        r#"    "SELECT * FROM bot_calls""#,
        r#""select r.*, x.agent from bot_runs r""#,
        r#""select c.* from bot_calls c""#,
        r#""insert into t values (1) returning *""#,
        r#""select   *  from t""#,
    ] {
        assert!(selects_by_star(hit), "{hit}");
    }
    for miss in [
        r#""select count(*) from bot_runs""#,
        r#""select profile, id from bot_runs""#,
        r#""select {} from bot_runs""#,
        "// select * from 은 주석이다",
        r#""select 1 from bot_runs r where r.id = $1""#,
        r#""select exists(select 1 from t)""#,
    ] {
        assert!(!selects_by_star(miss), "{miss}");
    }
}

#[test]
fn no_store_query_selects_every_column_by_star() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for entry in fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for (number, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
            if selects_by_star(line) {
                found.push(format!("{name}:{}", number + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "`select *`를 쓰는 곳이 있다 — 열을 명시한다(`src/cols.rs`): {found:?}"
    );
}

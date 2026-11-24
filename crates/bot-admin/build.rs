//! `ui/` 폴더의 파일로 내장 자산 표를 만든다. 표를 손으로 맞추다 파일을 더하거나 지울 때 빌드가 깨지는 일을 막는다.
//! 의도하지 않은 파일이 서빙되지 않게 다음만 싣는다: 폴더 바로 아래의 파일, 허용한 확장자(html·css·js·svg),
//! `[a-z0-9._-]` 이름. 숨김·임시·백업 파일과 그 밖의 파일은 빼고 경고만 남긴다.
//! 첫 항목은 `index.html`(`/ui`)이고 나머지는 이름순이다. 서빙 목록은 `tests/assets.rs`가 고정한다.

use std::{env, fs, path::Path};

/// (확장자, `assets.rs`의 MIME 상수 이름).
const KINDS: [(&str, &str); 4] = [
    ("html", "HTML"),
    ("css", "CSS"),
    ("js", "JS"),
    ("svg", "SVG"),
];

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dir = Path::new(&manifest).join("ui");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=build.rs");
    let mut served: Vec<(String, &str)> = Vec::new();
    for entry in fs::read_dir(&dir)
        .expect("ui 폴더를 읽을 수 없다")
        .filter_map(Result::ok)
    {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || !entry.path().is_file() {
            continue; // 숨김 파일·하위 폴더는 조용히 뺀다
        }
        let kind = name
            .rsplit_once('.')
            .and_then(|(_, ext)| KINDS.iter().find(|(e, _)| *e == ext));
        match kind {
            Some((_, mime)) if name_ok(&name) => served.push((name, mime)),
            _ => println!(
                "cargo:warning=ui/{name}은(는) 서빙하지 않는다(허용 확장자 html·css·js·svg, 이름은 [a-z0-9._-])"
            ),
        }
    }
    served.sort_by_key(|(n, _)| (n != "index.html", n.clone()));
    assert_eq!(
        served.first().map(|(n, _)| n.as_str()),
        Some("index.html"),
        "ui/index.html이 있어야 한다"
    );
    let mut table = String::from("&[\n");
    for (name, mime) in &served {
        let path = dir.join(name);
        table.push_str(&format!(
            "    Asset {{ path: {name:?}, mime: {mime}, bytes: include_bytes!({:?}) }},\n",
            path.to_string_lossy()
        ));
    }
    table.push(']');
    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("assets_table.rs");
    fs::write(out, table).expect("자산 표를 쓸 수 없다");
}

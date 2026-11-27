//! Generates the embedded asset table from `ui/` so adding or removing UI files stays in sync.
//! Only top-level files matching allowed extensions and `[a-z0-9._-]` are included (`index.html` first).

use std::{env, fs, path::Path};

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
        .expect("read ui directory")
        .filter_map(Result::ok)
    {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || !entry.path().is_file() {
            continue;
        }
        let kind = name
            .rsplit_once('.')
            .and_then(|(_, ext)| KINDS.iter().find(|(e, _)| *e == ext));
        match kind {
            Some((_, mime)) if name_ok(&name) => served.push((name, mime)),
            _ => println!(
                "cargo:warning=skipping ui/{name} (allowed extensions: html, css, js, svg; name: [a-z0-9._-])"
            ),
        }
    }
    served.sort_by_key(|(n, _)| (n != "index.html", n.clone()));
    assert_eq!(
        served.first().map(|(n, _)| n.as_str()),
        Some("index.html"),
        "ui/index.html must exist"
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
    fs::write(out, table).expect("write asset table");
}

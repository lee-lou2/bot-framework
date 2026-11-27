//! `bot init` and `bot add` scaffolding for starter workspaces and profile skeletons.
mod add;
mod files;
mod templates;
mod tool;

pub use add::{agent, skill};
pub use tool::tool;

use crate::cli::Template;
use bot_core::id::is_name;
use bot_profile::{Limits, Registry};
use files::Writer;
use std::path::Path;

fn label(template: Template) -> &'static str {
    match template {
        Template::Personal => "personal",
        Template::Team => "team",
        Template::Company => "company",
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

pub fn init(name: &str, template: Template, dir: &Path, uid: u32) -> Result<String, String> {
    if !is_name(name) {
        return Err(format!(
            "name {name:?} must start with a lowercase letter and use only lowercase letters, digits, and hyphens (max 63 chars) — e.g. my-bot"
        ));
    }
    let parent = Writer::new(dir)?;
    parent.require_absent(&[name])?;
    let planned = plan(name, templates::files(template), uid);
    let files = build(&parent, &std::env::temp_dir(), name, &planned)?;
    let base = std::path::absolute(dir.join(name)).unwrap_or_else(|_| dir.join(name));
    let mut lines = vec![format!(
        "Created: {} (template {}, {} agent(s))",
        base.display(),
        label(template),
        templates::agent_count(template)
    )];
    lines.extend(files.iter().map(|path| format!("  {path}")));
    lines.extend([
        "Next steps:".to_owned(),
        format!("  export BOT_PROFILES={}", quote(&base.join("profiles"))),
        format!("  export BOT_HOST_CONFIG={}", quote(&base.join("host.json"))),
        "  bot validate   # check profile and host config (no DB or Slack connection)".to_owned(),
        "  bot doctor     # check DB, secret, and model config (offline by default)".to_owned(),
        "host.json is a sample using the demo model and the current OS user (CLI account `owner`). Add real providers, gateways, and tool servers directly."
            .to_owned(),
    ]);
    Ok(lines.join("\n"))
}

fn fill(text: &str, name: &str, uid: u32) -> String {
    text.replace("__ID__", name)
        .replace("__UID__", &uid.to_string())
}

// Both verification and atomic publication write these exact bytes so verified == published.
struct Planned {
    host: String,
    files: Vec<(String, String)>,
}

fn plan(name: &str, files: &[(&str, &str)], uid: u32) -> Planned {
    let host = fill(templates::HOST_JSON, name, uid);
    let mut planned = vec![("host.json".to_owned(), host.clone())];
    planned.extend(
        files
            .iter()
            .map(|(path, text)| (format!("profiles/{name}/{path}"), fill(text, name, uid))),
    );
    Planned {
        host,
        files: planned,
    }
}

fn write_all(writer: &mut Writer, planned: &Planned) -> Result<(), String> {
    for (path, text) in &planned.files {
        writer.write(path, text)?;
    }
    Ok(())
}

// Validate against a private staged copy under `$TMPDIR` rather than reopening user paths.
fn verify_copy(temp: &Path, planned: &Planned) -> Result<(), String> {
    let base = Writer::new(temp)
        .map_err(|error| format!("cannot use temp directory ($TMPDIR) for validation: {error}"))?;
    let mut copy = base.stage()?;
    let checked = write_all(&mut copy.writer, planned)
        .and_then(|()| verify(&temp.join(copy.name()).join("profiles"), &planned.host));
    copy.discard();
    checked
}

fn build(
    parent: &Writer,
    temp: &Path,
    name: &str,
    planned: &Planned,
) -> Result<Vec<String>, String> {
    verify_copy(temp, planned)?;
    let mut stage = parent.stage()?;
    if let Err(error) = write_all(&mut stage.writer, planned) {
        stage.discard();
        return Err(error);
    }
    let created = stage.writer.created();
    stage.publish(name)?;
    Ok(created)
}

fn verify(profiles: &Path, host_text: &str) -> Result<(), String> {
    let host = crate::host::parse(host_text)
        .map_err(|error| format!("sample host config is invalid; nothing was created: {error}"))?;
    Registry::new()
        .reload(profiles, &host.catalog, &Limits::default())
        .map(|_| ())
        .map_err(|report| format!("profile failed validation; nothing was created:\n{report}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn count(path: &Path) -> usize {
        fs::read_dir(path).unwrap().count()
    }

    #[test]
    fn names_that_yaml_reads_as_non_text_still_make_a_valid_profile() {
        for name in ["null", "true", "false", "yes", "no", "on", "off", "y", "n"] {
            for template in [Template::Personal, Template::Team, Template::Company] {
                let dir = tempfile::tempdir().unwrap();
                init(name, template, dir.path(), 1000)
                    .unwrap_or_else(|error| panic!("{name} {template:?}: {error}"));
            }
        }
        let (dir, temp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut planned = plan("null", templates::files(Template::Personal), 1000);
        for (_, text) in &mut planned.files {
            *text = text.replace("id: \"null\"", "id: null");
        }
        let parent = Writer::new(dir.path()).unwrap();
        let error = build(&parent, temp.path(), "null", &planned).unwrap_err();
        assert!(error.contains("failed validation"), "{error}");
        assert_eq!(count(dir.path()), 0, "user dir untouched: {error}");
        assert_eq!(count(temp.path()), 0, "temp copy removed: {error}");
    }

    #[test]
    fn the_verified_bytes_are_the_published_bytes_and_the_private_copy_is_removed() {
        let (dir, temp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let planned = plan("demo", templates::files(Template::Team), 1000);
        let parent = Writer::new(dir.path()).unwrap();
        let created = build(&parent, temp.path(), "demo", &planned).unwrap();
        assert_eq!(count(temp.path()), 0, "temp copy removed");
        assert_eq!(count(dir.path()), 1, "single published directory");
        assert_eq!(created.len(), planned.files.len());
        for (path, text) in &planned.files {
            let published = fs::read_to_string(dir.path().join("demo").join(path)).unwrap();
            assert_eq!(&published, text, "{path}");
        }
        let gone = temp.path().join("not-here");
        let error = build(&parent, &gone, "other", &planned).unwrap_err();
        assert!(error.contains("$TMPDIR"), "{error}");
        assert_eq!(count(dir.path()), 1);
    }
}

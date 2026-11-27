//! Remote MCP tool definition sync (`bot mcp sync`) into deterministic `tools.lock.yaml`.
use crate::{
    load::{load_host, secret_store},
    serve::remote::ReqwestWire,
    settings::Settings,
};
use bot_broker::{
    egress::EgressPolicy,
    guard::{blocks_output, contains_credential},
    mcp::{McpError, header, http::Http},
    proxy::SystemResolver,
    resolve::HostPort,
};
use bot_core::{
    mcp::{entry_description, entry_sha256},
    port::{Error, SecretStore},
};
use bot_gateway::cli::socket::current_uid;
use bot_profile::{
    Limits, Schema, SchemaProblem,
    issue::Issue,
    locate::locate_skill,
    lock::{self, Entry},
};
use serde_json::Value;
use std::{
    collections::BTreeMap, io::Write, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc,
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);

/// Emits only fixed codes and locations so untrusted server text is never echoed in diagnostics.
fn fixed(issue: &Issue) -> String {
    if issue.at.is_empty() {
        format!("  [{}] {}", issue.code, issue.path)
    } else {
        format!("  [{}] {} {}", issue.code, issue.path, issue.at)
    }
}

fn fixed_lines(issues: &[Issue]) -> String {
    issues
        .iter()
        .take(12)
        .map(fixed)
        .collect::<Vec<_>>()
        .join("\n")
}

fn schema_lines(found: &[SchemaProblem]) -> String {
    let shown: Vec<String> = found
        .iter()
        .take(8)
        .map(|p| {
            format!(
                "[{}] {}",
                p.code,
                if p.pointer.is_empty() {
                    "/"
                } else {
                    &p.pointer
                }
            )
        })
        .collect();
    let more = found.len().saturating_sub(shown.len());
    format!(
        "{}{}",
        shown.join(", "),
        if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        }
    )
}

fn exposes(value: &Value, credentials: &[Vec<u8>], shapes: bool) -> bool {
    let text = |text: &str| {
        if shapes {
            blocks_output(text, credentials)
        } else {
            contains_credential(text, credentials)
        }
    };
    match value {
        Value::String(item) => text(item),
        Value::Number(number) => text(&number.to_string()),
        Value::Array(items) => items.iter().any(|item| exposes(item, credentials, shapes)),
        Value::Object(map) => map
            .iter()
            .any(|(key, item)| text(key) || exposes(item, credentials, shapes)),
        _ => false,
    }
}

pub(crate) fn entries_of(
    declared: &[(String, String)],
    listed: &[Value],
    credentials: &[Vec<u8>],
) -> Result<BTreeMap<String, Entry>, Vec<String>> {
    // Check credentials across the entire response (including undeclared tools) before building any diagnostic.
    if listed.iter().any(|tool| exposes(tool, credentials, false)) {
        return Err(vec![
            "server response contains a credential-like value — nothing was written; inspect the server"
                .to_owned(),
        ]);
    }
    let (mut out, mut problems) = (BTreeMap::new(), Vec::new());
    for (id, name) in declared {
        let found: Vec<&Value> = listed
            .iter()
            .filter(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))
            .collect();
        let tool = match found.as_slice() {
            [] => {
                problems.push(format!("`{id}`: server does not advertise tool `{name}`"));
                continue;
            }
            [one] => *one,
            _ => {
                problems.push(format!(
                    "`{id}`: server advertises multiple tools named `{name}`"
                ));
                continue;
            }
        };
        let (Some(description), Some(schema), Some(sha256)) = (
            entry_description(tool),
            tool.get("inputSchema").filter(|s| s.is_object()),
            entry_sha256(tool),
        ) else {
            problems.push(format!(
                "`{id}`: invalid definition shape (description must be a string and inputSchema an object)"
            ));
            continue;
        };
        match Schema::compile_input_problems(schema.clone()) {
            Ok(compiled) if compiled.doc().get("type") != Some(&Value::from("object")) => {
                problems.push(format!(
                    "`{id}`: input schema root must have `type: object`"
                ));
                continue;
            }
            Ok(_) => {}
            Err(found) => {
                // Report only fixed codes and JSON pointers so server-chosen text is never echoed.
                problems.push(format!(
                    "`{id}`: input schema uses unsupported profile vocabulary: {}",
                    schema_lines(&found)
                ));
                continue;
            }
        }
        if let Err(why) = header::check_schema(schema) {
            problems.push(format!("`{id}`: {why}"));
            continue;
        }
        if blocks_output(name, credentials)
            || blocks_output(&description, credentials)
            || exposes(schema, credentials, true)
        {
            problems.push(format!(
                "`{id}`: definition (description or schema) contains a credential-like value — not written; inspect the server"
            ));
            continue;
        }
        out.insert(
            id.clone(),
            Entry {
                name: name.clone(),
                description,
                input_schema: schema.clone(),
                sha256,
            },
        );
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    write_atomic_with(path, text, || bot_core::random::random_hex(8))
}

fn write_atomic_with(
    path: &Path,
    text: &str,
    suffix: impl FnOnce() -> String,
) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && !meta.is_file()
    {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let tmp = path.with_file_name(format!(".tools.lock.yaml.tmp-{}", suffix()));
    // If `create_new` fails (e.g. `AlreadyExists`), do not unlink it because it may belong to someone else.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    let finish = (|| -> std::io::Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    finish.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("failed to write {}: {e}", path.display())
    })
}

pub(crate) fn describe(error: &McpError) -> String {
    match error {
        McpError::Unreached => {
            "could not reach server (check URL, TLS certificate, DNS resolution, and egress)".into()
        }
        McpError::Status {
            status: 401 | 403, ..
        } => {
            format!(
                "server rejected authentication (HTTP {}) — check credentials",
                match error {
                    McpError::Status { status, .. } => *status,
                    _ => 0,
                }
            )
        }
        McpError::Status { status, .. } => format!("server responded with HTTP {status}"),
        McpError::Version => {
            "server does not support MCP protocol version 2026-07-28, 2025-11-25, 2025-06-18, or 2025-03-26"
                .into()
        }
        McpError::Timeout => "server timed out".into(),
        McpError::Exposed => {
            "server response contains a credential (in tool definitions, cursors, or notifications) — nothing was written; inspect the server".into()
        }
        McpError::Changed => {
            "server reported that its tool list changed during pagination — retry".into()
        }
        other => format!("could not parse server response ({other:?})"),
    }
}

pub async fn run(settings: &Settings, dir: &Path) -> Result<(), String> {
    run_with(settings, dir, || secret_store(settings)).await
}

pub async fn run_with(
    settings: &Settings,
    dir: &Path,
    make_store: impl FnOnce() -> Result<Arc<dyn SecretStore>, String>,
) -> Result<(), String> {
    let root = settings
        .profiles
        .as_ref()
        .ok_or("BOT_PROFILES is required (profiles root directory)")?;
    // Validate profile and SKILL.md before touching secrets or opening any remote connection.
    let located = locate_skill(root, dir, &Limits::default()).map_err(|issues| {
        format!(
            "cannot resolve profile, refusing to connect or read secrets:\n{}",
            fixed_lines(&issues)
        )
    })?;
    let profile = located.profile.0.clone();
    let text = String::from_utf8(located.tools_yaml.clone())
        .map_err(|_| "tools.yaml is not valid UTF-8".to_owned())?;
    let declared = lock::read_declared("tools.yaml", &text).map_err(|issues| {
        issues
            .iter()
            .map(|i| format!("tools.yaml {} {}", i.at, i.message))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if declared.tools.is_empty() {
        return Err("no tools declared in tools.yaml".into());
    }
    let uid = current_uid().map_err(|e| e.to_string())?;
    let host = load_host(settings, uid)?;
    let Some(remote) = host.remote_servers.get(&declared.server) else {
        return Err(if host.servers.contains_key(&declared.server) {
            format!(
                "server `{}` is a local (stdio) server — `bot mcp sync` currently supports remote (`transport: \"http\"`) servers only",
                declared.server
            )
        } else {
            format!(
                "server is not registered in host config: {}",
                declared.server
            )
        });
    };
    let cap: Option<Vec<HostPort>> = host
        .catalog
        .egress
        .get(&profile)
        .map(|set| set.iter().filter_map(|t| HostPort::parse(t)).collect());
    let blocked: Vec<String> = declared
        .tools
        .iter()
        .filter(|tool| {
            let wanted: Vec<HostPort> = tool
                .egress
                .iter()
                .filter_map(|t| HostPort::parse(t))
                .collect();
            !EgressPolicy::new(&wanted, cap.as_deref()).permits(&remote.host, remote.port)
        })
        .map(|tool| tool.id.clone())
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "server target `{}` is not permitted by `egress` for {} (and host cap `egress.{}`) — list the exact `host:port`",
            remote.target_text(),
            blocked.join(", "),
            profile
        ));
    }
    let (mut credentials, mut bearer) = (Vec::new(), None);
    if let Some(name) = &remote.bearer {
        let store = make_store()?;
        let handle = store
            .for_profile(located.profile.clone())
            .await
            .map_err(|_| format!("could not open secret store for profile {profile}"))?;
        let value = handle.resolve(name.clone()).await.map_err(|e| match e {
            Error::Missing => {
                format!(
                    "secret `{}` is missing from profile `{profile}` store",
                    name.as_str()
                )
            }
            _ => format!("could not read secret `{}` (store error)", name.as_str()),
        })?;
        let text = String::from_utf8(value.0.clone())
            .ok()
            .filter(|t| !t.is_empty() && !t.contains(['\0', '\r', '\n']))
            .ok_or_else(|| {
                format!(
                    "secret `{}` has a value that cannot be used as a bearer token",
                    name.as_str()
                )
            })?;
        credentials.push(value.0);
        bearer = Some(text);
    }
    let wire = Arc::new(ReqwestWire::new(Arc::new(SystemResolver))?);
    let mut http = Http::new(wire, remote.clone(), bearer, false);
    let listed = async {
        http.connect(LIMIT).await?;
        http.tools(LIMIT).await
    }
    .await;
    http.close().await;
    let listed =
        listed.map_err(|e| format!("failed to fetch tool definitions: {}", describe(&e)))?;
    let wanted: Vec<(String, String)> = declared
        .tools
        .iter()
        .map(|t| (t.id.clone(), t.name.clone()))
        .collect();
    let entries = entries_of(&wanted, &listed, &credentials).map_err(|problems| {
        format!("refused to write tools.lock.yaml:\n{}", problems.join("\n"))
    })?;
    let rendered = lock::render(&declared.server, &entries)
        .ok_or("could not canonicalize tool definitions (number out of safe range)")?;
    // Verify the rendered lock with the same validator `bot validate` uses before writing to disk.
    located
        .verify_lock(rendered.as_bytes(), &Limits::default())
        .map_err(|issues| {
            format!(
                "generated tools.lock.yaml failed validation and was not written:\n{}",
                fixed_lines(&issues)
            )
        })?;
    let path = located.profile_dir.join(&located.skill).join(lock::FILE);
    let same = std::fs::read_to_string(&path).is_ok_and(|old| old == rendered);
    if !same {
        write_atomic(&path, &rendered)?;
    }
    println!(
        "{}: {} {} tool definitions",
        path.display(),
        if same {
            "verified (unchanged)"
        } else {
            "wrote"
        },
        entries.len(),
    );
    for (id, entry) in &entries {
        println!(
            "  {id}  {}",
            entry.sha256.get(..12).unwrap_or(&entry.sha256)
        );
    }
    eprintln!(
        "Note: locked tool descriptions are included in model prompts. Review `description` fields in tools.lock.yaml before deploying."
    );
    Ok(())
}

#[cfg(test)]
mod tests;

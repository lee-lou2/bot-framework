//! Tool server package specs, content-addressed revision measurement, and read-only installations.
//!
//! `measure` hashes packages in place for decision-only roles and `bot validate`; `install` hashes
//! the bytes written into `<base>/<revision>/`, executes only from that read-only copy, and holds
//! shared locks to prevent concurrent recovery from deleting active installations.

use bot_broker::mcp::remote::Remote;
use bot_core::{
    id::{Revision, SecretName},
    install::{InstallEntry, ServerState, server_revision},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

mod place;
mod walk;

#[cfg(test)]
use place::remove_tree;
use place::{base_dir, install_one};
use walk::walk;

const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    pub root: PathBuf,
    /// `command[0]` is a relative path inside `root`; remaining items are passed without shell expansion.
    pub command: Vec<String>,
    /// `required` folds into the server revision so stateful servers bind to approvals and activation.
    pub state: ServerState,
}

fn relative_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Servers {
    pub local: BTreeMap<String, ServerSpec>,
    pub remote: BTreeMap<String, Remote>,
}

pub fn parse_all(value: &Value) -> Result<Servers, String> {
    let Value::Object(map) = value else {
        return parse_servers(value).map(|local| Servers {
            local,
            ..Servers::default()
        });
    };
    let (mut local, mut remote) = (serde_json::Map::new(), BTreeMap::new());
    for (alias, entry) in map {
        if entry.get("transport").and_then(Value::as_str) == Some("http") {
            if !bot_core::id::is_name(alias) {
                return Err(format!(
                    "servers.{alias}: alias must follow naming rules (lowercase letters, digits, hyphens)"
                ));
            }
            remote.insert(alias.clone(), parse_remote(alias, entry)?);
        } else {
            local.insert(alias.clone(), entry.clone());
        }
    }
    Ok(Servers {
        local: parse_servers(&Value::Object(local))?,
        remote,
    })
}

fn parse_remote(alias: &str, entry: &Value) -> Result<Remote, String> {
    let fields = entry
        .as_object()
        .ok_or_else(|| format!("servers.{alias}: must be an object"))?;
    if let Some(unknown) = fields
        .keys()
        .find(|k| !matches!(k.as_str(), "transport" | "url" | "auth"))
    {
        return Err(format!("servers.{alias}: unknown key: {unknown}"));
    }
    let url = fields
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("servers.{alias}.url: URL string is required"))?;
    match fields.get("auth") {
        None => Remote::parse(url, None).map_err(|why| format!("servers.{alias}.url: {why}")),
        Some(Value::Object(auth))
            if auth.get("kind").and_then(Value::as_str) == Some("bearer")
                && auth.keys().all(|k| matches!(k.as_str(), "kind" | "secret")) =>
        {
            let name = auth
                .get("secret")
                .and_then(Value::as_str)
                .and_then(SecretName::parse)
                .filter(|name| !bot_broker::private::reserved_secret(name))
                .ok_or_else(|| {
                    format!(
                        "servers.{alias}.auth.secret: valid secret name (lowercase, digits, hyphens) required"
                    )
                })?;
            Remote::parse(url, Some(name)).map_err(|why| format!("servers.{alias}.url: {why}"))
        }
        Some(Value::Object(auth))
            if auth.get("kind").and_then(Value::as_str) == Some("oauth")
                && auth
                    .keys()
                    .all(|k| matches!(k.as_str(), "kind" | "credential" | "issuer")) =>
        {
            let name = auth
                .get("credential")
                .and_then(Value::as_str)
                .and_then(SecretName::parse)
                .filter(|name| !bot_broker::private::reserved_secret(name))
                .ok_or_else(|| {
                    format!(
                        "servers.{alias}.auth.credential: valid credential name (lowercase, digits, hyphens) required"
                    )
                })?;
            let issuer = match auth.get("issuer") {
                None => None,
                Some(Value::String(raw)) if !raw.is_empty() => Some(raw.as_str()),
                Some(_) => {
                    return Err(format!(
                        "servers.{alias}.auth.issuer: non-empty https URL string required"
                    ));
                }
            };
            Remote::parse_oauth_pinned(url, name, issuer)
                .map_err(|why| format!("servers.{alias}: {why}"))
        }
        Some(_) => Err(format!(
            "servers.{alias}.auth: expected {{\"kind\": \"bearer\", \"secret\": \"<name>\"}} or {{\"kind\": \"oauth\", \"credential\": \"<name>\", \"issuer\"?: \"https://...\"}}"
        )),
    }
}

pub fn parse_servers(value: &Value) -> Result<BTreeMap<String, ServerSpec>, String> {
    let Value::Object(map) = value else {
        return Err(
            "servers: must be an object keyed by alias — e.g. {\"agenda\": {\"root\": \"/opt/bot/servers/agenda\", \"command\": [\"bin/server\", \"--stdio\"]}}"
                .to_owned(),
        );
    };
    let mut out = BTreeMap::new();
    for (alias, entry) in map {
        if !bot_core::id::is_name(alias) {
            return Err(format!(
                "servers.{alias}: alias must follow naming rules (lowercase letters, digits, hyphens)"
            ));
        }
        let Value::Object(fields) = entry else {
            return Err(format!(
                "servers.{alias}: must be an object with `root` and `command`"
            ));
        };
        // `transport: "http"` is split out by `parse_all`; only `stdio` (default) is valid here.
        if fields.get("transport").is_some_and(|t| t != "stdio") {
            return Err(format!(
                "servers.{alias}.transport: must be \"stdio\" (default) or \"http\""
            ));
        }
        if let Some(unknown) = fields
            .keys()
            .find(|k| !matches!(k.as_str(), "root" | "command" | "state" | "transport"))
        {
            let hint = if unknown == "revision" {
                " (revisions are computed automatically from installed contents)"
            } else {
                ""
            };
            return Err(format!("servers.{alias}: unknown key: {unknown}{hint}"));
        }
        let root = fields
            .get("root")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("servers.{alias}.root: absolute path string required"))?;
        if root.contains('\0') || !Path::new(root).is_absolute() || root == "/" {
            return Err(format!(
                "servers.{alias}.root: must be an absolute path other than `/`"
            ));
        }
        let command = fields
            .get("command")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| {
                format!("servers.{alias}.command: non-empty array of strings required")
            })?;
        if command.len() > MAX_ARGS {
            return Err(format!(
                "servers.{alias}.command: too many arguments (max {MAX_ARGS})"
            ));
        }
        let command: Vec<String> = command
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|text| !text.contains('\0') && text.len() <= MAX_ARG_BYTES)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        format!(
                            "servers.{alias}.command: must be strings without NUL, at most {MAX_ARG_BYTES} bytes"
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        if !relative_ok(&command[0]) {
            return Err(format!(
                "servers.{alias}.command[0]: must be a relative path under root (no leading `/` or `..`)"
            ));
        }
        let state = match fields.get("state") {
            None => ServerState::default(),
            Some(Value::String(text)) if text == "ephemeral" => ServerState::Ephemeral,
            Some(Value::String(text)) if text == "required" => ServerState::Required,
            Some(_) => {
                return Err(format!(
                    "servers.{alias}.state: must be \"ephemeral\" (default) or \"required\""
                ));
            }
        };
        out.insert(
            alias.clone(),
            ServerSpec {
                root: PathBuf::from(root),
                command,
                state,
            },
        );
    }
    Ok(out)
}

fn revision_of(
    alias: &str,
    spec: &ServerSpec,
    entries: &BTreeMap<String, InstallEntry>,
) -> Result<Revision, String> {
    server_revision(&spec.command, entries, spec.state).map_err(|_| {
        format!(
            "servers.{alias}: command[0] ({}) must be an executable regular file under root",
            spec.command[0]
        )
    })
}

pub fn measure_all(
    local: &BTreeMap<String, ServerSpec>,
    remote: &BTreeMap<String, Remote>,
) -> Result<BTreeMap<String, Revision>, String> {
    let mut out = measure(local)?;
    out.extend(
        remote
            .iter()
            .map(|(alias, remote)| (alias.clone(), Revision(remote.revision()))),
    );
    Ok(out)
}

pub fn measure(specs: &BTreeMap<String, ServerSpec>) -> Result<BTreeMap<String, Revision>, String> {
    let mut out = BTreeMap::new();
    for (alias, spec) in specs {
        let (entries, _) =
            walk(&spec.root, None, None).map_err(|e| format!("servers.{alias}: {e}"))?;
        out.insert(alias.clone(), revision_of(alias, spec, &entries)?);
    }
    Ok(out)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installed {
    pub revision: Revision,
    pub state: ServerState,
    pub dir: PathBuf,
    pub argv: Vec<String>,
}

/// Holds shared locks on installed revision trees for the lifetime of this value so concurrent recovery cannot delete them.
#[derive(Clone, Debug, Default)]
pub struct Installs {
    items: BTreeMap<String, Installed>,
    _locks: Vec<Arc<File>>,
}

impl Installs {
    pub fn get(&self, alias: &str) -> Option<&Installed> {
        self.items.get(alias)
    }

    #[cfg(test)]
    pub(crate) fn aliases(&self) -> impl Iterator<Item = &String> {
        self.items.keys()
    }

    pub fn revisions(&self) -> BTreeMap<String, Revision> {
        self.items
            .iter()
            .map(|(alias, item)| (alias.clone(), item.revision.clone()))
            .collect()
    }
}

pub fn install(
    specs: &BTreeMap<String, ServerSpec>,
    base: &Path,
    uid: u32,
) -> Result<Installs, String> {
    base_dir(base, uid)?;
    let mut items = BTreeMap::new();
    let mut locks = Vec::new();
    for (alias, spec) in specs {
        let (installed, lock) = install_one(alias, spec, base, uid)?;
        items.insert(alias.clone(), installed);
        locks.push(Arc::new(lock));
    }
    Ok(Installs {
        items,
        _locks: locks,
    })
}

pub fn default_base(configured: Option<&Path>, home: Option<&str>) -> Result<PathBuf, String> {
    match (configured, home) {
        (Some(path), _) if path.is_absolute() => Ok(path.to_owned()),
        (Some(_), _) => Err("BOT_INSTALLS must be an absolute path".to_owned()),
        (None, Some(home)) if Path::new(home).is_absolute() => {
            Ok(Path::new(home).join(".local/state/bot/installs"))
        }
        _ => Err(
            "cannot determine install directory — set BOT_INSTALLS to an absolute path".to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests;

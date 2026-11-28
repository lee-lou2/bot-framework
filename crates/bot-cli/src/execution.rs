//! Host execution sections (`providers`, `sandbox`) and authority fingerprint calculation.
use bot_core::id::Revision;
use bot_profile::HostCatalog;
use bot_runtime::model::CliKind;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

mod provider;
mod provider_view;
pub use provider::{
    ApiProvider, CliProvider, Provider, fallback_chain, parse_providers, reachable_aliases,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Srt {
    pub binary: PathBuf,
    pub work_root: PathBuf,
    /// Optional extra host directories (up to 16) appended to the sandboxed tool `PATH`.
    pub extra_path: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Apple {
    /// Legacy single digest-pinned VM image (`<name>@sha256:<64 hex>`, used for `claude-cli`).
    pub image: Option<String>,
    /// Per-CLI digest-pinned VM images keyed by `"claude"`, `"codex"`, or `"agy"`.
    pub images: BTreeMap<String, String>,
    pub network: String,
    /// Host-only IPv4 relay bind address; omits to use the anchor-detected gateway IP on an ephemeral port.
    pub relay_bind: Option<SocketAddr>,
    pub work_root: Option<PathBuf>,
    pub instance: Option<String>,
    pub binary: Option<PathBuf>,
}

impl Apple {
    /// Resolves the digest-pinned VM image for `kind`; `codex` and `agy` require explicit `images.<cli>` entries.
    pub fn image_for(&self, kind: CliKind) -> Option<&str> {
        match kind {
            CliKind::Claude => self
                .images
                .get("claude")
                .map(String::as_str)
                .or(self.image.as_deref()),
            CliKind::Codex => self.images.get("codex").map(String::as_str),
            CliKind::Agy => self.images.get("agy").map(String::as_str),
        }
    }

    /// Selects the digest-pinned image used to boot the shared anchor VM (`bot-anchor`).
    pub fn anchor_image(&self) -> Option<&str> {
        self.image
            .as_deref()
            .or_else(|| self.images.get("claude").map(String::as_str))
            .or_else(|| self.images.values().next().map(String::as_str))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sandbox {
    pub srt: Option<Srt>,
    pub apple: Option<Apple>,
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path}: must be an object"))
}

fn only(map: &Map<String, Value>, allowed: &[&str], path: &str) -> Result<(), String> {
    match map.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(unknown) => Err(format!("{path}: unknown key: {unknown}")),
        None => Ok(()),
    }
}

fn text<'a>(map: &'a Map<String, Value>, key: &str, path: &str) -> Result<&'a str, String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && !value.contains('\0'))
        .ok_or_else(|| format!("{path}.{key}: must be a non-empty string"))
}

fn absolute(map: &Map<String, Value>, key: &str, path: &str) -> Result<PathBuf, String> {
    let value = text(map, key, path)?;
    let path_buf = PathBuf::from(value);
    if path_buf.is_absolute() && value != "/" {
        Ok(path_buf)
    } else {
        Err(format!(
            "{path}.{key}: must be an absolute path other than `/`"
        ))
    }
}

fn image_pinned(image: &str) -> bool {
    match image.rsplit_once("@sha256:") {
        Some((name, digest)) => {
            !name.is_empty()
                && !name.contains(char::is_whitespace)
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        None => false,
    }
}

pub fn parse_sandbox(value: &Value) -> Result<Sandbox, String> {
    let top = object(value, "sandbox")?;
    only(top, &["srt", "apple"], "sandbox")?;
    let srt = match top.get("srt") {
        Some(entry) => {
            let fields = object(entry, "sandbox.srt")?;
            only(
                fields,
                &["binary", "work_root", "extra_path"],
                "sandbox.srt",
            )?;
            let extra_path = match fields.get("extra_path") {
                None => Vec::new(),
                Some(raw) => {
                    let items = raw
                        .as_array()
                        .ok_or_else(|| "sandbox.srt.extra_path: must be an array".to_owned())?;
                    if items.len() > 16 {
                        return Err(
                            "sandbox.srt.extra_path: must have at most 16 entries".to_owned()
                        );
                    }
                    let mut dirs = Vec::with_capacity(items.len());
                    for item in items {
                        let text = item
                            .as_str()
                            .filter(|s| !s.is_empty() && !s.contains('\0') && !s.contains(':'))
                            .ok_or_else(|| {
                                "sandbox.srt.extra_path: must be a non-empty string without `:` or NUL"
                                    .to_owned()
                            })?;
                        let path_buf = PathBuf::from(text);
                        if !path_buf.is_absolute() || text == "/" {
                            return Err(
                                "sandbox.srt.extra_path: must be an absolute path other than `/`"
                                    .to_owned(),
                            );
                        }
                        if !dirs.contains(&path_buf) {
                            dirs.push(path_buf);
                        }
                    }
                    dirs
                }
            };
            Some(Srt {
                binary: absolute(fields, "binary", "sandbox.srt")?,
                work_root: absolute(fields, "work_root", "sandbox.srt")?,
                extra_path,
            })
        }
        None => None,
    };
    let apple = match top.get("apple") {
        Some(entry) => {
            let fields = object(entry, "sandbox.apple")?;
            only(
                fields,
                &[
                    "image",
                    "images",
                    "network",
                    "relay_bind",
                    "work_root",
                    "instance",
                    "binary",
                ],
                "sandbox.apple",
            )?;
            let image = match fields.get("image") {
                None => None,
                Some(_) => {
                    let value = text(fields, "image", "sandbox.apple")?;
                    if !image_pinned(value) {
                        return Err(
                            "sandbox.apple.image: must be pinned to a digest (`name@sha256:<64 hex chars>`)"
                                .to_owned(),
                        );
                    }
                    Some(value.to_owned())
                }
            };
            let mut images = BTreeMap::new();
            if let Some(raw_images) = fields.get("images") {
                let map = object(raw_images, "sandbox.apple.images")?;
                only(map, &["claude", "codex", "agy"], "sandbox.apple.images")?;
                for key in map.keys() {
                    let value = text(map, key, "sandbox.apple.images")?;
                    if !image_pinned(value) {
                        return Err(format!(
                            "sandbox.apple.images.{key}: must be pinned to a digest (`name@sha256:<64 hex chars>`)"
                        ));
                    }
                    images.insert(key.clone(), value.to_owned());
                }
            }
            if image.is_none() && images.is_empty() {
                return Err(
                    "sandbox.apple: must set `image` or a non-empty `images` map (`claude`, `codex`, `agy`)"
                        .to_owned(),
                );
            }
            let relay_bind = match fields.get("relay_bind") {
                None => None,
                Some(_) => {
                    let bind: SocketAddr = text(fields, "relay_bind", "sandbox.apple")?
                        .parse()
                        .map_err(|_| {
                            "sandbox.apple.relay_bind: must be in `host:port` format".to_owned()
                        })?;
                    if bind.ip().is_unspecified() || bind.ip().is_loopback() || !bind.is_ipv4() {
                        return Err(
                            "sandbox.apple.relay_bind: wildcard (0.0.0.0, ::), loopback, and IPv6 addresses are not allowed — specify a host-only IPv4 address or omit to use the anchor-detected address"
                                .to_owned(),
                        );
                    }
                    Some(bind)
                }
            };
            let work_root = match fields.get("work_root") {
                None => None,
                Some(_) => Some(absolute(fields, "work_root", "sandbox.apple")?),
            };
            let binary = match fields.get("binary") {
                None => None,
                Some(_) => Some(absolute(fields, "binary", "sandbox.apple")?),
            };
            let instance = match fields.get("instance") {
                None => None,
                Some(_) => {
                    let name = text(fields, "instance", "sandbox.apple")?;
                    if !bot_gateway::identity::is_alias(name) {
                        return Err(
                            "sandbox.apple.instance: must be lowercase kebab-case (at most 63 chars)"
                                .to_owned(),
                        );
                    }
                    Some(name.to_owned())
                }
            };
            Some(Apple {
                image,
                images,
                network: text(fields, "network", "sandbox.apple")?.to_owned(),
                relay_bind,
                work_root,
                instance,
                binary,
            })
        }
        None => None,
    };
    Ok(Sandbox { srt, apple })
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn count(hash: &mut Sha256, n: usize) {
    hash.update((n as u64).to_be_bytes());
}

/// Authority fingerprint over security-relevant host settings (excludes secret values and host-local paths/addresses).
/// Byte layout is pinned by `fingerprint_tests.rs`; changing it forces re-activation across deployments.
pub fn authority_fingerprint(
    gateway: &[u8; 32],
    catalog: &HostCatalog,
    providers: &BTreeMap<String, Provider>,
    sandbox: &Sandbox,
    revisions: &BTreeMap<String, Revision>,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-v1\0");
    hash.update(gateway);
    for (tag, map) in [("models", &catalog.models), ("egress", &catalog.egress)] {
        hash.update(tag.as_bytes());
        count(&mut hash, map.len());
        for (profile, items) in map {
            field(&mut hash, profile.as_bytes());
            count(&mut hash, items.len());
            for item in items {
                field(&mut hash, item.as_bytes());
            }
        }
    }
    hash.update(b"providers");
    count(&mut hash, providers.len());
    for (alias, provider) in providers {
        field(&mut hash, alias.as_bytes());
        provider.authority(&mut hash);
    }
    hash.update(b"servers");
    count(&mut hash, catalog.servers.len());
    for alias in &catalog.servers {
        field(&mut hash, alias.as_bytes());
        match revisions.get(alias) {
            Some(revision) => {
                hash.update([1]);
                field(&mut hash, revision.0.as_bytes());
            }
            None => hash.update([0]),
        }
    }
    hash.update(b"vm");
    match &sandbox.apple {
        // Legacy configs without `images` keep the exact v1 byte layout (tag `1`) to avoid re-activation.
        Some(apple) if apple.images.is_empty() => {
            hash.update([1]);
            field(
                &mut hash,
                apple.image.as_deref().unwrap_or_default().as_bytes(),
            );
            // The host-only VM network is part of isolation; changing it must trigger re-activation.
            field(&mut hash, apple.network.as_bytes());
        }
        Some(apple) => {
            hash.update([2]);
            match &apple.image {
                Some(img) => {
                    hash.update([1]);
                    field(&mut hash, img.as_bytes());
                }
                None => hash.update([0]),
            }
            count(&mut hash, apple.images.len());
            for (cli, img) in &apple.images {
                field(&mut hash, cli.as_bytes());
                field(&mut hash, img.as_bytes());
            }
            field(&mut hash, apple.network.as_bytes());
        }
        None => hash.update([0]),
    }
    hash.finalize().into()
}

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod fingerprint_tests;
#[cfg(test)]
mod provider_tests;
#[cfg(test)]
mod tests;

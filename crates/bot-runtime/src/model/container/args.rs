//! Pure `container run` argument assembly and strict input validation for model and anchor VMs.
//!
//! Security boundaries: bind-mount sources must canonicalize under `mount_root`, secret-like env
//! vars pass by name only (`-e NAME` with values in client env, never on `argv`), and relay address
//! env vars are pinned by the host.

use bot_core::sandbox::ModelSpec;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// Pinned CLI binaries accepted by `images/model/bot-init`.
const FIREWALLED_PROGRAMS: [&str; 3] = ["claude", "codex", "agy"];

/// Trusted entrypoint contract for the pinned image (host-configured, never model-selectable).
#[derive(Clone, Debug)]
pub struct Entry {
    pub anchor: String,
    pub anchor_user: (u32, u32),
    /// Minimal startup capabilities for `bot-init`, dropped before execing non-root Claude.
    pub model_caps: Vec<String>,
}

impl Entry {
    /// Contract matching `images/model`: non-root `bot-anchor` (`10001:10001`) and minimal caps for `bot-init`.
    pub fn model_image() -> Self {
        Self {
            anchor: "/usr/local/bin/bot-anchor".into(),
            anchor_user: (10001, 10001),
            model_caps: ["NET_ADMIN", "SETGID", "SETPCAP", "SETUID"]
                .map(String::from)
                .into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContainerConfig {
    pub program: String,
    pub image: String,
    /// Optional per-program digest-pinned images (`claude`, `codex`, `agy`).
    pub images: BTreeMap<String, String>,
    pub network: String,
    pub prefix: String,
    pub instance: String,
    pub mount_root: PathBuf,
    pub cpus: u32,
    pub memory_mib: u32,
    pub entry: Option<Entry>,
    /// Test-only escape hatch allowing startup without a firewalled image entrypoint.
    pub unfirewalled: bool,
    /// Leave `None` in production to use the canonical per-user lock path (`~/.local/state/bot-framework/locks`).
    pub lock_dir: Option<PathBuf>,
    /// Optional private IPv4 CIDR (`a.b.c.d/16..29`) used only when creating a new host-only network.
    pub subnet: Option<String>,
}

pub(super) fn subnet_ok(text: &str) -> bool {
    let Some((address, length)) = text.split_once('/') else {
        return false;
    };
    let (Ok(address), Ok(length)) = (address.parse::<std::net::Ipv4Addr>(), length.parse::<u8>())
    else {
        return false;
    };
    address.is_private() && (16..=29).contains(&length)
}

impl ContainerConfig {
    pub fn new(
        image: impl Into<String>,
        network: impl Into<String>,
        mount_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: "container".into(),
            image: image.into(),
            images: BTreeMap::new(),
            network: network.into(),
            prefix: "botvm".into(),
            instance: "botvm".into(),
            mount_root: mount_root.into(),
            cpus: 2,
            memory_mib: 2048,
            entry: None,
            unfirewalled: false,
            subnet: None,
            lock_dir: None,
        }
    }

    #[must_use]
    pub fn with_image_for(mut self, program: impl Into<String>, image: impl Into<String>) -> Self {
        self.images.insert(program.into(), image.into());
        self
    }

    #[must_use]
    pub fn named(mut self, instance: impl Into<String>) -> Self {
        let name = instance.into();
        self.prefix.clone_from(&name);
        self.instance = name;
        self
    }

    #[must_use]
    pub fn with_entry(mut self, entry: Entry) -> Self {
        self.entry = Some(entry);
        self
    }

    #[must_use]
    pub fn with_lock_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.lock_dir = Some(dir.into());
        self
    }

    #[must_use]
    pub fn with_subnet(mut self, subnet: impl Into<String>) -> Self {
        self.subnet = Some(subnet.into());
        self
    }

    #[must_use]
    pub fn unfirewalled(mut self) -> Self {
        self.unfirewalled = true;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refused(pub &'static str);

#[derive(Debug)]
pub struct RunCommand {
    pub args: Vec<String>,
    pub secret_env: Vec<(String, String)>,
    pub name: String,
}

pub const OWNER_LABEL: &str = "bot.owner";
pub const MANAGED_LABEL: &str = "bot.managed";
pub const INSTANCE_LABEL: &str = "bot.instance";
pub const INCARNATION_LABEL: &str = "bot.incarnation";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Model,
    Anchor,
}

pub struct Plan<'a> {
    pub image: &'a str,
    pub incarnation: &'a str,
    pub serial: u64,
    pub relay: Option<SocketAddr>,
}

fn token_ok(s: &str, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || extra.contains(&b)
        })
}

pub(crate) fn guest_ok(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 255
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != ".." && token_ok(part, &[]))
        || path == "/"
}

fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub(crate) fn secret_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"]
        .iter()
        .any(|word| upper.contains(word))
}

fn source_ok(root: &Path, host: &Path) -> Result<String, Refused> {
    let text = host.to_str().ok_or(Refused("mount_path"))?;
    if text
        .chars()
        .any(|c| c.is_control() || matches!(c, ',' | '"' | '\'' | '='))
    {
        return Err(Refused("mount_path"));
    }
    let real = host.canonicalize().map_err(|_| Refused("mount_missing"))?;
    let base = root.canonicalize().map_err(|_| Refused("mount_root"))?;
    if !real.is_dir() || !real.starts_with(&base) {
        return Err(Refused("mount_outside_root"));
    }
    real.to_str()
        .filter(|s| {
            !s.chars()
                .any(|c| c.is_control() || matches!(c, ',' | '"' | '\'' | '='))
        })
        .map(str::to_owned)
        .ok_or(Refused("mount_path"))
}

pub fn owner_of(spec: &ModelSpec) -> String {
    let a = &spec.attempt;
    format!(
        "{}.{}.g{}.c{}",
        a.profile.0,
        a.run.0,
        a.generation.0,
        a.claim.map_or(0, |c| c.0)
    )
}

pub fn name_of(prefix: &str, owner: &str, incarnation: &str, serial: u64) -> String {
    let hash: String = Sha256::digest(format!("{owner}|{incarnation}").as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{prefix}-{hash}-{serial}")
}

/// Matches strictly `<prefix>-<12 lowercase hex>-<digits>` so prefix-overlapping instances stay disjoint.
pub fn is_ours(prefix: &str, id: &str) -> bool {
    let Some(rest) = id
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    let mut parts = rest.split('-');
    let (Some(hash), Some(serial), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    hash.len() == 12
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !serial.is_empty()
        && serial.bytes().all(|b| b.is_ascii_digit())
}

pub fn label_value(owner: &str) -> String {
    owner
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn image_ref_ok(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 255
        && !image.starts_with('-')
        && !image.chars().any(|c| c.is_control() || c.is_whitespace())
}

pub(super) fn valid(config: &ContainerConfig) -> Result<(), Refused> {
    if !token_ok(&config.prefix, &[])
        || !token_ok(&config.instance, &[])
        || !token_ok(&config.network, &[])
    {
        return Err(Refused("config"));
    }
    if config.subnet.as_deref().is_some_and(|s| !subnet_ok(s)) {
        return Err(Refused("config"));
    }
    if config
        .lock_dir
        .as_ref()
        .is_some_and(|dir| !dir.is_absolute())
    {
        return Err(Refused("config"));
    }
    if let Some(entry) = &config.entry {
        let caps_ok = entry.model_caps.iter().all(|cap| {
            !cap.is_empty()
                && cap.len() <= 32
                && cap != "ALL"
                && cap.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
        });
        if !guest_ok(&entry.anchor)
            || !caps_ok
            || entry.anchor_user.0 == 0
            || entry.anchor_user.1 == 0
        {
            return Err(Refused("entry"));
        }
    }
    if !image_ref_ok(&config.image) {
        return Err(Refused("image"));
    }
    for (program, image) in &config.images {
        if !token_ok(program, &[]) || program.starts_with('-') {
            return Err(Refused("config"));
        }
        if !image_ref_ok(image) {
            return Err(Refused("image"));
        }
    }
    Ok(())
}

pub fn anchor_owner(config: &ContainerConfig) -> String {
    format!("anchor.{}.g0.c0", config.instance)
}

fn base(config: &ContainerConfig, plan: &Plan<'_>, owner: &str, name: &str) -> Vec<String> {
    [
        "run",
        "--rm",
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--cap-drop",
        "ALL",
        "--init",
        "--no-dns",
        "--network",
        &config.network,
        "-c",
        &config.cpus.to_string(),
        "-m",
        &format!("{}M", config.memory_mib),
        "--label",
        &format!("{OWNER_LABEL}={}", label_value(owner)),
        "--label",
        &format!("{MANAGED_LABEL}=1"),
        "--label",
        &format!("{INSTANCE_LABEL}={}", label_value(&config.instance)),
        "--label",
        &format!("{INCARNATION_LABEL}={}", label_value(plan.incarnation)),
        "--name",
        name,
    ]
    .map(String::from)
    .into()
}

pub fn build_anchor(config: &ContainerConfig, plan: &Plan<'_>) -> Result<RunCommand, Refused> {
    valid(config)?;
    let owner = anchor_owner(config);
    let name = name_of(&config.prefix, &owner, plan.incarnation, plan.serial);
    let mut args = base(config, plan, &owner, &name);
    args.extend(["-w".into(), "/".into()]);
    if let Some(entry) = &config.entry {
        args.extend([
            "--entrypoint".into(),
            entry.anchor.clone(),
            "--user".into(),
            format!("{}:{}", entry.anchor_user.0, entry.anchor_user.1),
        ]);
    }
    args.push(plan.image.to_owned());
    if config.entry.is_none() {
        args.extend(["sleep".into(), "2147483647".into()]);
    }
    Ok(RunCommand {
        args,
        secret_env: Vec::new(),
        name,
    })
}

pub fn build(
    config: &ContainerConfig,
    spec: &ModelSpec,
    plan: &Plan<'_>,
) -> Result<RunCommand, Refused> {
    valid(config)?;
    if spec.program.is_empty()
        || spec.program.starts_with('-')
        || spec.program.len() > 255
        || spec.program.chars().any(char::is_control)
        // Firewalled image init only accepts pinned model CLIs (`claude`, `codex`, `agy`).
        || (config.entry.is_some() && !FIREWALLED_PROGRAMS.contains(&spec.program.as_str()))
    {
        return Err(Refused("program"));
    }
    if spec
        .args
        .iter()
        .any(|a| a.contains('\0') || a.len() > 1 << 20)
    {
        return Err(Refused("args"));
    }
    if !guest_ok(&spec.cwd) {
        return Err(Refused("cwd"));
    }
    let relay = match plan.relay {
        Some(SocketAddr::V4(relay)) if relay.port() != 0 => relay,
        _ => return Err(Refused("relay")),
    };
    let owner = owner_of(spec);
    let name = name_of(&config.prefix, &owner, plan.incarnation, plan.serial);
    let mut args = base(config, plan, &owner, &name);
    if let Some(entry) = &config.entry {
        for cap in &entry.model_caps {
            args.extend(["--cap-add".into(), cap.clone()]);
        }
    }
    args.extend(["-w".into(), spec.cwd.clone()]);
    if spec.stdin.is_some() {
        args.push("-i".into());
    }
    let mut guests = Vec::new();
    for (mount, readonly) in spec
        .readonly_mounts
        .iter()
        .map(|m| (m, true))
        .chain(spec.scratch.iter().map(|m| (m, false)))
    {
        if !guest_ok(&mount.guest) || mount.guest == "/" || guests.contains(&mount.guest) {
            return Err(Refused("mount_target"));
        }
        guests.push(mount.guest.clone());
        let source = source_ok(&config.mount_root, &mount.host)?;
        args.push("--mount".into());
        args.push(format!(
            "type=bind,source={source},target={}{}",
            mount.guest,
            if readonly { ",readonly" } else { "" }
        ));
    }
    let canonical = format!("http://{relay}");
    if spec.program == "codex" && !super::codex_args::codex_relay_args_ok(&spec.args, &canonical) {
        return Err(Refused("relay"));
    }
    let mut env: Vec<(String, String)> = Vec::new();
    for (key, value) in &spec.env {
        let is_relay_url = matches!(
            key.as_str(),
            "ANTHROPIC_BASE_URL" | "GOOGLE_GEMINI_BASE_URL"
        );
        if !env_name_ok(key)
            || value.contains('\0')
            || env.iter().any(|(seen, _)| seen == key)
            // Relay env vars are host-owned; specs cannot override them or point base URLs elsewhere.
            || matches!(key.as_str(), "BOT_RELAY_IP" | "BOT_RELAY_PORT")
            || (is_relay_url && value.trim_end_matches('/') != canonical)
        {
            return Err(Refused(if key.starts_with("BOT_RELAY") || is_relay_url {
                "relay"
            } else {
                "env"
            }));
        }
        env.push((key.clone(), value.clone()));
    }
    env.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "ANTHROPIC_BASE_URL" | "GOOGLE_GEMINI_BASE_URL"
        )
    });
    env.extend([
        ("BOT_RELAY_IP".to_owned(), relay.ip().to_string()),
        ("BOT_RELAY_PORT".to_owned(), relay.port().to_string()),
    ]);
    match spec.program.as_str() {
        "agy" => env.push(("GOOGLE_GEMINI_BASE_URL".to_owned(), canonical)),
        "codex" => {}
        _ => env.push(("ANTHROPIC_BASE_URL".to_owned(), canonical)),
    }
    let mut secret_env = Vec::new();
    for (key, value) in &env {
        args.push("-e".into());
        if secret_like(key) {
            args.push(key.clone());
            secret_env.push((key.clone(), value.clone()));
        } else {
            args.push(format!("{key}={value}"));
        }
    }
    args.push(plan.image.to_owned());
    args.push(spec.program.clone());
    args.extend(spec.args.iter().cloned());
    Ok(RunCommand {
        args,
        secret_env,
        name,
    })
}

pub(super) fn pin(image: &str, digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    if let Some((_, given)) = image.split_once('@') {
        return (given == digest).then(|| image.to_owned());
    }
    let name = match image.rfind(':') {
        Some(at) if !image[at..].contains('/') => &image[..at],
        _ => image,
    };
    Some(format!("{name}@{digest}"))
}

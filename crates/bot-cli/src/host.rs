//! Host registry (`host.json`) parsing, demo defaults, UID pinning, and authority fingerprint.
use crate::{
    embeddings::EmbeddingConfig,
    execution::{self, Provider, Sandbox},
    install::{ServerSpec, Servers, parse_all},
    tool_limits::HostLimits,
};
use bot_admin::AdminSettings;
use bot_broker::mcp::remote::Remote;
use bot_gateway::host::{CliAccount, HostConfig};
use bot_profile::{HostCatalog, SlackWorkspace};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

mod lists;

use lists::{
    bind_private_egress, bind_super_admins, is_egress, is_super_admin, private_egress_map,
    profile_map,
};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub struct HostFileError(pub String);

impl fmt::Display for HostFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostFileError {}

fn bad(message: impl Into<String>) -> HostFileError {
    HostFileError(message.into())
}

/// Per-profile pinned private/local egress mappings (`host:port -> ip:port`).
pub type PrivateEgressMap = BTreeMap<String, BTreeMap<(String, u16), std::net::SocketAddr>>;

pub struct HostFile {
    pub gateway: HostConfig,
    pub catalog: HostCatalog,
    pub admin: AdminSettings,
    pub servers: BTreeMap<String, ServerSpec>,
    pub remote_servers: BTreeMap<String, Remote>,
    pub providers: BTreeMap<String, Provider>,
    pub embeddings: Option<EmbeddingConfig>,
    pub sandbox: Sandbox,
    pub limits: HostLimits,
    pub private_egress: PrivateEgressMap,
    pub demo: bool,
}

pub fn parse(text: &str) -> Result<HostFile, HostFileError> {
    let Ok(Value::Object(mut top)) = serde_json::from_str::<Value>(text) else {
        return Err(bad("host config must be a JSON object"));
    };
    if top.remove("version").and_then(|v| v.as_u64()) != Some(1) {
        return Err(bad("version: 1 is required"));
    }
    let gateway = match top.remove("gateway") {
        Some(value) => HostConfig::from_value(value).map_err(|e| bad(e.to_string()))?,
        None => HostConfig::default(),
    };
    let servers = match top.remove("servers") {
        Some(value) => parse_all(&value).map_err(bad)?,
        None => Servers::default(),
    };
    let models = match top.remove("models") {
        Some(value) => profile_map(&value, "models", bot_core::id::is_name)?,
        None => BTreeMap::new(),
    };
    let egress = match top.remove("egress") {
        Some(value) => profile_map(&value, "egress", is_egress)?,
        None => BTreeMap::new(),
    };
    let super_admins = match top.remove("super_admins") {
        Some(value) => profile_map(&value, "super_admins", is_super_admin)?,
        None => BTreeMap::new(),
    };
    let private_egress = match top.remove("private_egress") {
        Some(value) => private_egress_map(&value)?,
        None => BTreeMap::new(),
    };
    let providers = match top.remove("providers") {
        Some(value) => execution::parse_providers(&value).map_err(bad)?,
        None => BTreeMap::new(),
    };
    let embeddings = match top.remove("embeddings") {
        Some(value) => Some(crate::embeddings::parse(&value).map_err(bad)?),
        None => None,
    };
    let sandbox = match top.remove("sandbox") {
        Some(value) => execution::parse_sandbox(&value).map_err(bad)?,
        None => Sandbox::default(),
    };
    let limits = match top.remove("limits") {
        Some(value) => crate::tool_limits::parse(&value).map_err(bad)?,
        None => HostLimits::default(),
    };
    let admin = match top.remove("admin") {
        Some(value) => AdminSettings::from_json(&value).map_err(bad)?,
        None => default_admin(),
    };
    if let Some(unknown) = top.keys().next() {
        return Err(bad(format!("unknown top-level key: {unknown}")));
    }
    Ok(HostFile {
        catalog: HostCatalog {
            tool_call_max_secs: Some(limits.tool_call_max_secs),
            super_admins,
            ..catalog(
                &gateway,
                servers
                    .local
                    .keys()
                    .chain(servers.remote.keys())
                    .cloned()
                    .collect(),
                &servers.remote,
                models,
                egress,
            )
        },
        gateway,
        admin,
        servers: servers.local,
        remote_servers: servers.remote,
        providers,
        embeddings,
        sandbox,
        limits,
        private_egress,
        demo: false,
    })
}

impl HostFile {
    pub fn authority_fingerprint(
        &self,
        revisions: &BTreeMap<String, bot_core::id::Revision>,
    ) -> Result<[u8; 32], String> {
        let gateway = bot_gateway::mapping::fingerprint(&self.gateway)
            .map_err(|e| format!("could not verify entrance mapping: {e}"))?;
        let base = execution::authority_fingerprint(
            &gateway,
            &self.catalog,
            &self.providers,
            &self.sandbox,
            revisions,
        );
        // Each optional binding leaves the base digest unchanged when at its default/empty state.
        let base = crate::tool_limits::bind(base, &self.limits);
        let base = crate::embeddings::bind(base, self.embeddings.as_ref());
        let base = bind_private_egress(base, &self.private_egress);
        let base = bind_super_admins(base, &self.catalog.super_admins);
        Ok(bot_admin::obs_fingerprint(base, &self.admin))
    }

    /// Attaches the synthetic `demo-agenda` server next to `profiles_root` (or from `examples/` in this repo).
    pub fn attach_demo_server(&mut self, profiles_root: Option<&Path>) {
        let sibling = profiles_root
            .and_then(Path::parent)
            .map(|dir| dir.join("servers/demo-agenda"));
        let built_from =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/servers/demo-agenda");
        let root = sibling
            .into_iter()
            .chain([built_from])
            .find_map(|candidate| {
                std::fs::canonicalize(candidate)
                    .ok()
                    .filter(|path| path.is_dir())
            });
        if let Some(root) = root {
            self.servers.insert(
                "demo-agenda".to_owned(),
                ServerSpec {
                    root,
                    command: vec!["bin/server".to_owned(), "--stdio".to_owned()],
                    state: Default::default(),
                },
            );
        }
    }

    /// Resolves `os_user` aliases to numeric UIDs once so fingerprint, routes, and admin identity stay consistent.
    pub fn pin_uids(&mut self) -> Result<(), String> {
        for (alias, account) in &mut self.gateway.cli.accounts {
            let uid = bot_gateway::host::resolve_uid(account)
                .map_err(|e| format!("gateway.cli.accounts.{alias}: {e}"))?;
            *account = CliAccount {
                os_user: None,
                uid: Some(uid),
            };
        }
        Ok(())
    }
}

fn default_admin() -> AdminSettings {
    AdminSettings::from_json(&Value::Object(Default::default()))
        .expect("empty admin settings are always valid")
}

fn catalog(
    gateway: &HostConfig,
    servers: BTreeSet<String>,
    remote: &BTreeMap<String, Remote>,
    models: BTreeMap<String, BTreeSet<String>>,
    egress: BTreeMap<String, BTreeSet<String>>,
) -> HostCatalog {
    HostCatalog {
        servers,
        remote_servers: remote
            .iter()
            .map(|(alias, remote)| (alias.clone(), remote.target_text()))
            .collect(),
        models,
        cli_accounts: gateway.cli.accounts.keys().cloned().collect(),
        slack: gateway
            .slack
            .workspaces
            .iter()
            .map(|(alias, ws)| {
                (
                    alias.clone(),
                    SlackWorkspace {
                        channels: ws.channels.keys().cloned().collect(),
                        dm: ws.dm,
                    },
                )
            })
            .collect(),
        http_endpoints: gateway.http.endpoints.keys().cloned().collect(),
        openai_endpoints: gateway
            .http
            .openai
            .iter()
            .map(|(alias, endpoint)| {
                (
                    alias.clone(),
                    endpoint
                        .keys
                        .values()
                        .map(|slot| slot.subject.clone())
                        .collect(),
                )
            })
            .collect(),
        egress,
        tool_call_max_secs: None,
        super_admins: BTreeMap::new(),
    }
}

pub fn load(path: &Path) -> Result<HostFile, HostFileError> {
    let meta = std::fs::metadata(path).map_err(|_| bad("could not read host config file"))?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(bad("host config must be a regular file at most 1 MiB"));
    }
    let text = std::fs::read_to_string(path).map_err(|_| bad("could not read host config file"))?;
    parse(&text)
}

/// Built-in demo host mapping both `owner` and `team` CLI accounts to `uid`.
pub fn demo(uid: u32) -> HostFile {
    let mut gateway = HostConfig::default();
    for alias in ["owner", "team"] {
        gateway.cli.accounts.insert(
            alias.into(),
            CliAccount {
                os_user: None,
                uid: Some(uid),
            },
        );
    }
    let mut catalog = HostCatalog::demo();
    catalog.cli_accounts = gateway.cli.accounts.keys().cloned().collect();
    HostFile {
        gateway,
        catalog,
        admin: default_admin(),
        servers: BTreeMap::new(),
        remote_servers: BTreeMap::new(),
        providers: BTreeMap::from([("assistant".to_owned(), Provider::Demo)]),
        embeddings: None,
        sandbox: Sandbox::default(),
        limits: HostLimits::default(),
        private_egress: BTreeMap::new(),
        demo: true,
    }
}

#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod trace_authority_tests;

#[cfg(test)]
mod remote_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "version": 1,
        "gateway": {
            "cli": {"accounts": {"owner": {"uid": 501}}},
            "slack": {"workspaces": {"home": {"team_id": "T1", "bot_token": "bot-token",
                "app_token": "app-token", "dm": true, "channels": {"planning": "C1"}}}},
            "http": {"endpoints": {"reminders": {"issuer": "i", "audience": "a", "key": "k"}}}
        },
        "servers": {"demo-agenda": {"root": "/opt/bot/servers/demo-agenda", "command": ["bin/server", "--stdio"]}},
        "models": {"personal": ["assistant"]},
        "egress": {"personal": ["calendar.example:443"]}
    }"#;

    #[test]
    fn derives_the_profile_catalog_from_the_gateway_section() {
        let host = parse(FULL).unwrap();
        let catalog = &host.catalog;
        assert!(catalog.servers.contains("demo-agenda"));
        assert!(catalog.allows_model("personal", "assistant"));
        assert!(catalog.cli_accounts.contains("owner"));
        assert!(catalog.http_endpoints.contains("reminders"));
        let home = &catalog.slack["home"];
        assert!(home.dm && home.channels.contains("planning"));
        assert_eq!(catalog.egress["personal"].len(), 1);
        assert!(!host.demo);
    }

    #[test]
    fn the_catalog_lists_the_subjects_the_key_slots_of_each_openai_entrance_point_at() {
        let host = parse(
            r#"{"version": 1, "gateway": {"http": {"endpoints": {
                "openai": {"kind": "openai", "keys": {
                    "a": {"secret": "key-a", "subject": "build"},
                    "b": {"secret": "key-b", "subject": "build"},
                    "c": {"secret": "key-c", "subject": "ops"}}},
                "hooks": {"issuer": "i", "audience": "a", "key": "k"}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            host.catalog.openai_endpoints,
            BTreeMap::from([(
                "openai".to_owned(),
                BTreeSet::from(["build".to_owned(), "ops".to_owned()])
            )])
        );
        assert!(host.catalog.http_endpoints.contains("hooks"));
        assert!(!host.catalog.http_endpoints.contains("openai"));
    }

    #[test]
    fn the_execution_sections_are_checked_in_shape_but_never_required() {
        let host = parse(
            r#"{"version": 1, "providers": {"assistant": {"kind": "demo"}},
                "sandbox": {"srt": {"binary": "/opt/srt", "work_root": "/var/work"}}}"#,
        )
        .unwrap();
        assert_eq!(host.providers["assistant"], Provider::Demo);
        assert!(host.sandbox.srt.is_some() && host.sandbox.apple.is_none());
        let bare = parse(r#"{"version": 1}"#).unwrap();
        assert!(bare.providers.is_empty() && bare.sandbox == Sandbox::default());
        for bad in [
            r#"{"version": 1, "providers": {"a": {"kind": "pi"}}}"#,
            r#"{"version": 1, "sandbox": {"apple": {"image": "x", "network": "n", "relay_bind": "0.0.0.0:1"}}}"#,
            r#"{"version": 1, "sandbox": {"srt": {"binary": "srt", "work_root": "/w"}}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(demo(501).providers["assistant"], Provider::Demo);
    }

    #[test]
    fn the_demo_host_connects_the_synthetic_agenda_server_next_to_the_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("servers/demo-agenda");
        std::fs::create_dir_all(server.join("bin")).unwrap();
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        let mut host = demo(501);
        assert!(host.servers.is_empty(), "no server before attaching");
        host.attach_demo_server(Some(&profiles));
        let spec = &host.servers["demo-agenda"];
        assert_eq!(spec.root, std::fs::canonicalize(&server).unwrap());
        assert_eq!(spec.command, ["bin/server", "--stdio"]);
        let mut fallback = demo(501);
        fallback.attach_demo_server(Some(&dir.path().join("elsewhere/profiles")));
        let root = &fallback.servers["demo-agenda"].root;
        assert!(root.ends_with("examples/servers/demo-agenda"), "{root:?}");
        assert!(fallback.catalog.servers.contains("demo-agenda"));
    }

    #[test]
    fn the_demo_host_knows_both_example_profiles() {
        let host = demo(501);
        let accounts: Vec<&str> = host
            .gateway
            .cli
            .accounts
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(accounts, ["owner", "team"]);
        assert!(
            host.gateway
                .cli
                .accounts
                .values()
                .all(|a| a.uid == Some(501))
        );
        assert_eq!(
            host.catalog.cli_accounts,
            ["owner", "team"].map(String::from).into()
        );
        for profile in ["personal", "team"] {
            assert!(host.catalog.allows_model(profile, "assistant"), "{profile}");
        }
    }

    #[test]
    fn account_names_are_resolved_once_into_the_effective_uid() {
        let me = bot_gateway::cli::socket::current_uid().unwrap();
        let name = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout;
        let name = String::from_utf8_lossy(&name).trim().to_owned();
        let text = format!(
            r#"{{"version": 1, "gateway": {{"cli": {{"accounts": {{"owner": {{"os_user": "{name}"}}, "fixed": {{"uid": 4242}}}}}}}}}}"#
        );
        let mut host = parse(&text).unwrap();
        let before = bot_gateway::mapping::fingerprint(&host.gateway).unwrap();
        host.pin_uids().unwrap();
        let accounts = &host.gateway.cli.accounts;
        assert_eq!(
            (accounts["owner"].os_user.clone(), accounts["owner"].uid),
            (None, Some(me))
        );
        assert_eq!(accounts["fixed"].uid, Some(4242));
        assert_eq!(
            bot_gateway::mapping::fingerprint(&host.gateway).unwrap(),
            before
        );
        let mut ghost = parse(
            r#"{"version": 1, "gateway": {"cli": {"accounts": {"owner": {"os_user": "no-such-user-zzz"}}}}}"#,
        )
        .unwrap();
        assert!(ghost.pin_uids().unwrap_err().contains("owner"));
    }

    #[test]
    fn the_admin_section_is_parsed_by_the_admin_crate_and_defaults_to_local() {
        let default = parse(r#"{"version": 1}"#).unwrap();
        assert_eq!(default.admin.bind.to_string(), "127.0.0.1:8787");
        let custom =
            parse(r#"{"version": 1, "admin": {"bind": "127.0.0.1:9100", "max_failures": 3}}"#)
                .unwrap();
        assert_eq!(
            (custom.admin.bind.to_string(), custom.admin.max_failures),
            ("127.0.0.1:9100".to_owned(), 3)
        );
        for bad_admin in [
            r#"{"bind": "0.0.0.0:8787"}"#,
            r#"{"nope": true}"#,
            r#""text""#,
        ] {
            let text = format!(r#"{{"version": 1, "admin": {bad_admin}}}"#);
            assert!(parse(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn rejects_unknown_keys_missing_version_and_bad_shapes() {
        for (json, needle) in [
            (r#"{"version": 1, "adnim": {}}"#, "adnim"),
            (r#"{"gateway": {}}"#, "version"),
            (r#"{"version": 2}"#, "version"),
            ("[]", "JSON object"),
            (r#"{"version": 1, "servers": "demo"}"#, "object"),
            (r#"{"version": 1, "servers": ["demo-agenda"]}"#, "servers"),
            (
                r#"{"version": 1, "servers": {"Demo": {"root": "/a", "command": ["x"]}}}"#,
                "servers.Demo",
            ),
            (
                r#"{"version": 1, "servers": {"demo": {"root": "/a", "command": ["x"], "revision": "1"}}}"#,
                "revision",
            ),
            (r#"{"version": 1, "models": {"Bad": ["a"]}}"#, "models"),
            (r#"{"version": 1, "models": {"p": ["A B"]}}"#, "models.p"),
            (
                r#"{"version": 1, "egress": {"p": ["no-port"]}}"#,
                "egress.p",
            ),
            (r#"{"version": 1, "gateway": {"slak": {}}}"#, "slak"),
        ] {
            let message = parse(json).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(message.contains(needle), "{json} -> {message}");
        }
        assert!(parse(r#"{"version": 1}"#).is_ok());
    }

    #[test]
    fn the_demo_host_maps_the_owner_account_to_the_running_user() {
        let host = demo(501);
        assert!(host.demo);
        assert_eq!(host.gateway.cli.accounts["owner"].uid, Some(501));
        assert!(host.catalog.cli_accounts.contains("owner"));
        assert!(host.catalog.servers.contains("demo-agenda"));
        assert!(host.catalog.allows_model("personal", "assistant"));
    }

    #[test]
    fn load_refuses_missing_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("none.json")).is_err());
        let big = dir.path().join("big.json");
        std::fs::write(&big, vec![b' '; (MAX_BYTES + 1) as usize]).unwrap();
        assert!(load(&big).is_err());
        let ok = dir.path().join("ok.json");
        std::fs::write(&ok, r#"{"version": 1}"#).unwrap();
        assert!(load(&ok).is_ok());
    }
}

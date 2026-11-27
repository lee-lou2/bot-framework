//! Offline pre-flight validation (`bot validate`) for profiles, host config, routes, and secrets.
use crate::{
    execution::{CliProvider, Provider},
    host::HostFile,
    load::{check_entrances, load, secret_store},
    settings::{SecretsKind, Settings},
};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, SecretStore},
    profile::{Builtin, Snapshot, TriggerSource},
};
use bot_gateway::{host::HostConfig, http::token::MIN_KEY_BYTES};
use bot_runtime::model::CliKind;
use std::collections::BTreeSet;

mod trace;
pub use trace::{all_needed_secrets, host_obs_secrets, trace_secrets};

pub fn needed_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = snapshot
        .secrets
        .iter()
        .map(|s| s.as_str().to_owned())
        .collect();
    for agent in snapshot.agents.values() {
        for skill in agent.skills.values() {
            for tool in skill.tools.values() {
                names.extend(tool.secrets.values().map(|s| s.as_str().to_owned()));
                // Remote bearer secrets belong to the using profile's secret namespace.
                names.extend(
                    host.remote_servers
                        .get(&tool.server)
                        .and_then(|remote| remote.bearer.as_ref())
                        .map(|name| name.as_str().to_owned()),
                );
            }
        }
    }
    names.extend(entrance_secrets(snapshot, &host.gateway));
    names.extend(model_secrets(snapshot, host));
    names.extend(trace_secrets(snapshot, host));
    names
}

pub fn model_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = host
        .catalog
        .models
        .get(&snapshot.id.0)
        .into_iter()
        .flatten()
        .filter_map(|alias| host.providers.get(alias).and_then(Provider::secret))
        .map(str::to_owned)
        .collect();
    if let Some(secret) = host.embeddings.as_ref().and_then(|e| e.secret.as_deref()) {
        names.insert(secret.to_owned());
    }
    if let Some(secret) = host.memory_judge.as_ref().and_then(|j| j.secret.as_deref()) {
        names.insert(secret.to_owned());
    }
    names
}

pub fn entrance_secrets(snapshot: &Snapshot, gateway: &HostConfig) -> BTreeSet<String> {
    use bot_core::profile::Adapter;
    let mut names = BTreeSet::new();
    for binding in snapshot.bindings.values() {
        match &binding.source {
            Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace } => {
                if let Some(ws) = gateway.slack.workspaces.get(workspace) {
                    names.insert(ws.bot_token.clone());
                    names.extend(ws.app_token.clone());
                }
            }
            Adapter::Http { endpoint } => {
                if let Some(config) = gateway.http.endpoints.get(endpoint) {
                    names.insert(config.key.clone());
                }
            }
            Adapter::OpenAi { endpoint, .. } => {
                if let Some(config) = gateway.http.openai.get(endpoint) {
                    names.extend(config.keys.values().map(|slot| slot.secret.clone()));
                }
            }
            Adapter::Cli { .. } => {}
        }
    }
    // Private DM intake requires the per-profile `private-intake` fingerprint key for deduplication.
    if has_private_triggers(snapshot) {
        names.insert(bot_broker::private::FINGERPRINT_SECRET.to_owned());
    }
    names
}

pub fn has_private_triggers(snapshot: &Snapshot) -> bool {
    snapshot
        .agents
        .values()
        .flat_map(|agent| &agent.triggers)
        .any(|t| t.private && !matches!(t.on, TriggerSource::Cron { .. }))
}

pub fn missing_model_sandbox(snapshot: &Snapshot, host: &HostFile) -> Option<String> {
    let kinds: BTreeSet<CliKind> = host
        .catalog
        .models
        .get(&snapshot.id.0)
        .into_iter()
        .flatten()
        .filter_map(|alias| host.providers.get(alias).and_then(Provider::cli_kind))
        .collect();
    if kinds.is_empty() {
        return None;
    }
    let Some(apple) = &host.sandbox.apple else {
        let names = kinds
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            "{}: allows a {names} alias, but host config is missing sandbox.apple (digest-pinned image and host-only network) — see docs/model-image.md",
            snapshot.id.0
        ));
    };
    let missing: Vec<_> = kinds
        .iter()
        .filter(|kind| apple.image_for(**kind).is_none())
        .map(|kind| format!("sandbox.apple.images.{}", kind.program()))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "{}: sandbox.apple is missing digest-pinned image(s) ({}) — see docs/model-image.md",
        snapshot.id.0,
        missing.join(", ")
    ))
}

fn builtin_name(tool: Builtin) -> &'static str {
    match tool {
        Builtin::Read => "read",
        Builtin::Grep => "grep",
        Builtin::Glob => "glob",
    }
}

pub fn unsupported_builtin_tools(snapshot: &Snapshot, host: &HostFile) -> Vec<String> {
    let mut problems = Vec::new();
    for (agent_id, agent) in &snapshot.agents {
        if agent.builtin.is_empty() {
            continue;
        }
        match host.providers.get(&agent.model) {
            Some(Provider::Api(_)) => {
                let list = agent
                    .builtin
                    .iter()
                    .copied()
                    .map(builtin_name)
                    .collect::<Vec<_>>()
                    .join(", ");
                problems.push(format!(
                    "{}: agent {} declares built-in tools ({}), but model alias {} uses a direct API provider that does not support built-in tools",
                    snapshot.id.0, agent_id.0, list, agent.model
                ));
            }
            Some(Provider::Cli(CliProvider {
                kind: CliKind::Claude,
                ..
            })) => {
                let bad: Vec<_> = agent
                    .builtin
                    .iter()
                    .copied()
                    .filter(|b| matches!(b, Builtin::Grep | Builtin::Glob))
                    .map(builtin_name)
                    .collect();
                if !bad.is_empty() {
                    problems.push(format!(
                        "{}: agent {} declares built-in tools ({}), but claude-cli (--bare VM) supports `read` only",
                        snapshot.id.0,
                        agent_id.0,
                        bad.join(", ")
                    ));
                }
            }
            Some(Provider::Cli(CliProvider {
                kind: kind @ (CliKind::Codex | CliKind::Agy),
                ..
            })) => {
                let list = agent
                    .builtin
                    .iter()
                    .copied()
                    .map(builtin_name)
                    .collect::<Vec<_>>()
                    .join(", ");
                problems.push(format!(
                    "{}: agent {} declares built-in tools ({}), but {} does not support built-in file tools",
                    snapshot.id.0,
                    agent_id.0,
                    list,
                    kind.as_str()
                ));
            }
            Some(Provider::Demo) | None => {}
        }
    }
    problems
}

pub async fn missing_secrets(
    store: &dyn SecretStore,
    snapshot: &Snapshot,
    host: &HostFile,
    kind: &SecretsKind,
) -> Vec<String> {
    missing_named(store, &snapshot.id, needed_secrets(snapshot, host), kind).await
}

pub async fn missing_named(
    store: &dyn SecretStore,
    profile: &ProfileId,
    names: BTreeSet<String>,
    kind: &SecretsKind,
) -> Vec<String> {
    let profile = profile.clone();
    let mut missing = Vec::new();
    let handle = store.for_profile(profile.clone()).await;
    for name in names {
        let Some(secret) = SecretName::parse(&name) else {
            missing.push(format!("{}: invalid secret name {name}", profile.0));
            continue;
        };
        let found = match &handle {
            Ok(handle) => handle.resolve(secret).await,
            Err(_) => Err(Error::Unavailable),
        };
        match found {
            Ok(_) => {}
            Err(Error::Missing) => {
                let hint = kind.missing_hint(&profile.0, &name);
                missing.push(format!("{}: missing secret {name}{hint}", profile.0));
            }
            Err(_) => missing.push(format!(
                "{}: failed to read secret {name} (store error)",
                profile.0
            )),
        }
    }
    missing
}

pub async fn weak_http_keys(
    store: &dyn SecretStore,
    snapshot: &Snapshot,
    gateway: &HostConfig,
) -> Vec<String> {
    use bot_core::profile::Adapter;
    let Ok(handle) = store.for_profile(snapshot.id.clone()).await else {
        return Vec::new();
    };
    let mut weak = Vec::new();
    for binding in snapshot.bindings.values() {
        let Adapter::Http { endpoint } = &binding.source else {
            continue;
        };
        let Some(config) = gateway.http.endpoints.get(endpoint) else {
            continue;
        };
        let Some(name) = SecretName::parse(&config.key) else {
            continue;
        };
        if let Ok(value) = handle.resolve(name).await
            && value.0.len() < MIN_KEY_BYTES
        {
            weak.push(format!(
                "{}: HTTP endpoint {endpoint} key {} is too short (must be at least {MIN_KEY_BYTES} bytes)",
                snapshot.id.0, config.key
            ));
        }
    }
    weak
}

/// Validates OpenAI entrance key slot shape and cross-slot uniqueness using SHA-256 hashes (never printing key values).
pub async fn weak_openai_keys(
    store: &dyn SecretStore,
    snapshots: &[std::sync::Arc<Snapshot>],
    gateway: &HostConfig,
) -> Vec<String> {
    use bot_core::profile::Adapter;
    use bot_gateway::host::openai_key_shaped;
    use sha2::{Digest, Sha256};
    let mut problems = Vec::new();
    let mut seen: std::collections::BTreeMap<[u8; 32], (String, String, String)> =
        Default::default();
    for snapshot in snapshots {
        let Ok(handle) = store.for_profile(snapshot.id.clone()).await else {
            continue;
        };
        let endpoints: BTreeSet<&String> = snapshot
            .bindings
            .values()
            .filter_map(|binding| match &binding.source {
                Adapter::OpenAi { endpoint, .. } => Some(endpoint),
                _ => None,
            })
            .collect();
        for endpoint in endpoints {
            let Some(config) = gateway.http.openai.get(endpoint) else {
                continue;
            };
            for (slot, key) in &config.keys {
                let Some(name) = SecretName::parse(&key.secret) else {
                    continue;
                };
                let Ok(value) = handle.resolve(name).await else {
                    continue;
                };
                let value = value.0.trim_ascii();
                if !openai_key_shaped(value) {
                    problems.push(format!(
                        "{}: OpenAI endpoint {endpoint} key slot {slot} has invalid key shape (must be 32..=256 non-whitespace ASCII bytes)",
                        snapshot.id.0
                    ));
                    continue;
                }
                let here = (snapshot.id.0.clone(), endpoint.clone(), slot.clone());
                if let Some(first) = seen.insert(Sha256::digest(value).into(), here.clone()) {
                    problems.push(format!(
                        "{}: OpenAI endpoint {endpoint} key slot {slot} reuses the same key value as {}/{} slot {} — every slot must have a distinct key",
                        here.0, first.0, first.1, first.2
                    ));
                }
            }
        }
    }
    problems
}

pub async fn run(settings: &Settings) -> Result<(), String> {
    let loaded = load(settings)?;
    let active = loaded.set.active().clone();
    check_entrances(active.clone(), &loaded.host)?;
    let store = secret_store(settings)?;
    let mut problems = Vec::new();
    let mut report = Vec::new();
    let revisions =
        match crate::install::measure_all(&loaded.host.servers, &loaded.host.remote_servers) {
            Ok(revisions) => {
                for (alias, revision) in &revisions {
                    report.push(format!(
                        "Server {alias}: revision {}",
                        revision.0.get(..12).unwrap_or(&revision.0)
                    ));
                }
                revisions
            }
            Err(problem) => {
                problems.push(problem);
                Default::default()
            }
        };
    for snapshot in active.values() {
        problems.extend(missing_model_sandbox(snapshot, &loaded.host));
        problems.extend(unsupported_builtin_tools(snapshot, &loaded.host));
        problems.extend(weak_http_keys(&*store, snapshot, &loaded.host.gateway).await);
        problems.extend(crate::serve::seed::problems(snapshot));
    }
    for (profile, names) in all_needed_secrets(active.values().map(AsRef::as_ref), &loaded.host) {
        problems.extend(missing_named(&*store, &profile, names, &settings.secrets).await);
    }
    let snapshots: Vec<_> = active.values().cloned().collect();
    problems.extend(weak_openai_keys(&*store, &snapshots, &loaded.host.gateway).await);
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let publication = crate::serve::Publication::of(&loaded.set, &loaded.host, &revisions)?;
    if loaded.host.demo {
        println!(
            "BOT_HOST_CONFIG is not set; using built-in demo host (CLI accounts owner, team = current user)"
        );
    }
    println!("{}", loaded.set.render());
    for line in report {
        println!("{line}");
    }
    publication.print_grants();
    println!(
        "Activation revision {} (profile set + host authority config)",
        publication
            .revision
            .0
            .get(..12)
            .unwrap_or(&publication.revision.0)
    );
    println!("Entrance and secret checks passed");
    Ok(())
}

pub fn known_profile(active: &bot_gateway::routes::Active, name: &str) -> bool {
    active.contains_key(&ProfileId(name.to_owned()))
}

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod tests;

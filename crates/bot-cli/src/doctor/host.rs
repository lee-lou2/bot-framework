//! Read-only host config, model provider, and tool server checks for `bot doctor`.
use super::{Item, State, capped};
use crate::{
    host::HostFile,
    install,
    load::load_host,
    serve::{self, remote::literal_ok},
    settings::Settings,
};
use bot_core::id::Revision;
use bot_gateway::cli::socket::current_uid;
use std::collections::{BTreeMap, BTreeSet};

pub struct Checked {
    pub file: HostFile,
    pub revisions: Option<BTreeMap<String, Revision>>,
}

pub fn check(settings: &Settings, items: &mut Vec<Item>) -> Option<Checked> {
    let Ok(uid) = current_uid() else {
        items.push(Item::new(
            State::Fail,
            "Host config",
            "could not determine OS user ID (id -u)",
        ));
        return None;
    };
    let host = match load_host(settings, uid) {
        Ok(host) => host,
        Err(error) => {
            let mut details = Vec::new();
            if let Some(path) = &settings.host_config {
                details.push(format!("file: {}", path.display()));
            }
            details.push(
                "check the path and JSON format (see docs/profile-spec.md, or run `bot init <name>`)"
                    .to_owned(),
            );
            items.push(Item::new(State::Fail, "Host config", error).with(details));
            return None;
        }
    };
    items.push(summary(&host, settings));
    items.push(providers(&host));
    let (item, revisions) = servers(&host);
    items.push(item);
    Some(Checked {
        file: host,
        revisions,
    })
}

fn summary(host: &HostFile, settings: &Settings) -> Item {
    let aliases: BTreeSet<&String> = host.catalog.models.values().flatten().collect();
    let counts = format!(
        "model aliases {} · providers {} · tool servers {} · CLI accounts {} · Slack workspaces {} · HTTP endpoints {} · OpenAI endpoints {}",
        aliases.len(),
        host.providers.len(),
        host.servers.len() + host.remote_servers.len(),
        host.gateway.cli.accounts.len(),
        host.gateway.slack.workspaces.len(),
        host.gateway.http.endpoints.len(),
        host.gateway.http.openai.len()
    );
    match &settings.host_config {
        Some(path) => {
            Item::new(State::Pass, "Host config", path.display().to_string()).with([counts])
        }
        None => Item::new(
            State::Warn,
            "Host config",
            "neither BOT_HOST_CONFIG nor --host is set; using the built-in demo host (not for production)",
        )
        .with([
            counts,
            "pass `--host <host.json>` or set BOT_HOST_CONFIG for real configuration".to_owned(),
        ]),
    }
}

fn providers(host: &HostFile) -> Item {
    let lines: Vec<String> = host
        .providers
        .iter()
        .map(|(alias, provider)| provider.describe(alias))
        .collect();
    // Reuse the exact worker startup alias check.
    let problems = serve::check_model_aliases(host).err();
    let state = if problems.is_none() {
        State::Pass
    } else {
        State::Fail
    };
    Item::new(
        state,
        "Model providers",
        format!("{}", host.providers.len()),
    )
    .with(problems)
    .with(capped(lines, 20))
}

fn servers(host: &HostFile) -> (Item, Option<BTreeMap<String, Revision>>) {
    if host.servers.is_empty() && host.remote_servers.is_empty() {
        return (
            Item::new(State::Pass, "Tool servers", "none configured"),
            Some(BTreeMap::new()),
        );
    }
    let measured = install::measure_all(&host.servers, &host.remote_servers);
    let mut problems = Vec::new();
    if let Err(problem) = &measured {
        problems.push(problem.clone());
        problems.push(
            "verify that `root` is an existing absolute directory containing the server executable"
                .to_owned(),
        );
    }
    if let Err(problem) = serve::refuse_required_state(&host.servers) {
        problems.push(problem);
    }
    let mut lines = Vec::new();
    for (alias, remote) in &host.remote_servers {
        let literal = remote.host.parse::<std::net::IpAddr>().is_ok();
        if literal && !literal_ok(&remote.host, remote.tls, false) {
            problems.push(format!(
                "servers.{alias}: disallowed IP literal — https requires a public IP and http allows loopback only (private/reserved addresses are rejected)"
            ));
        }
        let auth = match (&remote.bearer, &remote.oauth) {
            (Some(name), _) => format!("bearer secret {}", name.as_str()),
            (None, Some(name)) => format!("oauth credential {}", name.as_str()),
            (None, None) => "no auth".to_owned(),
        };
        let scheme = if remote.tls { "https" } else { "http" };
        let resolve = if literal {
            ""
        } else {
            " · DNS resolution checked at connect time"
        };
        lines.push(format!(
            "{alias}: remote {scheme} · {}:{} · {auth}{resolve}",
            remote.host, remote.port
        ));
    }
    if !problems.is_empty() {
        let item = Item::new(
            State::Fail,
            "Tool servers",
            "invalid configuration prevents startup",
        )
        .with(problems);
        return (item, measured.ok());
    }
    let revisions = measured.unwrap_or_default();
    lines.extend(
        host.servers
            .keys()
            .filter_map(|alias| revisions.get(alias).map(|revision| (alias, revision)))
            .map(|(alias, revision)| {
                format!(
                    "{alias}: local revision {}",
                    revision.0.get(..12).unwrap_or(&revision.0)
                )
            }),
    );
    let item = Item::new(
        State::Pass,
        "Tool servers",
        format!(
            "{} — revisions computed (no servers started or contacted)",
            revisions.len()
        ),
    )
    .with(capped(lines, 20));
    (item, Some(revisions))
}

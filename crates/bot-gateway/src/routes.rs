//! Gateway routing table builder validating entrance uniqueness, trigger determinism, and private-DM precedence.
use crate::{
    entrance::{Conflict, Entrance, Owner, Real, Table},
    host::{CliAccount, HostConfig, HostError},
};
use bot_core::{
    id::{AgentId, BindingId, ProfileId},
    intake::PushTransportId,
    profile::{Adapter, Binding, Snapshot, Trigger, TriggerSource, TriggerTarget},
};
use std::{collections::BTreeMap, fmt, sync::Arc};

pub type Active = Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;

pub(crate) fn reply_transport(adapter: &Adapter) -> Option<PushTransportId> {
    match adapter {
        Adapter::SlackDm { .. } | Adapter::SlackChannel { .. } => Some(PushTransportId::SLACK),
        Adapter::TelegramDm { .. } | Adapter::TelegramGroups { .. } => {
            Some(PushTransportId::TELEGRAM)
        }
        Adapter::Cli { .. } | Adapter::Http { .. } | Adapter::OpenAi { .. } => None,
    }
}

/// Reserved HTTP path prefix for request status queries; triggers cannot bind under it.
pub const STATUS_SEGMENT: &str = "_requests";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutesError(pub String);

impl fmt::Display for RoutesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RoutesError {}

impl From<Box<Conflict>> for RoutesError {
    fn from(conflict: Box<Conflict>) -> Self {
        RoutesError(conflict.to_string())
    }
}

impl From<HostError> for RoutesError {
    fn from(error: HostError) -> Self {
        RoutesError(error.0)
    }
}

#[derive(Clone, Debug)]
pub struct BoundTrigger {
    pub agent: AgentId,
    pub trigger: Trigger,
}

pub struct Route<'a> {
    pub snapshot: &'a Arc<Snapshot>,
    pub binding_id: &'a BindingId,
    pub binding: &'a Binding,
    /// Sorted by ascending `(order, agent, trigger.id)`.
    pub triggers: &'a [BoundTrigger],
}

pub struct Routes {
    source: Active,
    table: Table<Real>,
    triggers: BTreeMap<(ProfileId, BindingId), Vec<BoundTrigger>>,
}

impl Routes {
    pub fn build(
        source: Active,
        host: &HostConfig,
        uid_of: &dyn Fn(&CliAccount) -> Result<u32, HostError>,
    ) -> Result<Self, RoutesError> {
        let owners = || {
            source.iter().flat_map(|(profile, snapshot)| {
                snapshot.bindings.iter().map(move |(binding, entry)| {
                    (
                        Entrance::from(&entry.source),
                        Owner {
                            profile: profile.clone(),
                            binding: binding.clone(),
                        },
                    )
                })
            })
        };
        // Check uniqueness at both alias level and resolved host-target level.
        Table::build_described(owners())?;
        let mut resolved = Vec::new();
        for (entrance, owner) in owners() {
            let real = resolve(&entrance, &owner, host, uid_of)?;
            resolved.push((real, owner, entrance));
        }
        let table = Table::build(resolved)?;
        one_profile_per_openai_entrance(&table)?;
        let triggers = collect_triggers(&source)?;
        Ok(Self {
            source,
            table,
            triggers,
        })
    }

    pub fn find(&self, real: &Real) -> Option<Route<'_>> {
        let owner = self.table.get(real)?;
        let snapshot = self.source.get(&owner.profile)?;
        let binding = snapshot.bindings.get(&owner.binding)?;
        let triggers = self
            .triggers
            .get(&(owner.profile.clone(), owner.binding.clone()))
            .map_or(&[][..], Vec::as_slice);
        Some(Route {
            snapshot,
            binding_id: &owner.binding,
            binding,
            triggers,
        })
    }

    pub fn cli_profiles(&self, uid: u32) -> Vec<ProfileId> {
        self.table
            .iter()
            .filter_map(|(real, _)| match real {
                Real::Cli { uid: u, profile } if *u == uid => Some(profile.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn http_owner(&self, endpoint: &str) -> Option<&Owner> {
        self.table.get(&Real::Http {
            endpoint: endpoint.to_owned(),
        })
    }

    pub fn openai_models<'a>(
        &'a self,
        endpoint: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a Owner)> + 'a {
        self.table
            .iter()
            .filter_map(move |(real, owner)| match real {
                Real::HttpModel { endpoint: e, model } if e == endpoint => {
                    Some((model.as_str(), owner))
                }
                _ => None,
            })
    }

    pub fn openai_profile<'a>(&'a self, endpoint: &'a str) -> Option<&'a ProfileId> {
        self.openai_models(endpoint)
            .next()
            .map(|(_, owner)| &owner.profile)
    }

    pub fn profiles(&self) -> impl Iterator<Item = &Arc<Snapshot>> {
        self.source.values()
    }

    pub fn snapshot(&self, profile: &ProfileId) -> Option<&Arc<Snapshot>> {
        self.source.get(profile)
    }

    pub fn is_current(&self, active: &Active) -> bool {
        Arc::ptr_eq(&self.source, active)
    }

    pub fn entrances(&self) -> impl Iterator<Item = (&Real, &Owner)> {
        self.table.iter()
    }
}

/// Each OpenAI endpoint alias must map to at most one profile so key-to-profile authorization stays unambiguous.
fn one_profile_per_openai_entrance(table: &Table<Real>) -> Result<(), RoutesError> {
    let mut first: BTreeMap<&str, &Owner> = BTreeMap::new();
    for (real, owner) in table.iter() {
        let Real::HttpModel { endpoint, .. } = real else {
            continue;
        };
        match first.get(endpoint.as_str()) {
            Some(earlier) if earlier.profile != owner.profile => {
                return Err(RoutesError(format!(
                    "Entrance conflict: {earlier} and {owner} both use HTTP endpoint {endpoint}. Each OpenAI-compatible endpoint alias may belong to only one profile — change or remove one binding."
                )));
            }
            Some(_) => {}
            None => {
                first.insert(endpoint, owner);
            }
        }
    }
    Ok(())
}

fn unknown(owner: &Owner, what: &str, alias: &str) -> RoutesError {
    RoutesError(format!(
        "{owner}: unregistered {what} alias {alias} in host config"
    ))
}

fn resolve(
    entrance: &Entrance,
    owner: &Owner,
    host: &HostConfig,
    uid_of: &dyn Fn(&CliAccount) -> Result<u32, HostError>,
) -> Result<Real, RoutesError> {
    match entrance {
        Entrance::Cli { account } => {
            let entry = host
                .cli
                .accounts
                .get(account)
                .ok_or_else(|| unknown(owner, "cli account", account))?;
            Ok(Real::Cli {
                uid: uid_of(entry)?,
                profile: owner.profile.clone(),
            })
        }
        Entrance::SlackChannel { workspace, channel } => {
            let ws = host
                .slack
                .workspaces
                .get(workspace)
                .ok_or_else(|| unknown(owner, "slack workspace", workspace))?;
            let id = ws
                .channels
                .get(channel)
                .ok_or_else(|| unknown(owner, "slack channel", channel))?;
            Ok(Real::SlackChannel {
                team: ws.team_id.clone(),
                channel: id.clone(),
            })
        }
        Entrance::SlackDm { workspace } => {
            let ws = host
                .slack
                .workspaces
                .get(workspace)
                .ok_or_else(|| unknown(owner, "slack workspace", workspace))?;
            if !ws.dm {
                return Err(RoutesError(format!(
                    "{owner}: host has not enabled DM entrance for slack workspace {workspace} (dm: true)"
                )));
            }
            Ok(Real::SlackDm {
                team: ws.team_id.clone(),
            })
        }
        Entrance::TelegramDm { bot } => {
            let entry = host
                .telegram
                .bots
                .get(bot)
                .ok_or_else(|| unknown(owner, "telegram bot", bot))?;
            if !entry.dm {
                return Err(RoutesError(format!(
                    "{owner}: host has not enabled DM entrance for telegram bot {bot} (dm: true)"
                )));
            }
            Ok(Real::TelegramDm {
                bot_id: entry.bot_id,
            })
        }
        Entrance::TelegramGroups { bot } => {
            let entry = host
                .telegram
                .bots
                .get(bot)
                .ok_or_else(|| unknown(owner, "telegram bot", bot))?;
            if !entry.groups {
                return Err(RoutesError(format!(
                    "{owner}: host has not enabled group entrance for telegram bot {bot} (groups: true)"
                )));
            }
            Ok(Real::TelegramGroups {
                bot_id: entry.bot_id,
            })
        }
        Entrance::Http { endpoint } => {
            if !host.http.endpoints.contains_key(endpoint) {
                return Err(unknown(owner, "http endpoint", endpoint));
            }
            Ok(Real::Http {
                endpoint: endpoint.clone(),
            })
        }
        Entrance::HttpModel { endpoint, model } => {
            if !host.http.openai.contains_key(endpoint) {
                return Err(unknown(owner, "OpenAI-compatible endpoint", endpoint));
            }
            Ok(Real::HttpModel {
                endpoint: endpoint.clone(),
                model: model.clone(),
            })
        }
    }
}

fn candidate_key(trigger: &Trigger) -> String {
    match &trigger.on {
        TriggerSource::Event { event, .. } => format!("event {event}"),
        TriggerSource::Http { method, path, .. } => format!("http {method} {path}"),
        TriggerSource::Cron { .. } => "cron".into(),
    }
}

fn reserved(path: &str) -> bool {
    let first = path.trim_start_matches('/').split('/').next();
    first == Some(STATUS_SEGMENT)
}

fn collect_triggers(
    source: &Active,
) -> Result<BTreeMap<(ProfileId, BindingId), Vec<BoundTrigger>>, RoutesError> {
    let mut out: BTreeMap<(ProfileId, BindingId), Vec<BoundTrigger>> = BTreeMap::new();
    for (profile, snapshot) in source.iter() {
        for (agent_id, agent) in &snapshot.agents {
            for trigger in &agent.triggers {
                let (binding_id, http) = match &trigger.on {
                    TriggerSource::Cron { .. } => continue,
                    TriggerSource::Event { binding, event } => {
                        if !matches!(event.as_str(), "message" | "thread-reply" | "reaction") {
                            return Err(RoutesError(format!(
                                "{}/{}: unsupported event {event}",
                                profile.0, trigger.id
                            )));
                        }
                        (binding, false)
                    }
                    TriggerSource::Http { binding, path, .. } => {
                        if reserved(path) {
                            return Err(RoutesError(format!(
                                "{}/{}: path is under reserved /{STATUS_SEGMENT}",
                                profile.0, trigger.id
                            )));
                        }
                        (binding, true)
                    }
                };
                let binding = snapshot.bindings.get(binding_id).ok_or_else(|| {
                    RoutesError(format!(
                        "{}/{}: unknown binding {}",
                        profile.0, trigger.id, binding_id.0
                    ))
                })?;
                if matches!(binding.source, Adapter::Http { .. }) != http {
                    return Err(RoutesError(format!(
                        "{}/{}: trigger kind does not match binding {}",
                        profile.0, trigger.id, binding_id.0
                    )));
                }
                out.entry((profile.clone(), binding_id.clone()))
                    .or_default()
                    .push(BoundTrigger {
                        agent: agent_id.clone(),
                        trigger: trigger.clone(),
                    });
            }
        }
    }
    for ((profile, binding), list) in &mut out {
        list.sort_by(|a, b| {
            (a.trigger.order, &a.agent, &a.trigger.id).cmp(&(
                b.trigger.order,
                &b.agent,
                &b.trigger.id,
            ))
        });
        check_candidates(profile, binding, list, &source[profile].bindings[binding])?;
    }
    Ok(out)
}

fn check_candidates(
    profile: &ProfileId,
    binding: &BindingId,
    list: &[BoundTrigger],
    entry: &Binding,
) -> Result<(), RoutesError> {
    // Track `(candidate_key, order)` across the entire list so non-adjacent duplicates are caught too.
    let mut seen: BTreeMap<(String, i64), &str> = BTreeMap::new();
    for bound in list {
        let trigger = &bound.trigger;
        if let Some(first) = seen.insert((candidate_key(trigger), trigger.order), &trigger.id) {
            return Err(RoutesError(format!(
                "{}/{}: triggers {} and {} have the same order; match priority is ambiguous",
                profile.0, binding.0, first, trigger.id
            )));
        }
    }
    for private in list.iter().filter(|t| t.trigger.private) {
        let who = format!("{}/{}", profile.0, private.trigger.id);
        if !matches!(entry.source, Adapter::SlackDm { .. }) {
            return Err(RoutesError(format!(
                "{who}: private triggers are only allowed on Slack DM bindings"
            )));
        }
        if !matches!(private.trigger.target, TriggerTarget::Tool { .. }) {
            return Err(RoutesError(format!(
                "{who}: private triggers must target a tool directly"
            )));
        }
        let key = candidate_key(&private.trigger);
        if let Some(earlier) = list.iter().find(|t| {
            !t.trigger.private
                && candidate_key(&t.trigger) == key
                && t.trigger.order <= private.trigger.order
        }) {
            return Err(RoutesError(format!(
                "{who}: trigger {} can match earlier and route private input to a normal handler",
                earlier.trigger.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

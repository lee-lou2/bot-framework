//! Pure fingerprint inputs for pre-approved cron triggers and human-designated approvals.
//!
//! Activation and broker call-start compute the exact same fingerprint without inspecting secret
//! values; changing a secret's target requires renaming the secret to invalidate prior grants.

use crate::resolve::find_tool;
use bot_core::{
    canon::{canonicalize, sha256},
    grant::{GrantInputs, GrantSpec, Preapproval},
    id::*,
    json::parse,
    profile::{Destination, Snapshot, Tool, Trigger, TriggerSource, TriggerTarget},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Hashes declared secret names, selected env vars, egress, destination, and server alias.
pub fn mapping_fingerprint(tool: &Tool, env: &[(String, String)]) -> [u8; 32] {
    let destination = match &tool.destination {
        None => json!(null),
        Some(Destination::Reply) => json!("reply"),
        Some(Destination::Explicit { recipient, content }) => {
            json!({"recipient": recipient, "content": content})
        }
    };
    let material = json!({
        "v": 1,
        "server": tool.server,
        "secrets": tool.secrets.iter().map(|(env, name)| (env.clone(), name.as_str().to_owned())).collect::<std::collections::BTreeMap<_, _>>(),
        "env": env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
        "egress": tool.egress,
        "destination": destination,
    });
    Sha256::digest(material.to_string().as_bytes()).into()
}

/// Builds a `GrantSpec` for a pre-approved static tool cron trigger; returns `None` otherwise.
pub fn grant_spec(
    snapshot: &Snapshot,
    agent: &AgentId,
    trigger: &Trigger,
    scopes: &BTreeSet<String>,
    image: &str,
    env: &[(String, String)],
) -> Option<GrantSpec> {
    let pre: &Preapproval = trigger.preapproved.as_ref()?;
    let TriggerSource::Cron {
        schedule,
        beat: false,
        principal,
        ..
    } = &trigger.on
    else {
        return None;
    };
    let TriggerTarget::Tool { tool: id, args } = &trigger.target else {
        return None;
    };
    if !trigger.input.is_empty() || trigger.private || trigger.human_approval.is_some() {
        return None;
    }
    let tool = find_tool(snapshot, agent, id)?;
    if !tool.effects {
        return None;
    }
    let args_sha256 = sha256(&canonicalize(&parse(&args.0)?)?);
    let source = format!("cron:{schedule}@{};beat=false", snapshot.timezone);
    let revision = Revision(format!("{}@{image}", tool.revision.0));
    Some(GrantSpec::new(&GrantInputs {
        profile: &snapshot.id,
        agent,
        trigger: &trigger.id,
        source: &source,
        principal,
        scopes,
        tool: id,
        tool_revision: &revision,
        args_sha256,
        preapproval: pre,
        mapping: mapping_fingerprint(tool, env),
    }))
}

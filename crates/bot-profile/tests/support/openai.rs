#![allow(dead_code)]
//! Shared fixtures for OpenAI model binding tests: host with OpenAI endpoint, dual-model profile, and approver members.
use super::{Files, Temp, base, edit, host, write_profile};
use bot_core::{
    id::{BindingId, PrincipalId, ProfileId},
    profile::{ApprovalRoute, Snapshot},
};
use bot_profile::{HostCatalog, Limits, ProfileSet, Report, SlackWorkspace, load};

pub fn set(items: &[&str]) -> std::collections::BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Extends `host()` with OpenAI endpoint `openai` (subjects `build`, `ops`) and a second Slack workspace `other`.
pub fn host_openai() -> HostCatalog {
    let mut h = host();
    h.openai_endpoints
        .insert("openai".into(), set(&["build", "ops"]));
    h.slack.insert(
        "other".into(),
        SlackWorkspace {
            channels: set(&["lobby"]),
            dm: true,
        },
    );
    h
}

pub fn load_with(files: &Files, host: &HostCatalog) -> Result<ProfileSet, Report> {
    let root = Temp::new("openai");
    write_profile(root.path(), "personal", files);
    load(root.path(), host, &Limits::default())
}

pub fn codes(files: &Files) -> Vec<&'static str> {
    load_with(files, &host_openai())
        .err()
        .map_or_else(Vec::new, |r| r.codes())
}

pub fn ok(files: &Files) -> Snapshot {
    let set = load_with(files, &host_openai()).unwrap_or_else(|r| panic!("{r}"));
    (**set.profile(&ProfileId("personal".into())).unwrap()).clone()
}

/// Adds two OpenAI model bindings (`assistant -> main`, `research -> main/research`) and member `http:openai/build`.
pub fn models(approvers: &str) -> Files {
    let extra = if approvers.is_empty() {
        String::new()
    } else {
        format!(", approvers: {approvers}")
    };
    let files = edit(
        base(),
        "profile.yaml",
        "bindings:\n",
        "  - {identity: \"http:openai/build\", roles: [owner]}\nbindings:\n",
    );
    edit(
        files,
        "profile.yaml",
        "  - {id: inbox, adapter: http",
        &format!(
            "  - {{id: assistant, adapter: http, endpoint: openai, model: assistant, agent: main{extra}}}\n  - {{id: research, adapter: http, endpoint: openai, model: research, agent: main/research}}\n  - {{id: inbox, adapter: http"
        ),
    )
}

/// Adds Slack members from workspace `home` and workspace `other` as approver candidates.
pub fn with_people(files: Files) -> Files {
    edit(
        files,
        "profile.yaml",
        "bindings:\n",
        "  - {identity: \"slack:home/U1\", roles: [owner]}\n  - {identity: \"slack:other/U2\", roles: [owner]}\nbindings:\n",
    )
}

pub const LOCAL: &str = "{\"http:openai/build\": {principal: \"local:owner\"}}";
pub const SLACK: &str = "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: team}}";

pub fn route(snap: &Snapshot, binding: &str, requester: &str) -> Option<ApprovalRoute> {
    snap.approval_route(&BindingId(binding.into()), &PrincipalId(requester.into()))
}

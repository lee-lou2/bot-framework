//! Profile, entrance binding, and secret-name checks for `bot doctor`.
//! Reports secret names only (never values) and skips backends requiring external decryption (e.g., sops).

use super::{Item, State, capped, host::Checked};
use crate::{
    host::HostFile,
    load::{check_entrances, secret_store},
    serve::{Publication, seed},
    settings::Settings,
    validate::{
        all_needed_secrets, missing_model_sandbox, missing_named, unsupported_builtin_tools,
        weak_http_keys, weak_openai_keys,
    },
};
use bot_profile::{Limits, ProfileSet, Registry};
use std::sync::Arc;

const PROFILES: &str = "Profiles";
const ENTRANCES: &str = "Entrances";
const SECRETS: &str = "Secrets";

fn skipped_rest(items: &mut Vec<Item>, why: &str) {
    items.push(Item::skipped(ENTRANCES, why));
    items.push(Item::skipped(SECRETS, why));
}

pub async fn check(settings: &Settings, host: Option<&Checked>, items: &mut Vec<Item>) {
    let Some(root) = settings.profiles.as_ref() else {
        items.push(
            Item::new(State::Fail, PROFILES, "profiles root directory is not set").with([
                "Set BOT_PROFILES or `--profiles <dir>` to the parent directory of profile folders"
                    .to_owned(),
                "Run `bot init <name>` to create a new profile".to_owned(),
            ]),
        );
        skipped_rest(items, "skipped because profiles could not be loaded");
        return;
    };
    let Some(Checked {
        file: host,
        revisions,
    }) = host
    else {
        items.push(Item::skipped(
            PROFILES,
            "skipped because host config is invalid",
        ));
        skipped_rest(items, "skipped because host config is invalid");
        return;
    };
    let registry = Registry::new();
    let set = match registry.reload(root, &host.catalog, &Limits::default()) {
        Ok(set) => set,
        Err(report) => {
            let lines = report
                .to_string()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            items.push(
                Item::new(State::Fail, PROFILES, "validation failed").with(capped(lines, 30)),
            );
            skipped_rest(items, "skipped because profile validation failed");
            return;
        }
    };
    items.push(profiles(&set, host, revisions.as_ref()));
    items.push(match check_entrances(set.active().clone(), host) {
        Ok(_) => Item::new(State::Pass, ENTRANCES, "bindings match host entrances"),
        Err(error) => Item::new(State::Fail, ENTRANCES, error),
    });
    items.push(secrets(settings, host, &set).await);
}

fn profiles(
    set: &Arc<ProfileSet>,
    host: &HostFile,
    revisions: Option<&std::collections::BTreeMap<String, bot_core::id::Revision>>,
) -> Item {
    let mut problems = Vec::new();
    for snapshot in set.active().values() {
        problems.extend(missing_model_sandbox(snapshot, host));
        problems.extend(unsupported_builtin_tools(snapshot, host));
        problems.extend(seed::problems(snapshot));
    }
    // Publication requires server revisions; server failures are already reported by the host check.
    if let Some(revisions) = revisions
        && let Err(error) = Publication::of(set, host, revisions)
    {
        problems.push(error);
    }
    let lines = set
        .render()
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if problems.is_empty() {
        return Item::new(
            State::Pass,
            PROFILES,
            format!("{} profile(s) passed validation", set.active().len()),
        )
        .with(capped(lines, 30));
    }
    Item::new(
        State::Fail,
        PROFILES,
        format!("{} pre-start check failure(s)", problems.len()),
    )
    .with(capped(problems, 30))
    .with(capped(lines, 30))
}

async fn secrets(settings: &Settings, host: &HostFile, set: &Arc<ProfileSet>) -> Item {
    if let Some(program) = settings.secrets.decryptor() {
        return Item::skipped(
            SECRETS,
            &format!(
                "{program} store requires decryption and was not checked — run `bot validate` to verify"
            ),
        );
    }
    let store = match secret_store(settings) {
        Ok(store) => store,
        Err(error) => return Item::new(State::Fail, SECRETS, error),
    };
    let active = set.active();
    let mut problems = Vec::new();
    let mut total = 0;
    for (profile, names) in all_needed_secrets(active.values().map(AsRef::as_ref), host) {
        total += names.len();
        problems.extend(missing_named(&*store, &profile, names, &settings.secrets).await);
    }
    for snapshot in active.values() {
        problems.extend(weak_http_keys(&*store, snapshot, &host.gateway).await);
    }
    let snapshots: Vec<_> = active.values().cloned().collect();
    problems.extend(weak_openai_keys(&*store, &snapshots, &host.gateway).await);
    if problems.is_empty() {
        let summary = match total {
            0 => "no secrets required".to_owned(),
            n => format!("all {n} required secret(s) present (values checked, not printed)"),
        };
        return Item::new(State::Pass, SECRETS, summary);
    }
    Item::new(
        State::Fail,
        SECRETS,
        format!(
            "{} missing or invalid secret(s) (names only, values not printed)",
            problems.len()
        ),
    )
    .with(capped(problems, 30))
}

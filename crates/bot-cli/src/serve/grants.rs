//! Pre-approval grant calculation for static triggers at activation, matching `bot_broker::preapproval::grant_spec`.

use bot_core::{grant::GrantSpec, id::Revision};
use bot_profile::ProfileSet;
use bot_runtime::scheduler::schedule_set;
use std::collections::BTreeMap;

/// Computes `GrantSpec` per pre-approved trigger; rejects activation with `preapproval_unenforceable` if any input is indeterminate.
pub(super) fn grants_of(
    set: &ProfileSet,
    revisions: &BTreeMap<String, Revision>,
) -> Result<Vec<GrantSpec>, String> {
    use bot_core::grant::reason::UNENFORCEABLE;
    let mut grants = Vec::new();
    for snapshot in set.active().values() {
        let scheduled = schedule_set(snapshot).triggers;
        for (agent_id, agent) in &snapshot.agents {
            for trigger in agent.triggers.iter().filter(|t| t.preapproved.is_some()) {
                let at = format!("{}/{}/{}", snapshot.id.0, agent_id.0, trigger.id);
                let fail = |why: &str| format!("{UNENFORCEABLE}: {at} — {why}");
                let bot_core::profile::TriggerTarget::Tool { tool: id, .. } = &trigger.target
                else {
                    return Err(fail("not a fixed tool trigger"));
                };
                let tool = bot_broker::resolve::find_tool(snapshot, agent_id, id)
                    .ok_or_else(|| fail("tool not found"))?;
                let image = revisions
                    .get(&tool.server)
                    .ok_or_else(|| fail("installed server revision cannot be determined"))?;
                let env = bot_broker::resolve::selected_env(tool, agent_id, snapshot, snapshot)
                    .ok_or_else(|| fail("selected tool env values cannot be determined"))?;
                let scopes = scheduled
                    .iter()
                    .find(|s| {
                        s.trigger == trigger.id
                            && matches!(&s.target, bot_core::command::Target::Tool { agent, .. } if agent == agent_id)
                    })
                    .map(|s| s.scopes.clone())
                    .ok_or_else(|| fail("static schedule entry could not be built"))?;
                let spec = bot_broker::preapproval::grant_spec(
                    snapshot, agent_id, trigger, &scopes, &image.0, &env,
                )
                .ok_or_else(|| {
                    fail("cannot guarantee write mediation (effects tool), fixed args, and cron principal")
                })?;
                grants.push(spec);
            }
        }
    }
    Ok(grants)
}

#[cfg(test)]
pub(super) mod fixture {
    use bot_profile::{HostCatalog, Limits, ProfileSet, Registry};
    use std::{path::Path, sync::Arc};

    pub const NOTIFY: &str = "  - id: notify\n    \"on\": {cron: \"0 9 * * *\"}\n    principal: \"service:morning\"\n    target: {tool: agenda.notify, args: {}}\n    preapproved:\n      max_per_day: 3\n      targets:\n        - {operation: post, target: \"channel:reports\"}\n";

    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    pub fn load_into(registry: &Registry, root: &Path, extra_trigger: &str) -> Arc<ProfileSet> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles/personal");
        let dir = root.join("personal");
        let _ = std::fs::remove_dir_all(&dir);
        copy(&source, &dir);
        let skill = dir.join("agents/main/skills/agenda/tools.yaml");
        let mut tools = std::fs::read_to_string(&skill).unwrap();
        tools.push_str("  - id: notify\n    class: irreversible\n    effects: true\n    scopes: [calendar.read]\n    input: {type: object, properties: {}, additionalProperties: false}\n");
        std::fs::write(&skill, tools).unwrap();
        let triggers = dir.join("agents/main/triggers.yaml");
        let mut text = std::fs::read_to_string(&triggers).unwrap();
        text.push_str(extra_trigger);
        std::fs::write(&triggers, text).unwrap();
        registry
            .reload(root, &HostCatalog::demo(), &Limits::default())
            .unwrap_or_else(|report| panic!("{report}"))
    }

    pub fn load(root: &Path, extra_trigger: &str) -> Arc<ProfileSet> {
        load_into(&Registry::default(), root, extra_trigger)
    }
}

#[cfg(test)]
mod tests {
    use super::{fixture::*, *};
    use crate::serve::Publication;

    fn revisions(image: &str) -> BTreeMap<String, Revision> {
        BTreeMap::from([("demo-agenda".to_owned(), Revision(image.to_owned()))])
    }

    #[test]
    fn a_declared_trigger_gets_one_scope_bound_to_the_installed_revision() {
        let dir = std::env::temp_dir().join(format!("bot-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let set = load(&dir, NOTIFY);
        let grants = grants_of(&set, &revisions("img-1")).unwrap();
        assert_eq!(grants.len(), 1);
        let spec = &grants[0];
        assert_eq!(
            (
                spec.profile.0.as_str(),
                spec.agent.0.as_str(),
                spec.trigger.as_str(),
                spec.tool.0.as_str(),
                spec.max_per_day
            ),
            ("personal", "main", "notify", "agenda.notify", 3)
        );
        assert_eq!(
            spec.pairs,
            vec![("post".to_owned(), "channel:reports".to_owned())]
        );
        let line = spec.scope_line();
        assert!(
            line.contains("post→channel:reports") && line.contains("max 3/day"),
            "{line}"
        );
        assert_eq!(grants, grants_of(&set, &revisions("img-1")).unwrap());
        assert_ne!(
            spec.fingerprint,
            grants_of(&set, &revisions("img-2")).unwrap()[0].fingerprint
        );
        let none = grants_of(&set, &BTreeMap::new()).unwrap_err();
        assert!(
            none.starts_with("preapproval_unenforceable:") && none.contains("personal/main/notify"),
            "{none}"
        );
        let plain = std::env::temp_dir().join(format!("bot-grants-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&plain);
        assert!(
            grants_of(&load(&plain, ""), &revisions("img-1"))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn only_an_explicit_activation_keeps_the_grants_a_plain_start_drops_them_and_counts_them() {
        let dir = std::env::temp_dir().join(format!("bot-grants-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let set = load(&dir, NOTIFY);
        let grants = grants_of(&set, &revisions("img-1")).unwrap();
        let publication = || Publication {
            revision: Revision("act-1".into()),
            profiles: BTreeMap::new(),
            seeds: Vec::new(),
            schedules: BTreeMap::new(),
            grants: grants.clone(),
        };
        let (explicit, unissued) = publication().for_run(true);
        assert_eq!((explicit.grants.len(), unissued), (1, 0));
        let (plain, unissued) = publication().for_run(false);
        assert_eq!((plain.grants.len(), unissued), (0, 1));
        assert_eq!(
            plain.revision, explicit.revision,
            "revision must not change"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

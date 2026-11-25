//! 사전 승인(`preapproved`)을 선언한 정적 트리거의 승인 범위 계산. 활성화(`Publication::of`)가 쓰고, 호출 시작(브로커)은 같은 함수
//! (`bot_broker::preapproval::grant_spec`)로 다시 계산해 저장소의 승인 지문과 대조한다.

use bot_core::{grant::GrantSpec, id::Revision};
use bot_profile::ProfileSet;
use bot_runtime::scheduler::schedule_set;
use std::collections::BTreeMap;

/// 사전 승인을 선언한 트리거마다 승인 범위(지문 재료)를 계산한다. 설치한 서버 판본·선택 환경·주체 스코프를 확정할 수 없으면 활성화를 거절한다
/// (`preapproval_unenforceable`). 호출 시작(브로커)이 같은 함수(`bot_broker::preapproval::grant_spec`)로 다시 계산해 저장소 판정과 대조한다.
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
                    return Err(fail("고정 도구 트리거가 아니에요"));
                };
                let tool = bot_broker::resolve::find_tool(snapshot, agent_id, id)
                    .ok_or_else(|| fail("도구를 찾을 수 없어요"))?;
                let image = revisions
                    .get(&tool.server)
                    .ok_or_else(|| fail("도구 서버의 설치 판본을 확정할 수 없어요"))?;
                let env = bot_broker::resolve::selected_env(tool, agent_id, snapshot, snapshot)
                    .ok_or_else(|| fail("도구가 고른 환경 값을 확정할 수 없어요"))?;
                let scopes = scheduled
                    .iter()
                    .find(|s| {
                        s.trigger == trigger.id
                            && matches!(&s.target, bot_core::command::Target::Tool { agent, .. } if agent == agent_id)
                    })
                    .map(|s| s.scopes.clone())
                    .ok_or_else(|| fail("정적 예약 정의를 만들 수 없어요"))?;
                let spec = bot_broker::preapproval::grant_spec(
                    snapshot, agent_id, trigger, &scopes, &image.0, &env,
                )
                .ok_or_else(|| {
                    fail("쓰기 경유(effects 도구)·고정 인자·cron 주체를 보증할 수 없어요")
                })?;
                grants.push(spec);
            }
        }
    }
    Ok(grants)
}

#[cfg(test)]
pub(super) mod fixture {
    //! 사전 승인 시험이 함께 쓰는 프로필: 예시 `personal`에 `effects` 도구(`agenda.notify`)와 사전 승인한 cron 트리거를 더해 읽는다.
    use bot_profile::{HostCatalog, Limits, ProfileSet, Registry};
    use std::{path::Path, sync::Arc};

    /// `preapproved`를 선언한 cron 트리거 `notify`(허용 쌍 하나, 하루 3회).
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

    /// 예시 프로필을 `root/personal`에 복사하고 도구와 `extra_trigger`를 더한 뒤 `registry`로 읽는다.
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
            line.contains("post→channel:reports") && line.contains("하루 최대 3회"),
            "{line}"
        );
        // 같은 입력은 같은 지문이고 설치 판본이 다르면 다른 지문이다(재활성화 때 새 ID)
        assert_eq!(grants, grants_of(&set, &revisions("img-1")).unwrap());
        assert_ne!(
            spec.fingerprint,
            grants_of(&set, &revisions("img-2")).unwrap()[0].fingerprint
        );
        // 설치 판본을 확정할 수 없으면 활성화를 거절한다(preapproval_unenforceable)
        let none = grants_of(&set, &BTreeMap::new()).unwrap_err();
        assert!(
            none.starts_with("preapproval_unenforceable:") && none.contains("personal/main/notify"),
            "{none}"
        );
        // 선언이 없는 프로필은 승인이 없다
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
        assert_eq!(plain.revision, explicit.revision, "판본은 그대로다");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

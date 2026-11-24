//! 공통 인가 규칙을 브로커 호출 경계에서 적용한다.
pub use bot_core::scope::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{run_context, snapshot};
    use bot_core::{
        id::*,
        profile::{Scopes, Snapshot},
        state::RunContext,
    };
    use std::collections::BTreeMap;

    fn set(items: &[&str]) -> Scopes {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn world(profile: &[&str], owner: &[&str]) -> std::sync::Arc<Snapshot> {
        snapshot("personal", "r1")
            .role("owner", owner)
            .member("local:owner", &["owner"])
            .policy_scopes(profile)
            .agent("main/research")
            .build()
    }

    fn ctx(scopes: &[&str]) -> RunContext {
        run_context("personal", "r1", 1, "local:owner", "main", scopes)
    }

    #[test]
    fn allows_only_when_every_set_covers_every_required_scope() {
        let snap = world(
            &["run", "calendar.read", "calendar.delete"],
            &["run", "calendar.read", "calendar.delete"],
        );
        let requester = ctx(&["run", "calendar.read"]);
        assert_eq!(
            authorize(&set(&["calendar.read"]), &requester, &snap, &snap),
            Ok(())
        );
        assert_eq!(
            authorize(&set(&["calendar.delete"]), &requester, &snap, &snap),
            Err(Denial::Missing(vec!["calendar.delete".into()]))
        );
    }

    #[test]
    fn a_tool_needing_two_scopes_needs_both() {
        let snap = world(&["a", "b"], &["a", "b"]);
        assert_eq!(
            authorize(&set(&["a", "b"]), &ctx(&["a"]), &snap, &snap),
            Err(Denial::Missing(vec!["b".into()]))
        );
        assert_eq!(
            authorize(&set(&["a", "b"]), &ctx(&["a", "b"]), &snap, &snap),
            Ok(())
        );
    }

    #[test]
    fn profile_ceiling_beats_a_generous_requester() {
        let snap = world(&["calendar.read"], &["calendar.read", "calendar.delete"]);
        let requester = ctx(&["calendar.read", "calendar.delete"]);
        assert!(matches!(
            authorize(&set(&["calendar.delete"]), &requester, &snap, &snap),
            Err(Denial::Missing(_))
        ));
    }

    #[test]
    fn a_role_revoked_after_acceptance_stops_working_at_once() {
        // 접수 때는 calendar.delete가 있었지만 지금 판본의 역할에는 없다.
        let snap = world(&["calendar.read", "calendar.delete"], &["calendar.read"]);
        let requester = ctx(&["calendar.read", "calendar.delete"]);
        assert!(authorize(&set(&["calendar.delete"]), &requester, &snap, &snap).is_err());
        assert_eq!(
            authorize(&set(&["calendar.read"]), &requester, &snap, &snap),
            Ok(())
        );
    }

    #[test]
    fn a_principal_who_left_the_profile_is_denied() {
        let snap = snapshot("personal", "r1")
            .role("owner", &["a"])
            .policy_scopes(&["a"])
            .build();
        assert_eq!(
            authorize(&set(&["a"]), &ctx(&["a"]), &snap, &snap),
            Err(Denial::Member)
        );
    }

    #[test]
    fn an_agent_missing_from_the_current_revision_is_denied() {
        let snap = world(&["a"], &["a"]);
        let child = run_context("personal", "r1", 1, "local:owner", "main/gone", &["a"]);
        assert_eq!(
            authorize(&set(&["a"]), &child, &snap, &snap),
            Err(Denial::Agent)
        );
    }

    #[test]
    fn agent_ceiling_limits_a_child_and_inherits_the_parent_limit() {
        let snap = snapshot("personal", "r1")
            .role("owner", &["run", "a", "b", "c", "d"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["run", "a", "b", "c", "d"])
            .agent_limit("main", &["run", "a", "b", "c"])
            .agent_limit("main/research", &["b", "c", "d"])
            .build();
        let all = ["run", "a", "b", "c", "d"];
        let child = run_context("personal", "r1", 1, "local:owner", "main/research", &all);
        let root = run_context("personal", "r1", 1, "local:owner", "main", &all);
        // main/research: {b, c, d} ∩ {run, a, b, c} = {b, c}. d는 부모 상한 밖이라 열리지 않는다.
        assert_eq!(
            agent_ceiling(&snap.policy.agents, &AgentId("main/research".into())),
            Some(set(&["b", "c"]))
        );
        assert_eq!(authorize(&set(&["b"]), &child, &snap, &snap), Ok(()));
        assert!(authorize(&set(&["d"]), &child, &snap, &snap).is_err());
        assert!(authorize(&set(&["a"]), &child, &snap, &snap).is_err());
        assert_eq!(authorize(&set(&["a"]), &root, &snap, &snap), Ok(()));
    }

    #[test]
    fn the_agent_ceilings_of_both_revisions_apply() {
        let ours = ["a", "b"];
        let build = |revision: &str, main: &[&str]| {
            snapshot("personal", revision)
                .role("owner", &ours)
                .member("local:owner", &["owner"])
                .policy_scopes(&ours)
                .agent_scopes("main", main)
                .build()
        };
        let (wide, narrow) = (build("r1", &["a", "b"]), build("r2", &["a"]));
        let requester = ctx(&["a", "b"]);
        // 고정 판본은 b를 허용하지만 지금 판본의 에이전트 상한이 좁다(역도 마찬가지다)
        assert_eq!(authorize(&set(&["a"]), &requester, &narrow, &wide), Ok(()));
        assert!(authorize(&set(&["b"]), &requester, &narrow, &wide).is_err());
        assert!(authorize(&set(&["b"]), &requester, &wide, &narrow).is_err());
        assert_eq!(authorize(&set(&["b"]), &requester, &wide, &wide), Ok(()));
        // 고정 판본에 에이전트가 없으면 거절한다
        let empty = snapshot("personal", "r3").build();
        let missing = run_context("personal", "r1", 1, "local:owner", "main/gone", &["a"]);
        assert_eq!(
            authorize(&set(&["a"]), &missing, &wide, &empty),
            Err(Denial::Agent)
        );
    }

    #[test]
    fn unlisted_descendants_inherit_the_nearest_listed_ancestor() {
        let agents: BTreeMap<AgentId, Scopes> = [(AgentId("main".into()), set(&["a"]))]
            .into_iter()
            .collect();
        assert_eq!(
            agent_ceiling(&agents, &AgentId("main/x/y".into())),
            Some(set(&["a"]))
        );
        assert_eq!(agent_ceiling(&agents, &AgentId("other".into())), None);
    }

    #[test]
    fn a_tool_without_declared_scopes_is_never_callable() {
        let snap = world(&["a"], &["a"]);
        assert_eq!(
            authorize(&set(&[]), &ctx(&["a"]), &snap, &snap),
            Err(Denial::Undeclared)
        );
    }

    #[test]
    fn denial_codes_are_stable() {
        assert_eq!(Denial::Undeclared.code(), "scope_undeclared");
        assert_eq!(Denial::Member.code(), "member_unknown");
        assert_eq!(Denial::Agent.code(), "agent_unknown");
        assert_eq!(Denial::Missing(vec![]).code(), "scope_missing");
    }
}

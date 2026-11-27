//! Broker acceptance tests for super-admin live skill/MCP management tools (`skill.*`, `mcp.*`).
use super::*;
use bot_core::{
    builtin::{MCP_ADD_TOOL, SKILL_CREATE_TOOL, SKILL_INSPECT_TOOL},
    ledger::ApprovalState,
    manage::{ManageCall, ManageError, ManageOutcome, ProfileManager},
    port::Reply,
};
use std::sync::Mutex;

const MANAGE_SCOPES: [&str; 10] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
    SKILL_INSPECT_TOOL,
    SKILL_CREATE_TOOL,
    MCP_ADD_TOOL,
];

struct FakeManager {
    calls: Mutex<Vec<ManageCall>>,
    next: Mutex<Option<Result<ManageOutcome, ManageError>>>,
    on_apply: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeManager {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next: Mutex::new(None),
            on_apply: Mutex::new(None),
        }
    }

    fn set_next(&self, result: Result<ManageOutcome, ManageError>) {
        *self.next.lock().unwrap() = Some(result);
    }

    fn set_on_apply(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_apply.lock().unwrap() = Some(Box::new(f));
    }

    fn calls(&self) -> Vec<ManageCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl ProfileManager for FakeManager {
    fn execute(&self, call: ManageCall) -> Reply<'_, Result<ManageOutcome, ManageError>> {
        self.calls.lock().unwrap().push(call);
        if let Some(hook) = self.on_apply.lock().unwrap().take() {
            hook();
        }
        let outcome = self.next.lock().unwrap().take().unwrap_or_else(|| {
            Ok(ManageOutcome {
                payload: JsonPayload(r#"{"ok":true}"#.into()),
                activated: None,
            })
        });
        Box::pin(std::future::ready(Ok(outcome)))
    }
}

fn manage_world(
    super_admins: &[&str],
    approve: &[&str],
) -> (World, Arc<FakeManager>, BrokerService) {
    let w = World::new();
    let mut b = base("r1")
        .role("owner", &MANAGE_SCOPES)
        .policy_scopes(&MANAGE_SCOPES)
        .approve(approve);
    for admin in super_admins {
        b = b.super_admin(admin);
    }
    w.publish(b.build());
    w.ledger.add_run(run_context(
        "personal",
        "r-admin",
        1,
        "local:owner",
        "main",
        &MANAGE_SCOPES,
    ));
    let manager = Arc::new(FakeManager::new());
    let broker = w.reboot().with_manager(manager.clone());
    (w, manager, broker)
}

#[test]
fn non_super_admin_is_rejected_without_touching_manager() {
    let (w, manager, broker) = manage_world(&["local:alice"], &[]);
    for (call, tool, args) in [
        ("c1", SKILL_INSPECT_TOOL, "{}"),
        (
            "c2",
            SKILL_CREATE_TOOL,
            r#"{"name":"demo","description":"desc","body":"body"}"#,
        ),
        (
            "c3",
            MCP_ADD_TOOL,
            r#"{"alias":"weather","transport":"http","url":"https://mcp.example/mcp"}"#,
        ),
    ] {
        let res = w.call_on(&broker, "r-admin", 1, call, tool, args);
        assert!(matches!(res, Err(Error::Denied)), "{tool}: {res:?}");
    }
    assert!(manager.calls().is_empty(), "manager must never be called");
    assert_eq!(
        w.audit
            .verdicts()
            .iter()
            .filter(|(_, v)| v == "not_super_admin")
            .count(),
        3
    );
}

#[test]
fn super_admin_can_inspect_and_create_with_live_revision_verification() {
    let (w, manager, broker) = manage_world(&["local:owner"], &[]);

    manager.set_next(Ok(ManageOutcome {
        payload: JsonPayload(r#"{"ok":true,"agents":["main"]}"#.into()),
        activated: None,
    }));
    let inspected = done(w.call_on(&broker, "r-admin", 1, "c1", SKILL_INSPECT_TOOL, "{}"));
    assert!(inspected.contains(r#""agents":["main"]"#), "{inspected}");
    assert!(
        w.audit
            .verdicts()
            .iter()
            .any(|(a, v)| a == "manage.inspect" && v == "ok")
    );

    let catalog = w.catalog.clone();
    let ledger = w.ledger.clone();
    manager.set_on_apply(move || {
        let next = base("r2")
            .role("owner", &MANAGE_SCOPES)
            .policy_scopes(&MANAGE_SCOPES)
            .super_admin("local:owner")
            .build();
        ledger.set_active_revision("personal", "r2");
        catalog.publish(next);
    });
    manager.set_next(Ok(ManageOutcome {
        payload: JsonPayload(r#"{"ok":true,"revision":"r2"}"#.into()),
        activated: Some(Revision("r2".into())),
    }));
    let created = done(w.call_on(
        &broker,
        "r-admin",
        1,
        "c2",
        SKILL_CREATE_TOOL,
        r#"{"name":"review","description":"code review","body":"checklist"}"#,
    ));
    assert!(created.contains(r#""revision":"r2""#), "{created}");
    assert_eq!(manager.calls().len(), 2);
    assert!(
        w.audit
            .verdicts()
            .iter()
            .any(|(a, v)| a == "manage.apply" && v == "ok")
    );
}

#[test]
fn revoked_super_admin_in_current_snapshot_is_denied_even_if_pinned_allowed() {
    let (w, manager, broker) = manage_world(&["local:owner"], &[]);
    let next = base("r2")
        .role("owner", &MANAGE_SCOPES)
        .policy_scopes(&MANAGE_SCOPES)
        .super_admin("local:alice")
        .build();
    w.publish(next);

    let res = w.call_on(&broker, "r-admin", 1, "c1", SKILL_INSPECT_TOOL, "{}");
    assert!(matches!(res, Err(Error::Denied)), "{res:?}");
    assert!(manager.calls().is_empty());
    assert!(
        w.audit
            .verdicts()
            .iter()
            .any(|(_, v)| v == "not_super_admin")
    );
}

#[test]
fn approval_card_redacts_mcp_secret_values_and_runs_with_original_args_after_approval() {
    let (w, manager, broker) = manage_world(&["local:owner"], &[MCP_ADD_TOOL]);
    let raw_secret_a = "synthetic-raw-bearer-value-999";
    let raw_secret_b = "synthetic-raw-env-value-888";
    let args = format!(
        r#"{{"alias":"weather","transport":"http","url":"https://mcp.example/mcp","auth_secret":"weather-key","secret_value":"{raw_secret_a}","secret_values":{{"extra":"{raw_secret_b}"}}}}"#
    );

    let nonce = awaiting(w.call_on(&broker, "r-admin", 1, "c-mcp", MCP_ADD_TOOL, &args));
    assert!(manager.calls().is_empty(), "must not run before approval");

    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.state, ApprovalState::Pending);
    assert!(
        !stored.card.0.contains(raw_secret_a) && !stored.card.0.contains(raw_secret_b),
        "raw secret leaked into approval card: {}",
        stored.card.0
    );
    assert!(
        stored.card.0.contains("[REDACTED]"),
        "redaction marker missing from approval card: {}",
        stored.card.0
    );

    assert!(matches!(
        block_on(broker.decide(nonce.clone(), PrincipalId("local:alice".into()), true)),
        Err(Error::Denied)
    ));
    block_on(broker.decide(nonce.clone(), PrincipalId("local:owner".into()), true)).unwrap();
    let reply = done(w.call_on(&broker, "r-admin", 1, "c-mcp", MCP_ADD_TOOL, &args));
    assert!(reply.contains(r#""ok":true"#), "{reply}");
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    let calls = manager.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].args.0.contains(raw_secret_a) && calls[0].args.0.contains(raw_secret_b),
        "execution after approval must receive original args"
    );
}

#[test]
fn missing_manager_fails_closed_with_unavailable() {
    let (w, _manager, _with_mgr) = manage_world(&["local:owner"], &[]);
    let bare = w.bare();
    let res = w.call_on(&bare, "r-admin", 1, "c1", SKILL_INSPECT_TOOL, "{}");
    assert!(matches!(res, Err(Error::Unavailable)), "{res:?}");
}

#[test]
fn rate_limited_manage_call_returns_rate_limited() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    w.publish(
        base("r1")
            .role("owner", &MANAGE_SCOPES)
            .policy_scopes(&MANAGE_SCOPES)
            .super_admin("local:owner")
            .build(),
    );
    w.ledger.add_run(run_context(
        "personal",
        "r-admin",
        1,
        "local:owner",
        "main",
        &MANAGE_SCOPES,
    ));
    let manager = Arc::new(FakeManager::new());
    let broker = w.reboot().with_manager(manager);
    done(w.call_on(&broker, "r-admin", 1, "c1", SKILL_INSPECT_TOOL, "{}"));
    let limited = w.call_on(&broker, "r-admin", 1, "c2", SKILL_INSPECT_TOOL, "{}");
    assert!(matches!(limited, Err(Error::RateLimited)), "{limited:?}");
}

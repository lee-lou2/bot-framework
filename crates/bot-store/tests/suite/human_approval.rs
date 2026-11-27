//! Designated-human event approval (PG + real broker): verifies authenticated event evidence
//! capture, single-use consumption, replay convergence, bypass rejection, and independent guards.

use crate::support::{human::*, *};
use bot_core::{
    command::*,
    grant::{Authority, Granted, reason},
    id::*,
    ledger::StartCall,
    port::{Broker, DelegationStore, Error},
    state::RunContext,
};
use std::sync::Arc;

const MEMBERS: &[&str] = &[APPROVER, OTHER, "service:morning"];

async fn rig_for(approvers: &[&str], members: &[&str]) -> Option<Rig> {
    rig(world(approvers, members)).await
}

#[tokio::test]
async fn the_designated_person_approves_once_with_no_card_and_the_evidence_is_recorded_with_the_start()
 {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    let evidence: (String, String, String, String) =
        sqlx::query_as("select kind, team, event_id, actor from bot_run_evidence")
            .fetch_one(r.db.store.pool())
            .await
            .unwrap();
    assert_eq!(
        evidence,
        (
            "reaction".into(),
            "T1".into(),
            "Ev1".into(),
            APPROVER.into()
        )
    );
    let result = r.broker.call(Rig::call_of(&ctx)).await.unwrap();
    assert!(matches!(result, CallResult::Done(_)));
    assert_eq!(r.runner.jobs().len(), 1, "tool runs once");
    assert_eq!(
        r.count("select count(*) from bot_approvals").await,
        0,
        "no approval card created"
    );
    let used: (String, String, String, String, String, Vec<u8>) = sqlx::query_as(
        "select team, event_id, actor, tool, call, args_sha256 from bot_evidence_uses",
    )
    .fetch_one(r.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        (
            used.0.as_str(),
            used.1.as_str(),
            used.2.as_str(),
            used.3.as_str()
        ),
        ("T1", "Ev1", APPROVER, "reports.notify")
    );
    assert_eq!(used.4, format!("{}:tool", ctx.attempt.run.0));
    assert_eq!(used.5.len(), 32);
    assert_eq!(
        r.verdicts("call.start").await,
        vec!["started:human;evidence=T1/Ev1".to_owned()]
    );
    assert!(matches!(
        r.broker.call(Rig::call_of(&ctx)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 1);
}

#[tokio::test]
async fn a_redelivered_event_and_a_reused_evidence_cannot_approve_twice() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let first = r.react("req-1", APPROVER, "Ev1").await;
    let again = runs(&r.db)
        .accept_with(
            event_accept("req-1", APPROVER),
            AcceptExtras {
                source: Some(source("Ev1", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await
        .unwrap();
    assert_eq!(again, first.attempt.run);
    assert_eq!(r.count("select count(*) from bot_run_evidence").await, 1);
    assert!(matches!(
        r.broker.call(Rig::call_of(&first)).await.unwrap(),
        CallResult::Done(_)
    ));
    // Re-receiving the same Slack event under a different request key converges on the first run.
    let converged = runs(&r.db)
        .accept_with(
            event_accept("req-2", APPROVER),
            AcceptExtras {
                source: Some(source("Ev1", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await
        .unwrap();
    assert_eq!(converged, first.attempt.run);
    // Even if a legacy row lacked `slack_key` and created a second run, reused evidence is denied.
    r.db.sql("update bot_requests set slack_key = null").await;
    let second = r.react("req-2", APPROVER, "Ev1").await;
    let denied = r.broker.call(Rig::call_of(&second)).await;
    assert!(
        matches!(denied, Err(Error::Denied)),
        "evidence already used"
    );
    assert_eq!(r.runner.jobs().len(), 1, "side effect executes only once");
    assert!(
        r.verdicts("call.deny")
            .await
            .contains(&reason::HUMAN_MISMATCH.to_owned())
    );
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 1);
}

#[tokio::test]
async fn other_people_other_candidates_models_and_revoked_members_get_no_bypass() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    // Undesignated member falls back to the standard approval card path.
    let other = r.react("req-o", OTHER, "Ev-o").await;
    let waiting = r.broker.call(Rig::call_of(&other)).await.unwrap();
    assert!(matches!(waiting, CallResult::Awaiting(_)));
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
    assert!(
        r.verdicts("call.note")
            .await
            .contains(&reason::HUMAN_MISMATCH.to_owned())
    );
    // Evidence binds strictly to the first candidate's direct call (`{run}:tool`).
    let mine = r.react("req-m", APPROVER, "Ev-m").await;
    let mut other_call = Rig::call_of(&mine);
    other_call.call = CallId(format!("{}:tool:1", mine.attempt.run.0));
    assert!(matches!(
        r.broker.call(other_call).await.unwrap(),
        CallResult::Awaiting(_)
    ));
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);

    let child =
        r.db.store
            .delegate(Delegate {
                checked_revision: Revision("r1".into()),
                parent: mine.attempt.clone(),
                step: "review".into(),
                child: AgentId("main".into()),
                instruction: "go".into(),
                scopes: scopes(&["reports.post"]),
            })
            .await;
    if let Ok(child) = child {
        let next = runs(&r.db).claim().await.unwrap().unwrap();
        assert_eq!(next.attempt.run, child);
        let result = r
            .broker
            .call(ToolCall {
                call: CallId("c1".into()),
                ..Rig::call_of(&next)
            })
            .await;
        assert!(
            !matches!(result, Ok(CallResult::Done(_))),
            "delegated child must not bypass approval"
        );
    }
    assert!(r.runner.jobs().is_empty());

    let start = |ctx: &RunContext, call: &str| StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Irreversible,
        tool_revision: Revision("t1@reports-img".into()),
        attempt: ctx.attempt.clone(),
        call: CallId(call.into()),
        approval: None,
        private_recall: false,
        owner: format!("{}.{}", ctx.attempt.run.0, call),
    };
    let wrong_actor = Authority::Human {
        actor: PrincipalId(OTHER.into()),
    };
    let fresh = r.react("req-s", APPROVER, "Ev-s").await;
    let call_id = format!("{}:tool", fresh.attempt.run.0);
    calls(&r.db)
        .begin(
            fresh.attempt.clone(),
            CallId(call_id.clone()),
            ToolId(TOOL.into()),
            sha_args(),
        )
        .await
        .unwrap();
    assert_eq!(
        calls(&r.db)
            .start_with(start(&fresh, &call_id), wrong_actor)
            .await
            .unwrap(),
        Granted::Refused(reason::HUMAN_MISMATCH),
        "mismatched evidence actor must be refused"
    );
    assert_eq!(
        r.count("select count(*) from bot_evidence_uses").await,
        0,
        "refusal must not consume evidence"
    );
}

fn sha_args() -> [u8; 32] {
    let value = bot_core::json::parse(ARGS).unwrap();
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&value).unwrap())
}

#[tokio::test]
async fn a_member_revoked_after_the_reaction_cannot_approve() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    let revoked = world(&[APPROVER], &[OTHER, "service:morning"]);
    r.catalog.publish(revoked);
    let result = r.broker.call(Rig::call_of(&ctx)).await;
    assert!(
        !matches!(result, Ok(CallResult::Done(_))),
        "revoked member must not execute the tool"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);

    let undesignated = world(&[OTHER], MEMBERS);
    r.catalog.publish(undesignated);
    let ctx = r.react("req-2", APPROVER, "Ev2").await;
    let result = r.broker.call(Rig::call_of(&ctx)).await.unwrap();
    assert!(
        matches!(result, CallResult::Awaiting(_)),
        "removed designation must fall back to approval card"
    );
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn evidence_is_only_made_from_an_authenticated_human_direct_tool_event_with_its_candidate() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let evidence_rows = || async { r.count("select count(*) from bot_run_evidence").await };
    r.accept("req-a", APPROVER, AcceptExtras::default()).await;
    r.accept(
        "req-b",
        APPROVER,
        AcceptExtras {
            source: Some(source("Ev-b", "1.1")),
            chain: None,
        },
    )
    .await;
    let service = r
        .accept(
            "req-c",
            "service:morning",
            AcceptExtras {
                source: Some(source("Ev-c", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await;
    assert!(service.1.is_some());

    let mut model = event_accept("req-d", APPROVER);
    model.target = Target::Agent {
        agent: AgentId("main".into()),
        instruction: "react".into(),
    };
    let refused = runs(&r.db)
        .accept_with(
            model,
            AcceptExtras {
                source: Some(source("Ev-d", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await;
    assert!(
        matches!(refused, Err(Error::Invalid)),
        "agent target cannot carry a direct-tool pass chain: {refused:?}"
    );
    let mut mismatched = chain("confirm");
    mismatched.candidates[0].target = Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId("other.tool".into()),
        args: JsonPayload("{}".into()),
    };
    let refused = runs(&r.db)
        .accept_with(
            event_accept("req-e", APPROVER),
            AcceptExtras {
                source: Some(source("Ev-e", "1.1")),
                chain: Some(mismatched),
            },
        )
        .await;
    assert!(
        matches!(refused, Err(Error::Invalid)),
        "first candidate must match accept target: {refused:?}"
    );
    assert_eq!(evidence_rows().await, 0);
    assert_eq!(
        r.count("select count(*) from bot_requests where request in ('req-d', 'req-e')")
            .await,
        0,
        "rejected accepts must not insert request rows"
    );
    r.accept(
        "req-f",
        APPROVER,
        AcceptExtras {
            source: Some(source("Ev-f", "1.1")),
            chain: Some(chain("confirm")),
        },
    )
    .await;
    assert_eq!(evidence_rows().await, 1);
}

#[tokio::test]
async fn sensitive_recall_confirmation_is_not_replaced_by_a_human_reaction() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let mut external = world(&[APPROVER], MEMBERS);
    {
        let main = Arc::make_mut(&mut external)
            .agents
            .get_mut(&AgentId("main".into()))
            .unwrap();
        for skill in main.skills.values_mut() {
            for tool in skill.tools.values_mut() {
                tool.external = true;
            }
        }
    }
    r.catalog.publish(external);
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    r.db.sql("update bot_runs set private_memory_read = true")
        .await;
    let result = r.broker.call(Rig::call_of(&ctx)).await.unwrap();
    assert!(
        matches!(result, CallResult::Awaiting(_)),
        "external tool after private recall still requires explicit card confirmation"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
}

#[tokio::test]
async fn the_evidence_only_starts_the_first_candidates_direct_call_and_both_snapshots_must_designate()
 {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    let other = CallId(format!("{}:tool:1", ctx.attempt.run.0));
    calls(&r.db)
        .begin(
            ctx.attempt.clone(),
            other.clone(),
            ToolId(TOOL.into()),
            sha_args(),
        )
        .await
        .unwrap();
    let start = StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Irreversible,
        tool_revision: Revision("t1@reports-img".into()),
        attempt: ctx.attempt.clone(),
        call: other,
        approval: None,
        private_recall: false,
        owner: "o".into(),
    };
    let human = Authority::Human {
        actor: PrincipalId(APPROVER.into()),
    };
    assert_eq!(
        calls(&r.db).start_with(start, human).await.unwrap(),
        Granted::Refused(reason::HUMAN_MISMATCH)
    );
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);

    // Both the pinned run revision (r1) and the current active revision (r2) must designate the approver.
    let mut now = world(&[APPROVER], MEMBERS);
    Arc::make_mut(&mut now).revision = Revision("r2".into());
    r.catalog.publish(world(&[OTHER], MEMBERS));
    r.catalog.publish(now);
    let current = bot_core::profile::ActiveRevisions::current(&r.db.store)
        .await
        .unwrap();
    bot_core::profile::ActiveRevisions::replace(
        &r.db.store,
        current,
        Revision("set-2".into()),
        std::collections::BTreeMap::from([
            (ProfileId("personal".into()), Revision("r2".into())),
            (ProfileId("work".into()), Revision("r1".into())),
        ]),
        vec![],
        Default::default(),
    )
    .await
    .unwrap();
    let result = r.broker.call(Rig::call_of(&ctx)).await;
    assert!(
        !matches!(result, Ok(CallResult::Done(_))),
        "actor not designated in pinned snapshot must not execute"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
}

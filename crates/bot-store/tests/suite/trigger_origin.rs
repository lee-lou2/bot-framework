use crate::support::*;
use bot_core::{
    command::{Delegate, Occurrence, Target},
    id::{AgentId, Revision},
    ledger::{ApprovalState, Decision},
    port::{DelegationStore, Error, ScheduleStore},
    profile::ActiveRevisions,
    state::{RunOrigin, TriggerRef},
};
use std::{
    collections::BTreeMap,
    time::{Duration, UNIX_EPOCH},
};

#[tokio::test]
async fn a_scheduled_child_loads_the_exact_trigger_of_its_root() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut trigger = trigger_spec("morning", "* * * * *", false);
    let owner = AgentId("main".into());
    trigger.target = Target::Agent {
        agent: owner.clone(),
        instruction: "Review the agenda".into(),
    };
    let target = trigger.target.clone();
    db.publish_schedule(schedule_set("personal", "r1", vec![trigger]))
        .await
        .unwrap();
    db.backdate_schedule().await;
    let time = db.now_ms().await;
    let scheduled = Occurrence {
        profile: profile_id(),
        agent: owner.clone(),
        trigger: "morning".into(),
        scheduled_at: UNIX_EPOCH + Duration::from_millis(time as u64),
    };
    let request = occurrence_accept("personal", "morning", time, target, &["calendar.read"]);
    let root = db
        .store
        .accept_occurrence(scheduled, request)
        .await
        .unwrap();
    let parent = runs(&db).claim().await.unwrap().unwrap();
    let origin = Some(TriggerRef {
        agent: owner,
        id: "morning".into(),
    });
    assert_eq!(parent.attempt.run, root);
    assert_eq!(parent.trigger, origin);
    assert_eq!(parent.origin, RunOrigin::Schedule);

    let child = AgentId("main/research".into());
    let child_run = db
        .store
        .delegate(Delegate {
            checked_revision: bot_core::id::Revision("r1".into()),
            parent: parent.attempt,
            step: "agenda".into(),
            child: child.clone(),
            instruction: "Summarize the agenda".into(),
            scopes: scopes(&["calendar.read"]),
        })
        .await
        .unwrap();
    let delegated = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(delegated.attempt.run, child_run);
    assert_eq!(delegated.agent, child);
    assert_eq!(delegated.trigger, origin);
    assert_eq!(delegated.origin, RunOrigin::Schedule);
}

#[tokio::test]
async fn a_replaced_profile_leaves_an_approved_call_unconsumed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "request").await;
    assert!(run.trigger.is_none());
    begin_call(&db, &run.attempt, "search").await;
    let approval = binding(&run.attempt, "search", "nonce");
    let open = open_approval(approval.clone());
    let reasons = open.reasons.clone();
    calls(&db).open_approval(open).await.unwrap();
    calls(&db)
        .decide(Decision {
            nonce: approval.nonce.clone(),
            actor: approval.approver.clone(),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let mut start = start_call(
        &resumed.attempt,
        "search",
        Some((approval.clone(), reasons)),
    );
    start.risk = approval.risk;
    start.tool_revision = approval.tool_revision;
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([(profile_id(), Revision("r2".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(calls(&db).start(start).await, Err(Error::Stale)));
    assert_eq!(
        calls(&db).approval(approval.nonce).await.unwrap().state,
        ApprovalState::Approved
    );
}

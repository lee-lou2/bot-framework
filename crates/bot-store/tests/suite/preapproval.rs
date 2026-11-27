//! Pre-approval grants: issuance/revocation on activation, run binding on accept, cardless broker start, and non-inheritance.

use crate::support::{preapproval::*, *};
use bot_core::{
    command::*,
    grant::{Authority, Granted, reason},
    id::*,
    port::{Broker, DelegationStore, Error},
};

const PAIRS: &[(&str, &str)] = &[("post", "channel:reports"), ("post", "channel:alerts")];

async fn active_grants(db: &TestDb) -> Vec<String> {
    sqlx::query_scalar("select id from bot_grants where state = 'active' order by id")
        .fetch_all(db.store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn activation_issues_keeps_and_revokes_and_a_revoked_id_never_returns() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let first = r.grant.0.clone();
    assert_eq!(active_grants(&r.db).await, vec![first.clone()]);
    assert!(
        publish(&r.db, &r.snap, vec![r.spec.clone()])
            .await
            .is_empty()
    );
    assert_eq!(active_grants(&r.db).await, vec![first.clone()]);
    let changed = world(4, PAIRS);
    let spec2 = spec_of(&changed, IMAGE);
    let issued = publish(&r.db, &changed, vec![spec2]).await;
    let second = issued[0].0.0.clone();
    assert_ne!(second, first);
    assert_eq!(active_grants(&r.db).await, vec![second.clone()]);
    let old: String =
        r.db.scalar(&format!(
            "select state from bot_grants where id = '{first}'"
        ))
        .await;
    assert_eq!(old, "revoked");
    // Reverting to the earlier config allocates a fresh grant ID rather than reviving the revoked one.
    let back = publish(&r.db, &r.snap, vec![r.spec.clone()]).await;
    let third = back[0].0.0.clone();
    assert!(
        third != first && third != second,
        "reverting must issue a new grant ID"
    );
    assert_eq!(active_grants(&r.db).await, vec![third.clone()]);
    assert!(publish(&r.db, &r.snap, vec![]).await.is_empty());
    assert!(active_grants(&r.db).await.is_empty());
    assert_eq!(r.audits("preapproval.issue").await, 3);
    assert_eq!(r.audits("preapproval.revoke").await, 3);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_trigger_pause")
            .await,
        1
    );
}

#[tokio::test]
async fn a_preapproved_run_goes_through_the_broker_without_a_card_and_leaves_the_grant_on_every_receipt()
 {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let ctx = r.next_run().await;
    let call = CallId(format!("{}:tool", ctx.attempt.run.0));
    let bound: String = r.db.scalar("select grant_id from bot_run_grants").await;
    assert_eq!(bound, r.grant.0, "run must bind to the grant on accept");
    assert!(matches!(
        r.broker.call(tool_call(&ctx, &call)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 1, "tool must execute once");
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_approvals")
            .await,
        0,
        "no approval card should be opened"
    );
    let recorded: String = r.db.scalar("select grant_id from bot_calls").await;
    assert_eq!(recorded, r.grant.0, "call receipt must record grant ID");
    assert_eq!(
        r.verdicts("call.start").await,
        vec![format!("started:preapproved;grant={}", r.grant.0)]
    );
    assert!(matches!(
        r.broker.call(tool_call(&ctx, &call)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn model_delegation_other_triggers_and_fake_bodies_do_not_inherit_the_grant() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let mut forged = tool_accept("personal", "forged", TOOL, ARGS);
    forged.scopes = scopes(&["run", "reports.post"]);
    forged.approver = Some(PrincipalId(OWNER.into()));
    runs(&r.db).accept(forged).await.unwrap();
    let forged = runs(&r.db).claim().await.unwrap().unwrap();
    let forged_call = CallId(format!("{}:tool", forged.attempt.run.0));
    let result = r
        .broker
        .call(tool_call(&forged, &forged_call))
        .await
        .unwrap();
    assert!(
        matches!(result, CallResult::Awaiting(_)),
        "ordinary request must require an approval card"
    );
    assert!(r.runner.jobs().is_empty());
    let parent = r.next_run().await;
    let child =
        r.db.store
            .delegate(Delegate {
                checked_revision: Revision("r1".into()),
                parent: parent.attempt.clone(),
                step: "review".into(),
                child: AgentId("main/research".into()),
                instruction: "go".into(),
                scopes: scopes(&["reports.post"]),
            })
            .await
            .unwrap();
    let next = runs(&r.db).claim().await.unwrap().unwrap();
    assert_eq!(next.attempt.run, child);
    let denied = r.broker.call(tool_call(&next, &CallId("c1".into()))).await;
    assert!(
        matches!(denied, Err(Error::Denied)),
        "delegated child without approver must be denied"
    );
    assert!(r.runner.jobs().is_empty(), "nothing should execute");
    let loose = tool_accept("personal", "loose", TOOL, ARGS);
    runs(&r.db).accept(loose).await.unwrap();
    let loose = runs(&r.db).claim().await.unwrap().unwrap();
    let call = r.begin(&loose, ARGS).await;
    assert_eq!(
        r.start(&loose, &call).await.unwrap(),
        Granted::Refused(reason::STALE),
        "run without bound grant must be refused"
    );
    let ctx = r.next_run().await;
    let call = r.begin(&ctx, ARGS).await;
    let wrong = Authority::Grant {
        fingerprint: [9; 32],
    };
    assert_eq!(
        calls(&r.db)
            .start_with(start_of(&ctx, &call), wrong)
            .await
            .unwrap(),
        Granted::Refused(reason::STALE),
        "mismatched fingerprint must be refused"
    );
    let mut other_revision = start_of(&ctx, &call);
    other_revision.tool_revision = Revision("t1@other-image".into());
    let authority = Authority::Grant {
        fingerprint: r.spec.fingerprint,
    };
    assert_eq!(
        calls(&r.db)
            .start_with(other_revision, authority)
            .await
            .unwrap(),
        Granted::Refused(reason::STALE),
        "mismatched tool install revision must be refused"
    );
    let plain = calls(&r.db).start(start_of(&ctx, &call)).await;
    assert!(matches!(plain, Err(Error::Denied)));
    let state: String =
        r.db.scalar(&format!(
            "select state from bot_calls where run = '{}'",
            ctx.attempt.run.0
        ))
        .await;
    assert_eq!(state, "denied");
    assert_eq!(r.runner.jobs().len(), 0);
    assert!(
        r.verdicts("call.deny")
            .await
            .contains(&reason::STALE.to_owned())
    );
}

#[tokio::test]
async fn a_changed_install_or_republished_scope_leaves_old_runs_refused() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let ctx = r.next_run().await;
    let call = CallId(format!("{}:tool", ctx.attempt.run.0));
    r.runner.set_image("reports", Some("reports-img-2"));
    let swapped = r.broker.call(tool_call(&ctx, &call)).await;
    assert!(matches!(swapped, Err(Error::Denied)));
    assert!(r.runner.jobs().is_empty());
    assert!(
        r.verdicts("call.deny")
            .await
            .contains(&reason::STALE.to_owned())
    );
    // Restoring the matching image restores the grant since its ID was never revoked.
    r.runner.set_image("reports", Some(IMAGE));
    let again = r.next_run().await;
    let again_call = CallId(format!("{}:tool", again.attempt.run.0));
    assert!(matches!(
        r.broker.call(tool_call(&again, &again_call)).await.unwrap(),
        CallResult::Done(_)
    ));
    let waiting = r.next_run().await;
    let waiting_call = CallId(format!("{}:tool", waiting.attempt.run.0));
    let changed = world(9, PAIRS);
    publish(&r.db, &changed, vec![spec_of(&changed, IMAGE)]).await;
    r.catalog.publish(changed);
    let late = r.broker.call(tool_call(&waiting, &waiting_call)).await;
    assert!(
        matches!(late, Err(Error::Denied)),
        "run bound to superseded grant must be denied"
    );
    let grants = r.verdicts("call.deny").await;
    assert!(grants.iter().filter(|v| *v == reason::STALE).count() >= 2);
}

#[tokio::test]
async fn a_pause_or_republish_between_acceptance_and_start_keeps_the_old_run_from_starting() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let ctx = r.next_run().await;
    let call = r.begin(&ctx, ARGS).await;
    r.pause(&r.grant.0, 0, true).await.unwrap();
    assert_eq!(
        r.start(&ctx, &call).await.unwrap(),
        Granted::Refused(reason::PAUSED)
    );
    r.pause(&r.grant.0, 1, false).await.unwrap();
    assert_eq!(
        r.start(&ctx, &call).await.unwrap(),
        Granted::Refused(reason::STALE)
    );
    let state: String = r.db.scalar("select state from bot_calls").await;
    assert_eq!(
        state, "open",
        "refused start leaves the call open for the broker to close"
    );
    assert_eq!(r.audits("call.start").await, 0);
    // Only the direct `{run}:tool` call may use the grant.
    let fresh = r.next_run().await;
    let other = CallId(format!("{}:other", fresh.attempt.run.0));
    calls(&r.db)
        .begin(fresh.attempt.clone(), other.clone(), tool(TOOL), sha(ARGS))
        .await
        .unwrap();
    assert_eq!(
        r.start(&fresh, &other).await.unwrap(),
        Granted::Refused(reason::STALE),
        "non-direct call ID must be refused"
    );
    let direct = r.begin(&fresh, ARGS).await;
    assert_eq!(r.start(&fresh, &direct).await.unwrap(), Granted::Started);
    let changed = world(9, PAIRS);
    publish(&r.db, &changed, vec![spec_of(&changed, IMAGE)]).await;
    let late = r
        .effect(&fresh, &direct, "s1", "post", "channel:reports")
        .await;
    assert!(
        matches!(late, Err(Error::Stale)),
        "effect under revoked grant must be stale"
    );
    assert_eq!(r.used().await, 0);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_effects").await,
        0
    );
    // Daily quota persists per (trigger, day) across grant re-issuance.
    let next = r.next_run().await;
    let next_call = r.begin(&next, ARGS).await;
    let authority = Authority::Grant {
        fingerprint: spec_of(&changed, IMAGE).fingerprint,
    };
    assert_eq!(
        calls(&r.db)
            .start_with(start_of(&next, &next_call), authority)
            .await
            .unwrap(),
        Granted::Started
    );
    assert!(
        r.effect(&next, &next_call, "s2", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    assert_eq!(r.used().await, 1);
    let changed_again = world(10, PAIRS);
    publish(&r.db, &changed_again, vec![spec_of(&changed_again, IMAGE)]).await;
    assert_eq!(
        r.used().await,
        1,
        "re-activation must not reset daily grant usage"
    );
}

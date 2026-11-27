//! Pre-approval pause and resume (PG): blocks new permits, preserves in-flight result recording,
//! enforces epoch fencing and CAS transitions, handles concurrent races, and exposes admin listing.

use crate::support::{preapproval::*, *};
use bot_core::{
    admin::{AdminAct, AdminRead},
    command::*,
    grant::{GrantCas, reason},
    id::*,
    port::Error,
    state::*,
};

const PAIRS: &[(&str, &str)] = &[("post", "channel:reports"), ("notify", "user:ops")];

fn confirmed(effect: &EffectId, receipt: &str) -> EffectStatus {
    EffectStatus {
        effect: effect.clone(),
        state: EffectState::Confirmed,
        resolution: None,
        receipt: Some(receipt.into()),
    }
}

#[tokio::test]
async fn pause_stops_new_permits_not_results_and_a_resume_cannot_revive_an_old_call() {
    let Some(r) = rig(10, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let first = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(first.send);

    // Pause blocks new permits while allowing replays and recording results of already-permitted steps.
    let paused = pause(&r, &r.grant.0, 0, true).await.unwrap();
    assert!(paused.row.paused && paused.row.epoch == 1 && !paused.repeated);
    let blocked = r.effect(&ctx, &call, "s2", "post", "channel:reports").await;
    assert!(matches!(blocked, Err(Error::Denied)));
    assert_eq!(
        r.verdicts("effect.refuse").await,
        vec![reason::PAUSED.to_owned()]
    );
    let replay = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(!replay.send, "replay is not a new write");
    effects(&r.db)
        .record(
            ctx.attempt.clone(),
            call.clone(),
            confirmed(&first.status.effect, "msg-1"),
        )
        .await
        .unwrap();
    assert_eq!(r.used().await, 1);

    // Occurrences while paused advance the cursor and skip without creating runs.
    let cursor_before =
        r.db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await;
    assert!(matches!(r.occur(1).await, Err(Error::Stale)));
    let cursor_after =
        r.db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await;
    assert!(
        cursor_after > cursor_before,
        "skipped occurrence advances cursor"
    );
    assert_eq!(r.db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        r.verdicts("schedule.occurrence")
            .await
            .last()
            .map(String::as_str),
        Some("skip:trigger_paused")
    );
    let held_back = r.next_run_unchecked().await;
    assert!(held_back.is_none());

    // Resuming bumps epoch so pre-pause calls remain stale and cannot acquire new permits.
    let resumed = pause(&r, &r.grant.0, 1, false).await.unwrap();
    assert!(!resumed.row.paused && resumed.row.epoch == 2);
    let stale = r.effect(&ctx, &call, "s3", "post", "channel:reports").await;
    assert!(
        matches!(stale, Err(Error::Stale)),
        "pre-pause call must fail as Stale, not Denied"
    );
    assert!(
        r.verdicts("effect.refuse")
            .await
            .contains(&reason::STALE.to_owned())
    );
    assert_eq!(r.used().await, 1);

    let (fresh, fresh_call) = r.started().await;
    let bound: i64 =
        r.db.scalar(&format!(
            "select epoch from bot_run_grants where run = '{}'",
            fresh.attempt.run.0
        ))
        .await;
    assert_eq!(bound, 2);
    assert!(
        r.effect(&fresh, &fresh_call, "s4", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    assert_eq!(r.used().await, 2, "pause/resume preserves daily usage");
}

#[tokio::test]
async fn pause_commands_are_compare_and_set_and_listing_shows_usage() {
    let Some(r) = rig(5, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    assert!(
        r.effect(&ctx, &call, "s1", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    let rows = AdminRead::preapprovals(&r.db.store, &operator())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        (
            row.profile.as_str(),
            row.agent.as_str(),
            row.trigger.as_str(),
            row.grant_id.as_str()
        ),
        ("personal", "main", "notify", r.grant.0.as_str())
    );
    assert_eq!(
        (row.epoch, row.paused, row.used_today, row.max_per_day),
        (0, false, 1, 5)
    );
    let outsider = bot_core::admin::Viewer {
        principal: PrincipalId("local:guest".into()),
        operator: false,
    };
    assert!(matches!(
        AdminRead::preapprovals(&r.db.store, &outsider).await,
        Err(Error::Denied)
    ));

    let g = r.grant.0.clone();
    let one = pause(&r, &g, 0, true).await.unwrap();
    assert!(!one.repeated && one.row.epoch == 1 && one.row.paused);
    let replay = pause(&r, &g, 0, true).await.unwrap();
    assert!(replay.repeated && replay.row == one.row);
    let same_epoch = pause(&r, &g, 1, true).await.unwrap();
    assert!(same_epoch.repeated, "already paused at current epoch");
    assert_eq!(r.audits("preapproval.pause").await, 1);

    // After resume and re-pause, replaying the first pause fails ABA check as stale.
    assert!(!pause(&r, &g, 1, false).await.unwrap().repeated);
    assert!(!pause(&r, &g, 2, true).await.unwrap().repeated);
    assert!(matches!(pause(&r, &g, 0, true).await, Err(Error::Stale)));
    assert!(matches!(pause(&r, &g, 1, false).await, Err(Error::Stale)));
    assert!(
        matches!(pause(&r, "g-other", 3, false).await, Err(Error::Stale)),
        "mismatched grant ID"
    );
    assert!(matches!(
        pause(&r, &g, -1, false).await,
        Err(Error::Stale) | Err(Error::Invalid)
    ));
    assert!(matches!(
        pause(&r, &g, i64::MAX, false).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        (
            r.audits("preapproval.pause").await,
            r.audits("preapproval.resume").await
        ),
        (2, 1)
    );

    let missing = AdminAct::set_pause(
        &r.db.store,
        PrincipalId(OWNER.into()),
        bot_core::grant::TriggerKey {
            trigger: "ghost".into(),
            ..key()
        },
        GrantCas {
            grant_id: g.clone(),
            epoch: 0,
        },
        true,
    )
    .await;
    assert!(matches!(missing, Err(Error::Missing)));
    let changed = world(6, PAIRS);
    let issued = publish(&r.db, &changed, vec![spec_of(&changed, IMAGE)]).await;
    let fresh = issued[0].0.0.clone();
    assert!(
        matches!(pause(&r, &g, 3, false).await, Err(Error::Stale)),
        "superseded grant ID must be stale"
    );
    let rows = AdminRead::preapprovals(&r.db.store, &operator())
        .await
        .unwrap();
    assert_eq!(
        (
            rows.len(),
            rows[0].grant_id.as_str(),
            rows[0].paused,
            rows[0].epoch
        ),
        (1, fresh.as_str(), true, 3),
        "pause state persists across grant replacement"
    );
}

#[tokio::test]
async fn a_pause_racing_new_permits_leaves_no_permit_after_its_commit() {
    let Some(r) = rig(50, &[("post", "channel:reports")]).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let peer = r.db.peer("pauser").await;
    let g = r.grant.0.clone();
    let writers = async {
        let mut sent = 0;
        for i in 0..20 {
            let step = format!("w{i}");
            let args = format!(
                r#"{{"operation":"post","target":"channel:reports","params":{{"i":{i}}}}}"#
            );
            match bot_core::port::EffectJournal::request(
                &r.db.store,
                effect_req(&ctx, &call, &step, &args),
            )
            .await
            {
                Ok(permit) if permit.send => sent += 1,
                _ => {}
            }
        }
        sent
    };
    let stopper = async {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        AdminAct::set_pause(
            &peer,
            PrincipalId(OWNER.into()),
            key(),
            GrantCas {
                grant_id: g.clone(),
                epoch: 0,
            },
            true,
        )
        .await
        .unwrap()
    };
    let (sent, paused) = tokio::join!(writers, stopper);
    assert!(paused.row.paused);
    assert_eq!(r.used().await, sent);
    assert_eq!(r.audits("effect.start").await, sent);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_effects").await,
        sent
    );
    let after = r
        .effect(&ctx, &call, "late", "post", "channel:reports")
        .await;
    assert!(matches!(after, Err(Error::Denied)));
}

#[tokio::test]
async fn occurrences_scheduled_in_or_before_a_pause_stay_skipped_after_the_resume() {
    let Some(r) = rig(10, PAIRS).await else {
        return;
    };
    let before = r.db.now_ms().await - 1;
    r.pause(&r.grant.0, 0, true).await.unwrap();
    let during = r.db.now_ms().await + 2;
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    r.pause(&r.grant.0, 1, false).await.unwrap();

    // Catch-up after resume skips occurrences scheduled before or during the pause window.
    for at in [before, during] {
        assert!(matches!(r.occur_at(at).await, Err(Error::Stale)), "{at}");
    }
    assert_eq!(r.db.scalar::<i64>("select count(*) from bot_runs").await, 0);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_run_grants")
            .await,
        0
    );
    let skipped = r.verdicts("schedule.occurrence").await;
    assert_eq!(skipped, vec!["skip:trigger_paused".to_owned(); 2]);
    let cursor =
        r.db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await;
    assert_eq!(cursor, during, "cursor advances past skipped occurrences");

    let fresh = r.db.now_ms().await;
    r.occur_at(fresh).await.unwrap();
    let bound: i64 = r.db.scalar("select epoch from bot_run_grants").await;
    assert_eq!(bound, 2);
    assert_eq!(r.verdicts("schedule.occurrence").await.len(), 3);
}

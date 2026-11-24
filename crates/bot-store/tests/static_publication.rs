mod support;

use bot_core::{
    command::TriggerSpec,
    id::{ProfileId, Revision},
    port::{Error, ScheduleStore},
    profile::ActiveRevisions,
};
use std::collections::BTreeMap;
use support::{TestDb, trigger_spec};

fn revision(value: &str) -> Revision {
    Revision(value.to_owned())
}

async fn publish(
    db: &TestDb,
    expected: Option<Revision>,
    version: &str,
    definitions: &[(&str, Vec<TriggerSpec>)],
) -> Result<(), Error> {
    let profiles = definitions
        .iter()
        .map(|(profile, _)| (ProfileId((*profile).into()), revision(version)))
        .collect();
    let schedules: BTreeMap<_, _> = definitions
        .iter()
        .map(|(profile, specs)| (ProfileId((*profile).into()), specs.clone()))
        .collect();
    db.store
        .replace(expected, revision(version), profiles, vec![], schedules)
        .await
}

#[tokio::test]
async fn removing_profiles_closes_their_segments_together_and_stale_publication_cannot_reopen_them()
{
    let Some(db) = TestDb::bare().await else {
        return;
    };
    let trigger = trigger_spec("morning", "0 9 * * *", false);
    let both = [("personal", vec![trigger.clone()]), ("work", vec![trigger])];
    publish(&db, None, "one", &both).await.unwrap();
    db.backdate_schedule().await;
    publish(&db, Some(revision("one")), "two", &[("personal", vec![])])
        .await
        .unwrap();
    assert!(matches!(
        publish(&db, Some(revision("one")), "old", &both).await,
        Err(Error::Conflict)
    ));
    assert_eq!(db.store.current().await.unwrap(), Some(revision("two")));
    let rows = db.store.snapshot().await.unwrap().triggers;
    assert_eq!(
        rows.len(),
        2,
        "both old cursors must still be available for catch-up"
    );
    let boundary = rows[0].segments[0].until.unwrap();
    for row in rows {
        assert_eq!(row.segments.len(), 1);
        assert_eq!(row.segments[0].until, Some(boundary));
        assert!(row.cursor < boundary);
    }
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_schedule_segments where until_ms is null")
            .await,
        0
    );
}

#[tokio::test]
async fn a_failed_schedule_write_rolls_back_the_revision_all_segments_and_the_audit() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    let trigger = trigger_spec("morning", "0 9 * * *", false);
    publish(&db, None, "one", &[("personal", vec![trigger.clone()])])
        .await
        .unwrap();
    let count = db.scalar::<i64>("select count(*) from bot_audit").await;
    // Force the second profile's INSERT to fail after the first profile has changed in this transaction.
    db.sql("alter table bot_schedule_segments add check (trigger <> 'blocked')")
        .await;
    let broken = [
        ("personal", vec![trigger]),
        ("work", vec![trigger_spec("blocked", "0 9 * * *", false)]),
    ];
    assert!(
        publish(&db, Some(revision("one")), "two", &broken)
            .await
            .is_err()
    );
    assert_eq!(db.store.current().await.unwrap(), Some(revision("one")));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_active_profiles")
            .await,
        1
    );
    let rows = db.store.snapshot().await.unwrap().triggers;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].segments.len(), 1);
    assert_eq!(rows[0].segments[0].revision, revision("one"));
    assert!(rows[0].segments[0].until.is_none());
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        count
    );
}

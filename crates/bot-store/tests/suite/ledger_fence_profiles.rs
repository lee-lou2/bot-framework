//! Run IDs are scoped per profile: stale claims return `Stale`, foreign-profile runs return `Denied`, and absent runs return `Missing` regardless of scan order.

const OWNER: &str = "local:owner";

use crate::support::*;
use bot_core::{id::*, port::Error};

/// Clones `work`'s run row into `personal` and touches `work`'s row so `personal` appears first under both index and sequential scans.
async fn same_id_in_the_other_profile(db: &TestDb, run: &str) {
    let columns: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns
         where table_schema = current_schema() and table_name = 'bot_runs' order by ordinal_position",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    let list = columns.join(", ");
    let values = columns
        .iter()
        .map(|c| {
            if c == "profile" {
                "'personal'"
            } else {
                c.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    db.sql(&format!(
        "insert into bot_runs ({list}) select {values} from bot_runs where profile = 'work' and id = '{run}'"
    ))
    .await;
    db.sql(&format!(
        "update bot_runs set updated_ms = updated_ms where profile = 'work' and id = '{run}'"
    ))
    .await;
    for plan in [
        "set enable_indexscan = off; set enable_bitmapscan = off",
        "set enable_seqscan = off",
    ] {
        let mut conn = db.raw().await;
        for statement in plan.split(';') {
            sqlx::query(statement.trim())
                .execute(&mut conn)
                .await
                .unwrap();
        }
        let first: String = sqlx::query_scalar(&format!(
            "select profile from bot_runs where id = '{run}' limit 1"
        ))
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            first, "personal",
            "{plan}: foreign profile row must scan first"
        );
    }
}

#[tokio::test]
async fn a_stale_claim_stays_stale_when_another_profile_has_the_same_run_id() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    runs(&db)
        .accept(accept_as(
            "work",
            "r1",
            OWNER,
            bot_core::state::ConversationKind::Direct,
            "c1",
            "hi",
        ))
        .await
        .unwrap();
    let run = runs(&db).claim().await.unwrap().expect("expected claim");
    assert_eq!(run.attempt.profile.0, "work");
    same_id_in_the_other_profile(&db, &run.attempt.run.0).await;
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_runs where id = '{}'",
            run.attempt.run.0
        ))
        .await,
        2,
        "both profiles should hold the same run ID"
    );
    db.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where profile = 'work' and id = '{}'",
        run.attempt.run.0
    ))
    .await;
    let taken = runs(&db).claim().await.unwrap().expect("expected takeover");
    assert_eq!(taken.attempt.profile.0, "work");
    assert_ne!(taken.attempt.claim, run.attempt.claim);
    for _ in 0..5 {
        let got = calls(&db)
            .begin(
                run.attempt.clone(),
                call("c1"),
                tool("agenda.list"),
                [1; 32],
            )
            .await;
        assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    }
    let foreign = Attempt {
        profile: ProfileId("other".into()),
        ..run.attempt.clone()
    };
    let got = calls(&db)
        .begin(foreign, call("c1"), tool("agenda.list"), [1; 32])
        .await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    let ghost = Attempt {
        run: RunId("ghost".into()),
        ..run.attempt.clone()
    };
    let got = calls(&db)
        .begin(ghost, call("c1"), tool("agenda.list"), [1; 32])
        .await;
    assert!(matches!(got, Err(Error::Missing)), "{got:?}");
}

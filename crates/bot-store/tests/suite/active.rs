use crate::support::*;
use bot_core::{id::*, port::*, profile::ActiveRevisions};
use std::collections::BTreeMap;

fn set(pairs: &[(&str, &str)]) -> BTreeMap<ProfileId, Revision> {
    pairs
        .iter()
        .map(|(p, r)| (ProfileId((*p).into()), Revision((*r).into())))
        .collect()
}

fn rev(s: &str) -> Revision {
    Revision(s.into())
}

#[tokio::test]
async fn replaces_the_active_set_only_when_the_expectation_matches() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    db.store
        .replace(
            None,
            rev("set-1"),
            set(&[("personal", "r1"), ("work", "w1")]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .replace(
                None,
                rev("set-2"),
                set(&[("personal", "r2")]),
                Vec::new(),
                Default::default()
            )
            .await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        db.store
            .replace(
                Some(rev("other")),
                rev("set-2"),
                set(&[("personal", "r2")]),
                Vec::new(),
                Default::default()
            )
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_active_profiles")
            .await,
        2
    );

    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<String>("select revision from bot_active_profiles where profile = 'personal'")
            .await,
        "r2"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_active_profiles")
            .await,
        1,
        "omitted profile is removed from active set"
    );
    // Every affected profile (including removed ones) gets an audit entry.
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where profile = 'work' and action = 'active.replace'"
        )
        .await,
        2
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_audit where profile = 'personal' and action = 'active.replace'").await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_replacements_let_exactly_one_win() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    let mut tasks = Vec::new();
    for n in 0..6 {
        let store = db.peer(&format!("host-{n}")).await;
        tasks.push(tokio::spawn(async move {
            store
                .replace(
                    None,
                    rev(&format!("set-{n}")),
                    set(&[("personal", "r1")]),
                    Vec::new(),
                    Default::default(),
                )
                .await
                .is_ok()
        }));
    }
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(wins, 1);
}

#[tokio::test]
async fn current_reports_the_last_published_set() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    assert_eq!(
        db.store.current().await.unwrap(),
        None,
        "none before first publish"
    );
    db.store
        .replace(
            None,
            rev("set-1"),
            set(&[("personal", "r1")]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(db.store.current().await.unwrap(), Some(rev("set-1")));
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(db.store.current().await.unwrap(), Some(rev("set-2")));
}

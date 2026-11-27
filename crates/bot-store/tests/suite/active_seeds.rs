//! Atomic active-set publishing (PG): CAS, profile memory seeds, and audit commit in one transaction.

use crate::support::*;
use bot_core::{command::MemorySeed, id::*, port::*, profile::ActiveRevisions, state::Visibility};
use bot_store::{SeedError, check_seed};
use std::collections::BTreeMap;

fn set(items: &[(&str, &str)]) -> BTreeMap<ProfileId, Revision> {
    items
        .iter()
        .map(|(p, r)| (ProfileId((*p).into()), Revision((*r).into())))
        .collect()
}

fn rev(s: &str) -> Revision {
    Revision(s.into())
}

fn seed(profile: &str, path: &str, hash: u8, text: &str) -> MemorySeed {
    MemorySeed {
        profile: ProfileId(profile.into()),
        agent: AgentId("main".into()),
        path: path.into(),
        sha256: [hash; 32],
        owner: PrincipalId(OWNER.into()),
        visibility: Visibility::Private,
        text: text.into(),
    }
}

async fn memories_in(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_memories").await
}

#[tokio::test]
async fn the_set_the_seeds_and_the_audit_commit_together() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    db.store
        .replace(
            None,
            rev("set-1"),
            set(&[("personal", "r1"), ("work", "w1")]),
            vec![
                seed("personal", "memory/a.md", 1, "first seed"),
                seed("work", "memory/b.md", 2, "second seed"),
            ],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(memories_in(&db).await, 2);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.seed'")
            .await,
        2,
        "one audit entry per inserted seed"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'active.replace'")
            .await,
        2
    );
    assert_eq!(db.store.current().await.unwrap(), Some(rev("set-1")));
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);

    // Republishing an identical seed reuses the existing row.
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2"), ("work", "w1")]),
            vec![
                seed("personal", "memory/a.md", 1, "first seed"),
                seed("personal", "memory/c.md", 3, "new seed"),
            ],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(memories_in(&db).await, 3, "only the new seed is inserted");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.seed'")
            .await,
        3
    );
}

#[tokio::test]
async fn a_bad_seed_or_a_seed_for_a_profile_outside_the_set_rolls_everything_back() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    db.store
        .replace(
            None,
            rev("set-1"),
            set(&[("personal", "r1")]),
            vec![seed("personal", "memory/a.md", 1, "existing seed")],
            Default::default(),
        )
        .await
        .unwrap();
    let audits = db.scalar::<i64>("select count(*) from bot_audit").await;
    for bad in [
        seed("personal", "memory/x.md", 9, "   "),
        seed("personal", "memory/x.md", 9, &"€".repeat(9000)),
        seed("work", "memory/x.md", 9, "profile outside set"),
        seed("personal", "", 9, "missing path"),
    ] {
        let result = db
            .store
            .replace(
                Some(rev("set-1")),
                rev("set-2"),
                set(&[("personal", "r2")]),
                vec![seed("personal", "memory/ok.md", 5, "valid seed"), bad],
                Default::default(),
            )
            .await;
        assert!(matches!(result, Err(Error::Invalid)));
        assert_eq!(
            db.store.current().await.unwrap(),
            Some(rev("set-1")),
            "active revision unchanged"
        );
        assert_eq!(memories_in(&db).await, 1, "valid sibling seed rolled back");
        assert_eq!(
            db.scalar::<i64>("select count(*) from bot_audit").await,
            audits
        );
    }
}

#[tokio::test]
async fn an_error_in_the_middle_of_the_transaction_leaves_the_previous_set_seeds_and_audit_untouched()
 {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    db.store
        .replace(
            None,
            rev("set-1"),
            set(&[("personal", "r1")]),
            vec![seed("personal", "memory/a.md", 1, "existing seed")],
            Default::default(),
        )
        .await
        .unwrap();
    let audits = db.scalar::<i64>("select count(*) from bot_audit").await;
    // Passes pre-validation (`check_seed`) but triggers a mid-transaction DB constraint error.
    db.sql("alter table bot_memories add constraint reject_poison check (body <> 'poison seed')")
        .await;
    let result = db
        .store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            vec![
                seed("personal", "memory/first.md", 2, "first seed (valid)"),
                seed("personal", "memory/second.md", 3, "poison seed"),
            ],
            Default::default(),
        )
        .await;
    assert!(result.is_err(), "constraint violation must fail publish");
    assert_eq!(
        db.store.current().await.unwrap(),
        Some(rev("set-1")),
        "previous revision unchanged"
    );
    assert_eq!(
        db.scalar::<String>("select revision from bot_active_profiles where profile = 'personal'")
            .await,
        "r1"
    );
    assert_eq!(
        memories_in(&db).await,
        1,
        "earlier seed in the same tx must roll back"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
    // Dropping the constraint lets the same publish succeed cleanly.
    db.sql("alter table bot_memories drop constraint reject_poison")
        .await;
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            vec![seed("personal", "memory/second.md", 3, "poison seed")],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(memories_in(&db).await, 2);
}

#[tokio::test]
async fn a_losing_cas_inserts_no_seeds() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    db.store
        .replace(
            None,
            rev("set-1"),
            set(&[("personal", "r1")]),
            vec![],
            Default::default(),
        )
        .await
        .unwrap();
    let result = db
        .store
        .replace(
            Some(rev("stale")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            vec![seed("personal", "memory/a.md", 1, "loser seed")],
            Default::default(),
        )
        .await;
    assert!(matches!(result, Err(Error::Conflict)));
    assert_eq!(memories_in(&db).await, 0);
    assert_eq!(db.store.current().await.unwrap(), Some(rev("set-1")));
}

#[tokio::test]
async fn racing_publishers_leave_exactly_the_winners_seeds() {
    let Some(db) = TestDb::bare().await else {
        return;
    };
    let mut tasks = Vec::new();
    for n in 0..6u8 {
        let peer = db.peer(&format!("pub-{n}")).await;
        tasks.push(tokio::spawn(async move {
            peer.replace(
                None,
                Revision(format!("set-{n}")),
                set(&[("personal", &format!("r{n}"))]),
                vec![seed(
                    "personal",
                    &format!("memory/{n}.md"),
                    n,
                    &format!("seed {n}"),
                )],
                Default::default(),
            )
            .await
        }));
    }
    let mut winners = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => winners += 1,
            Err(error) => assert!(matches!(error, Error::Conflict)),
        }
    }
    assert_eq!(winners, 1, "exactly one publisher wins CAS");
    assert_eq!(memories_in(&db).await, 1, "only the winner's seed exists");
    let current = db.store.current().await.unwrap().unwrap();
    let path = db
        .scalar::<String>("select seed_path from bot_memories")
        .await;
    assert_eq!(
        path,
        format!("memory/{}.md", current.0.trim_start_matches("set-"))
    );
}

#[test]
fn the_shared_check_names_the_reason_without_quoting_the_body() {
    let ok = seed("personal", "memory/a.md", 1, "valid");
    assert_eq!(check_seed(&ok), Ok(()));
    let secret = "secret body SENTINEL";
    let mut bad_field = seed("personal", "memory/a.md", 1, secret);
    bad_field.owner = PrincipalId(String::new());
    assert_eq!(check_seed(&bad_field), Err(SeedError::BadField("owner")));
    assert_eq!(
        check_seed(&seed("personal", "p", 1, "  \n")),
        Err(SeedError::EmptyText)
    );
    let long = "€".repeat(9000);
    let too_long = check_seed(&seed("personal", "p", 1, &long));
    assert_eq!(
        too_long,
        Err(SeedError::TextTooLong {
            bytes: long.len(),
            max: 8 * 1024
        })
    );
    assert_eq!(
        check_seed(&seed("personal", "p", 1, "a\0b")),
        Err(SeedError::TextHasNul)
    );
    for error in [
        check_seed(&bad_field).unwrap_err(),
        too_long.unwrap_err(),
        SeedError::EmptyText,
        SeedError::TextHasNul,
    ] {
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("SENTINEL") && !shown.contains("€€"),
            "{shown}"
        );
    }
}

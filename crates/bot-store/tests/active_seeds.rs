//! 활성 집합 게시(PG): CAS·소속 프로필의 기억 시드·감사가 한 트랜잭션이다. 어느 하나라도 실패하면 이전 판본과 기억이 그대로다.

mod support;

use bot_core::{command::MemorySeed, id::*, port::*, profile::ActiveRevisions, state::Visibility};
use bot_store::{SeedError, check_seed};
use std::collections::BTreeMap;
use support::*;

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
                seed("personal", "memory/a.md", 1, "첫째 시드"),
                seed("work", "memory/b.md", 2, "둘째 시드"),
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
        "새로 들어간 시드마다 감사 한 줄(본문 없이)"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'active.replace'")
            .await,
        2
    );
    assert_eq!(db.store.current().await.unwrap(), Some(rev("set-1")));
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);

    // 같은 시드를 다시 게시해도 다시 만들지 않는다(재사용)
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2"), ("work", "w1")]),
            vec![
                seed("personal", "memory/a.md", 1, "첫째 시드"),
                seed("personal", "memory/c.md", 3, "새 시드"),
            ],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(memories_in(&db).await, 3, "새 시드만 늘었다");
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
            vec![seed("personal", "memory/a.md", 1, "기존 시드")],
            Default::default(),
        )
        .await
        .unwrap();
    let audits = db.scalar::<i64>("select count(*) from bot_audit").await;
    for bad in [
        seed("personal", "memory/x.md", 9, "   "),
        seed("personal", "memory/x.md", 9, &"가".repeat(9000)),
        seed("work", "memory/x.md", 9, "집합 밖 프로필"),
        seed("personal", "", 9, "경로 없음"),
    ] {
        let result = db
            .store
            .replace(
                Some(rev("set-1")),
                rev("set-2"),
                set(&[("personal", "r2")]),
                vec![seed("personal", "memory/ok.md", 5, "정상 시드"), bad],
                Default::default(),
            )
            .await;
        assert!(matches!(result, Err(Error::Invalid)));
        assert_eq!(
            db.store.current().await.unwrap(),
            Some(rev("set-1")),
            "판본은 그대로"
        );
        assert_eq!(memories_in(&db).await, 1, "정상 시드도 들어가지 않았다");
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
            vec![seed("personal", "memory/a.md", 1, "기존 시드")],
            Default::default(),
        )
        .await
        .unwrap();
    let audits = db.scalar::<i64>("select count(*) from bot_audit").await;
    // 사전 검증(`check_seed`)은 통과하지만 DB의 제약이 둘째 시드를 거절한다: 트랜잭션 중간의 실제 오류
    db.sql("alter table bot_memories add constraint reject_poison check (body <> '독 시드')")
        .await;
    let result = db
        .store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            vec![
                seed("personal", "memory/first.md", 2, "첫 시드(정상)"),
                seed("personal", "memory/second.md", 3, "독 시드"),
            ],
            Default::default(),
        )
        .await;
    assert!(result.is_err(), "제약 위반은 게시 실패다");
    assert_eq!(
        db.store.current().await.unwrap(),
        Some(rev("set-1")),
        "이전 판본 그대로"
    );
    assert_eq!(
        db.scalar::<String>("select revision from bot_active_profiles where profile = 'personal'")
            .await,
        "r1"
    );
    assert_eq!(
        memories_in(&db).await,
        1,
        "먼저 들어간 첫 시드도 남지 않는다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
    // 제약을 풀면 같은 게시가 성공한다(실패가 잠금이나 반쪽 상태를 남기지 않았다)
    db.sql("alter table bot_memories drop constraint reject_poison")
        .await;
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            set(&[("personal", "r2")]),
            vec![seed("personal", "memory/second.md", 3, "독 시드")],
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
            vec![seed("personal", "memory/a.md", 1, "패자의 시드")],
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
                    &format!("시드 {n}"),
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
    assert_eq!(winners, 1, "CAS를 이긴 게시는 하나");
    assert_eq!(memories_in(&db).await, 1, "이긴 쪽의 시드만 있다");
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
    let ok = seed("personal", "memory/a.md", 1, "정상");
    assert_eq!(check_seed(&ok), Ok(()));
    let secret = "비밀 본문 SENTINEL";
    let mut bad_field = seed("personal", "memory/a.md", 1, secret);
    bad_field.owner = PrincipalId(String::new());
    assert_eq!(check_seed(&bad_field), Err(SeedError::BadField("owner")));
    assert_eq!(
        check_seed(&seed("personal", "p", 1, "  \n")),
        Err(SeedError::EmptyText)
    );
    let long = "가".repeat(9000);
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
            !shown.contains("SENTINEL") && !shown.contains("가가"),
            "{shown}"
        );
    }
}

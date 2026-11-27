use crate::support::*;
use bot_core::{command::*, id::*, port::*, state::*};

#[tokio::test]
async fn protects_owner_memory() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "Evening walk every day at 7 PM").await;
    assert_eq!(
        db.scalar::<String>("select owner || ':' || visibility || ':' || agent from bot_memories")
            .await,
        "local:owner:private:main"
    );
    assert!(
        !flag(&db, &cli).await,
        "creating private memory must not set private_memory_read"
    );
    assert_eq!(
        texts(&db, &cli, "walk").await,
        ["Evening walk every day at 7 PM"]
    );
    assert!(
        flag(&db, &cli).await,
        "reading private memory must set private_memory_read on the request tree"
    );

    // Owner channels, other users' DMs, schedules, and HTTP cannot read private memory.
    for (n, (principal, kind)) in [
        (OWNER, ConversationKind::Channel),
        (GUEST, ConversationKind::Direct),
        ("service:morning", ConversationKind::Scheduled),
        (OWNER, ConversationKind::Http),
    ]
    .into_iter()
    .enumerate()
    {
        let run = run_in(&db, &format!("other-{n}"), principal, kind).await;
        assert!(texts(&db, &run, "walk").await.is_empty(), "{principal} {n}");
        assert!(!flag(&db, &run).await);
    }
    let dm = run_in(&db, "dm", OWNER, ConversationKind::Direct).await;
    assert_eq!(texts(&db, &dm, "walk").await.len(), 1);

    // Other principals cannot edit or delete another owner's memory.
    let guest = run_in(&db, "guest", GUEST, ConversationKind::Direct).await;
    let edit = memories(&db)
        .change(
            guest.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "changed".into(),
            },
        )
        .await;
    let delete = memories(&db)
        .change(
            guest.attempt.clone(),
            MemoryChange::Delete { id: id.clone() },
        )
        .await;
    assert!(matches!(edit, Err(Error::Denied)) && matches!(delete, Err(Error::Denied)));
    assert_eq!(
        db.scalar::<String>(&format!(
            "select body from bot_memories where id = '{}'",
            id.0
        ))
        .await,
        "Evening walk every day at 7 PM"
    );
    let mine = create(&db, &guest, "my note").await;
    assert_eq!(
        db.scalar::<String>(&format!(
            "select owner from bot_memories where id = '{}'",
            mine.0
        ))
        .await,
        GUEST
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_audit where action like 'memory.%' and actor = 'slack:home/U2'").await, 1);
}

#[tokio::test]
async fn owner_edits_and_deletes_only_where_the_memory_is_readable() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "secret note").await;
    let channel = run_in(&db, "channel", OWNER, ConversationKind::Channel).await;
    for change in [
        MemoryChange::Edit {
            id: id.clone(),
            text: "x".into(),
        },
        MemoryChange::Delete { id: id.clone() },
    ] {
        assert!(matches!(
            memories(&db).change(channel.attempt.clone(), change).await,
            Err(Error::Denied)
        ));
    }
    memories(&db)
        .change(
            cli.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "updated note".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(texts(&db, &cli, "updated").await, ["updated note"]);
    memories(&db)
        .change(cli.attempt.clone(), MemoryChange::Delete { id: id.clone() })
        .await
        .unwrap();
    assert!(texts(&db, &cli, "").await.is_empty());
    assert_eq!(
        db.scalar::<String>("select body from bot_memories").await,
        "",
        "deletion clears the stored body"
    );
    let again = memories(&db)
        .change(cli.attempt.clone(), MemoryChange::Delete { id })
        .await;
    assert!(matches!(again, Err(Error::Missing)));
}

#[tokio::test]
async fn delegated_children_keep_the_requester_and_conversation_kind() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let setup = run_in(&db, "setup", OWNER, ConversationKind::Cli).await;
    create(&db, &setup, "Check personal schedule in the morning").await;
    for (request, kind, sees) in [
        ("channel", ConversationKind::Channel, false),
        ("dm", ConversationKind::Direct, true),
    ] {
        let parent = run_in(&db, request, OWNER, kind).await;
        let delegate = Delegate {
            checked_revision: bot_core::id::Revision("r1".into()),
            parent: parent.attempt.clone(),
            step: "1".into(),
            child: AgentId("main/research".into()),
            instruction: "research".into(),
            scopes: scopes(&["run"]),
        };
        db.store.delegate(delegate).await.unwrap();
        let child = runs(&db).claim().await.unwrap().unwrap();
        assert_eq!(child.agent.0, "main/research");
        let found = texts(&db, &child, "schedule").await;
        assert_eq!(!found.is_empty(), sees, "{request}");
        assert_eq!(flag(&db, &parent).await, sees, "{request}");
    }
}

#[tokio::test]
async fn memories_are_scoped_to_the_creating_agent_and_its_descendants() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let parent = run_in(&db, "p", OWNER, ConversationKind::Cli).await;
    let delegate = Delegate {
        checked_revision: bot_core::id::Revision("r1".into()),
        parent: parent.attempt.clone(),
        step: "1".into(),
        child: AgentId("main/research".into()),
        instruction: "research".into(),
        scopes: scopes(&["run"]),
    };
    db.store.delegate(delegate).await.unwrap();
    let child = runs(&db).claim().await.unwrap().unwrap();
    create(&db, &child, "Learned by research agent").await;
    assert_eq!(texts(&db, &child, "learned").await.len(), 1);

    // Parent cannot read child memories; descendants can read ancestor memories.
    runs(&db)
        .finish(child.attempt.clone(), done("x"))
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert!(texts(&db, &resumed, "learned").await.is_empty());
    create(&db, &resumed, "Main agent note").await;
    let next = Delegate {
        checked_revision: bot_core::id::Revision("r1".into()),
        parent: resumed.attempt.clone(),
        step: "2".into(),
        child: AgentId("main/research".into()),
        instruction: "again".into(),
        scopes: scopes(&["run"]),
    };
    db.store.delegate(next).await.unwrap();
    let again = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(texts(&db, &again, "note").await, ["Main agent note"]);
}

#[tokio::test]
async fn seeds_register_once_per_path_and_hash_and_stay_deleted() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let seed = |path: &str, hash: u8, vis: Visibility, text: &str| MemorySeed {
        profile: ProfileId("personal".into()),
        agent: AgentId("main".into()),
        path: path.into(),
        sha256: [hash; 32],
        owner: PrincipalId(OWNER.into()),
        visibility: vis,
        text: text.into(),
    };
    memories(&db)
        .seed(seed("memory/a.md", 1, Visibility::Private, "seed one"))
        .await
        .unwrap();
    memories(&db)
        .seed(seed("memory/a.md", 1, Visibility::Private, "seed one"))
        .await
        .unwrap();
    memories(&db)
        .seed(seed("memory/b.md", 2, Visibility::Profile, "shared seed"))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories").await,
        2
    );
    memories(&db)
        .seed(seed(
            "memory/a.md",
            3,
            Visibility::Private,
            "seed one revised",
        ))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories").await,
        3,
        "distinct content hash inserts a new seed"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.seed'")
            .await,
        3
    );

    let guest = run_in(&db, "g", GUEST, ConversationKind::Direct).await;
    assert_eq!(texts(&db, &guest, "").await, ["shared seed"]);
    let cli = run_in(&db, "c", OWNER, ConversationKind::Cli).await;
    assert_eq!(texts(&db, &cli, "").await.len(), 3);

    // Deleting a seed leaves a tombstone so re-seeding the same path+hash does not resurrect it.
    let first = db.scalar::<String>("select id from bot_memories where seed_path = 'memory/a.md' and seed_sha256 = decode(repeat('01', 32), 'hex')").await;
    memories(&db)
        .change(
            cli.attempt.clone(),
            MemoryChange::Delete {
                id: MemoryId(first),
            },
        )
        .await
        .unwrap();
    memories(&db)
        .seed(seed("memory/a.md", 1, Visibility::Private, "seed one"))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories").await,
        3,
        "re-seeding a deleted seed must not insert a new row"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories where deleted_ms is not null")
            .await,
        1
    );
    assert!(
        texts(&db, &cli, "one")
            .await
            .iter()
            .all(|t| t != "seed one")
    );
}

#[tokio::test]
async fn search_matches_terms_ranks_and_escapes_wildcards() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    create(&db, &cli, "Today's schedule has a morning meeting").await;
    create(&db, &cli, "Today has a lunch plan").await;
    create(&db, &cli, "100% completion log").await;
    create(&db, &cli, "snake_case naming convention").await;
    assert_eq!(
        texts(&db, &cli, "today schedule").await[0],
        "Today's schedule has a morning meeting",
        "more matched terms rank higher"
    );
    assert_eq!(
        texts(&db, &cli, "scheduled").await.len(),
        0,
        "substring search does not stem: 'scheduled' is absent from the body"
    );
    assert_eq!(texts(&db, &cli, "schedule").await.len(), 1);
    assert_eq!(texts(&db, &cli, "100%").await, ["100% completion log"]);
    assert_eq!(
        texts(&db, &cli, "snake_case").await,
        ["snake_case naming convention"]
    );
    assert_eq!(
        texts(&db, &cli, "").await.len(),
        4,
        "empty query returns recent visible memories"
    );
    assert!(texts(&db, &cli, "nonexistent").await.is_empty());
}

#[tokio::test]
async fn expired_or_replaced_owners_cannot_read_or_write_memory() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = run_in(&db, "r", OWNER, ConversationKind::Cli).await;
    create(&db, &old, "note").await;
    let new = db.takeover(&old.attempt.run).await;
    let stale = |r: Result<_, Error>| matches!(r, Err(Error::Stale));
    assert!(stale(
        memories(&db)
            .search(old.attempt.clone(), "note".into())
            .await
            .map(|_| ())
    ));
    assert!(stale(
        memories(&db)
            .change(
                old.attempt.clone(),
                MemoryChange::Create { text: "x".into() }
            )
            .await
            .map(|_| ())
    ));
    assert_eq!(texts(&db, &new, "note").await.len(), 1);
    for text in ["", "   ", "\0"] {
        let bad = memories(&db)
            .change(
                new.attempt.clone(),
                MemoryChange::Create { text: text.into() },
            )
            .await;
        assert!(matches!(bad, Err(Error::Invalid)), "{text:?}");
    }
    let huge = memories(&db)
        .change(
            new.attempt.clone(),
            MemoryChange::Create {
                text: "x".repeat(9000),
            },
        )
        .await;
    assert!(matches!(huge, Err(Error::Invalid)));
}

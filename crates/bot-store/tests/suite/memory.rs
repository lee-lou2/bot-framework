use crate::support::*;
use bot_core::{command::*, id::*, port::*, state::*};

#[tokio::test]
async fn protects_owner_memory() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 소유자는 CLI에서 private 기억을 만든다: owner는 인증된 요청자로 고정된다
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    let id = create(&db, &cli, "산책은 매일 저녁 7시에 한다").await;
    assert_eq!(
        db.scalar::<String>("select owner || ':' || visibility || ':' || agent from bot_memories")
            .await,
        "local:owner:private:main"
    );
    assert!(
        !flag(&db, &cli).await,
        "만들기만 해서는 private 조회가 아니다"
    );
    assert_eq!(
        texts(&db, &cli, "산책").await,
        ["산책은 매일 저녁 7시에 한다"]
    );
    assert!(
        flag(&db, &cli).await,
        "private 기억을 조회하면 모델에 주기 전에 요청 트리 플래그가 선다"
    );

    // 본인의 채널·다른 사용자의 DM·예약·HTTP에서는 private을 읽지 못하고 플래그도 서지 않는다
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
        assert!(texts(&db, &run, "산책").await.is_empty(), "{principal} {n}");
        assert!(!flag(&db, &run).await);
    }
    // 본인의 DM에서는 읽는다
    let dm = run_in(&db, "dm", OWNER, ConversationKind::Direct).await;
    assert_eq!(texts(&db, &dm, "산책").await.len(), 1);

    // 타인은 소유자를 지정·수정·삭제할 수 없다. 만든 기억의 owner는 그 사람 자신이다
    let guest = run_in(&db, "guest", GUEST, ConversationKind::Direct).await;
    let edit = memories(&db)
        .change(
            guest.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "바꿈".into(),
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
        "산책은 매일 저녁 7시에 한다"
    );
    let mine = create(&db, &guest, "내 메모").await;
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
    let id = create(&db, &cli, "비밀 메모").await;
    // 채널에서는 읽지 못하는 private 기억을 고치거나 지울 수도 없다
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
                text: "고친 메모".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(texts(&db, &cli, "고친").await, ["고친 메모"]);
    memories(&db)
        .change(cli.attempt.clone(), MemoryChange::Delete { id: id.clone() })
        .await
        .unwrap();
    assert!(texts(&db, &cli, "").await.is_empty());
    assert_eq!(
        db.scalar::<String>("select body from bot_memories").await,
        "",
        "삭제하면 본문을 비운다"
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
    create(&db, &setup, "개인 일정은 아침에 확인").await;
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
            instruction: "조사".into(),
            scopes: scopes(&["run"]),
        };
        db.store.delegate(delegate).await.unwrap();
        let child = runs(&db).claim().await.unwrap().unwrap();
        assert_eq!(child.agent.0, "main/research");
        let found = texts(&db, &child, "일정").await;
        assert_eq!(!found.is_empty(), sees, "{request}");
        // 자식이 private을 읽었다면 플래그는 요청 트리(루트)에 선다. 부모가 채널이면 아무것도 서지 않는다
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
        instruction: "조사".into(),
        scopes: scopes(&["run"]),
    };
    db.store.delegate(delegate).await.unwrap();
    let child = runs(&db).claim().await.unwrap().unwrap();
    create(&db, &child, "조사 에이전트가 배운 것").await;
    assert_eq!(texts(&db, &child, "배운").await.len(), 1);
    // 부모(main)는 자식의 기억을 읽지 못한다. 반대로 main의 기억은 자식이 읽는다
    runs(&db)
        .finish(child.attempt.clone(), done("x"))
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert!(texts(&db, &resumed, "배운").await.is_empty());
    create(&db, &resumed, "메인의 메모").await;
    let next = Delegate {
        checked_revision: bot_core::id::Revision("r1".into()),
        parent: resumed.attempt.clone(),
        step: "2".into(),
        child: AgentId("main/research".into()),
        instruction: "다시".into(),
        scopes: scopes(&["run"]),
    };
    db.store.delegate(next).await.unwrap();
    let again = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(texts(&db, &again, "메모").await, ["메인의 메모"]);
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
        .seed(seed("memory/a.md", 1, Visibility::Private, "시드 하나"))
        .await
        .unwrap();
    memories(&db)
        .seed(seed("memory/a.md", 1, Visibility::Private, "시드 하나"))
        .await
        .unwrap();
    memories(&db)
        .seed(seed("memory/b.md", 2, Visibility::Profile, "공용 시드"))
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
            "시드 하나 개정",
        ))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories").await,
        3,
        "내용 해시가 다르면 새 항목"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.seed'")
            .await,
        3
    );

    // 시드의 가시성: private은 소유자 DM·CLI, profile은 누구나
    let guest = run_in(&db, "g", GUEST, ConversationKind::Direct).await;
    assert_eq!(texts(&db, &guest, "").await, ["공용 시드"]);
    let cli = run_in(&db, "c", OWNER, ConversationKind::Cli).await;
    assert_eq!(texts(&db, &cli, "").await.len(), 3);

    // 소유자가 지운 시드는 다시 등록해도 되살아나지 않는다
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
        .seed(seed("memory/a.md", 1, Visibility::Private, "시드 하나"))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories").await,
        3,
        "지운 시드는 다시 등록해도 새로 만들어지지 않는다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_memories where deleted_ms is not null")
            .await,
        1
    );
    assert!(
        texts(&db, &cli, "하나")
            .await
            .iter()
            .all(|t| t != "시드 하나")
    );
}

#[tokio::test]
async fn search_matches_terms_ranks_and_escapes_wildcards() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cli = run_in(&db, "cli", OWNER, ConversationKind::Cli).await;
    create(&db, &cli, "오늘 일정은 오전 회의").await;
    create(&db, &cli, "오늘은 점심 약속").await;
    create(&db, &cli, "100% 완료 기록").await;
    create(&db, &cli, "snake_case 이름 규칙").await;
    assert_eq!(
        texts(&db, &cli, "오늘 일정").await[0],
        "오늘 일정은 오전 회의",
        "낱말을 더 많이 맞춘 것이 앞선다"
    );
    assert_eq!(
        texts(&db, &cli, "일정을").await.len(),
        0,
        "조사가 붙은 검색어는 본문 부분 일치로 찾는다: '일정을'은 본문에 없다"
    );
    assert_eq!(texts(&db, &cli, "일정").await.len(), 1);
    assert_eq!(texts(&db, &cli, "100%").await, ["100% 완료 기록"]);
    assert_eq!(
        texts(&db, &cli, "snake_case").await,
        ["snake_case 이름 규칙"]
    );
    assert_eq!(
        texts(&db, &cli, "").await.len(),
        4,
        "빈 검색어는 보이는 최근 기억"
    );
    assert!(texts(&db, &cli, "없는낱말").await.is_empty());
}

#[tokio::test]
async fn expired_or_replaced_owners_cannot_read_or_write_memory() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = run_in(&db, "r", OWNER, ConversationKind::Cli).await;
    create(&db, &old, "메모").await;
    let new = db.takeover(&old.attempt.run).await;
    let stale = |r: Result<_, Error>| matches!(r, Err(Error::Stale));
    assert!(stale(
        memories(&db)
            .search(old.attempt.clone(), "메모".into())
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
    assert_eq!(texts(&db, &new, "메모").await.len(), 1);
    // 잘못된 입력
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

//! 관리 조회(PG): 승인 목록의 필터·정렬·커서, 기억 공유 카드 본문의 공개 범위와 형식, 실행·감사 목록의 쪽 나누기.

use crate::support::{admin::*, *};
use bot_core::{
    admin::*,
    command::*,
    id::*,
    ledger::*,
    port::*,
    state::{Risk, RunContext},
};
use sha2::Digest;

/// 승인 하나를 연다(run마다 호출 하나). 만료까지 남은 시간(초)을 고른다.
async fn approval(db: &TestDb, name: &str, nonce: &str, ttl_secs: u64) -> RunContext {
    let run = leased(db, name).await;
    begin_call(db, &run.attempt, "c1").await;
    let mut a = binding(&run.attempt, "c1", nonce);
    a.expires_at = std::time::SystemTime::now() + std::time::Duration::from_secs(ttl_secs);
    a.risk = Risk::Write;
    calls(db).open_approval(open_approval(a)).await.unwrap();
    run
}

async fn pages(db: &TestDb, filter: ApprovalFilter, size: u32) -> Vec<String> {
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = AdminRead::approvals(
            &db.store,
            &operator(),
            ApprovalQuery {
                profile: None,
                filter,
                page: Page { limit: size, after },
            },
        )
        .await
        .unwrap();
        seen.extend(page.items.iter().map(|a| a.nonce.0.clone()));
        match page.next {
            Some(next) => after = Some(next),
            None => return seen,
        }
    }
}

#[tokio::test]
async fn approvals_are_filtered_before_paging_and_ordered_by_the_documented_keys() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    approval(&db, "a", "n-late", 7200).await;
    approval(&db, "b", "n-soon", 600).await;
    approval(&db, "c", "n-mid", 1800).await;
    approval(&db, "d", "n-mid2", 3000).await;
    approval(&db, "e", "n-denied", 3600).await;
    approval(&db, "f", "n-approved", 3600).await;
    let sup = approval(&db, "g", "n-sup", 3600).await;
    let decide = |nonce: &str, allow: bool| {
        calls(&db).decide(Decision {
            nonce: ApprovalId(nonce.into()),
            actor: PrincipalId(OWNER.into()),
            allow,
        })
    };
    decide("n-denied", false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    decide("n-approved", true).await.unwrap();
    // 자동으로 닫힌 행(처리 시각 없음)
    calls(&db)
        .supersede(ApprovalId("n-sup".into()), PrincipalId(OWNER.into()))
        .await
        .unwrap();
    assert!(
        db.scalar::<bool>("select decided_ms is null from bot_approvals where nonce = 'n-sup'")
            .await
    );
    let _ = sup;

    // 필터를 쪽 나누기 전에 적용하고, 어느 쪽 크기로 넘겨도 같은 순서다
    for size in [1, 2, 200] {
        assert_eq!(
            pages(&db, ApprovalFilter::Pending, size).await,
            ["n-soon", "n-mid", "n-mid2", "n-late"],
            "Pending은 expires_at 오름차순"
        );
        assert_eq!(
            pages(&db, ApprovalFilter::Decided, size).await,
            ["n-approved", "n-denied", "n-sup"],
            "Decided는 처리 시각 최신순, 시각 없는 행은 뒤"
        );
        assert_eq!(
            pages(&db, ApprovalFilter::All, size).await,
            [
                "n-approved",
                "n-denied",
                "n-sup",
                "n-soon",
                "n-mid",
                "n-mid2",
                "n-late"
            ],
            "All은 처리 행이 먼저, 대기 묶음은 마지막이며 만료가 가까운 순"
        );
    }
}

#[tokio::test]
async fn a_memory_share_card_body_is_shown_only_to_the_owner_who_requested_and_approves_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "share").await;
    begin_call(&db, &run.attempt, "c1").await;
    let mut a = binding(&run.attempt, "c1", "n-share");
    a.tool = ToolId("memory.share".into());
    let card = r#"{"approval":{"nonce":"n-share","title":"기억 공유","lines":["내용: 비밀 본문-SENTINEL"]}}"#;
    calls(&db)
        .open_approval(OpenApproval {
            approval: a,
            reasons: vec!["irreversible".into()],
            card: JsonPayload(card.into()),
            delivery: None,
        })
        .await
        .unwrap();
    let shows = |view: &ApprovalView| format!("{:?}", view.render).contains("본문-SENTINEL");
    let nonce = ApprovalId("n-share".into());
    // 소유자 본인: 상세·목록 모두 본문이 보인다
    let mine = AdminRead::approval(&db.store, &owner(), nonce.clone())
        .await
        .unwrap();
    assert!(shows(&mine));
    // 다른 운영자: 메타데이터는 보이지만 본문은 없다(상세·목록 모두)
    let other = AdminRead::approval(&db.store, &operator(), nonce.clone())
        .await
        .unwrap();
    assert!(!shows(&other));
    assert!(
        other.render.is_empty(),
        "비소유자에게는 빈 배열(문구는 화면 몫)"
    );
    assert_eq!(other.tool.0, "memory.share");
    assert_eq!(other.args_sha256, mine.args_sha256, "메타데이터는 그대로");
    let listing = AdminRead::approvals(
        &db.store,
        &operator(),
        ApprovalQuery {
            page: Page {
                limit: 10,
                after: None,
            },
            ..ApprovalQuery::default()
        },
    )
    .await
    .unwrap();
    assert!(listing.items.iter().all(|v| !shows(v)));
    let listing = AdminRead::approvals(
        &db.store,
        &owner(),
        ApprovalQuery {
            page: Page {
                limit: 10,
                after: None,
            },
            ..ApprovalQuery::default()
        },
    )
    .await
    .unwrap();
    assert!(listing.items.iter().any(shows));
    // 운영자이면서 소유자여도 요청자·승인자가 본인일 때만 본다
    let both = Viewer {
        principal: PrincipalId(OWNER.into()),
        operator: true,
    };
    assert!(shows(
        &AdminRead::approval(&db.store, &both, nonce).await.unwrap()
    ));
}

#[tokio::test]
async fn the_audit_listing_can_return_one_extra_row_to_detect_a_next_page() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for i in 0..205 {
        db.store
            .append(bot_core::command::AuditEntry {
                profile: profile_id(),
                actor: "system".into(),
                action: "test.row".into(),
                target: format!("t{i}"),
                args_sha256: None,
                verdict: "ok".into(),
            })
            .await
            .unwrap();
    }
    let rows = AdminRead::audit(
        &db.store,
        &operator(),
        AuditQuery {
            profile: None,
            before: None,
            limit: 201,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        201,
        "한 쪽 최대(200)보다 한 건 더 요청할 수 있다"
    );
}

#[tokio::test]
async fn the_share_card_body_comes_back_as_one_row_that_hashes_to_the_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "card").await;
    begin_call(&db, &run.attempt, "c1").await;
    let id = MemoryId("m-1".into());
    let body = "첫 줄\n\"따옴표\"와 역슬래시 \\ 그리고 아주 긴 글이 이어진다. ".repeat(6);
    let digest: [u8; 32] = sha2::Sha256::digest(body.as_bytes()).into();
    // 브로커 카드와 같은 모양: 본문을 JSON 문자열로 쓰고 줄 상한(여기서는 80바이트)마다 이어 쓴다
    let quoted = serde_json::to_string(&body).unwrap();
    let mut lines = vec![
        "도구: memory.share".to_owned(),
        "기억 ID: m-1".to_owned(),
        "공유 범위: 프로필 전체".to_owned(),
    ];
    let (mut first, mut rest) = (true, quoted.as_str());
    while !rest.is_empty() {
        let mut end = rest.len().min(80);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        lines.push(format!(
            "{}{}",
            if first { "본문: " } else { "  " },
            &rest[..end]
        ));
        (first, rest) = (false, &rest[end..]);
    }
    assert!(lines.len() > 5, "여러 줄로 쪼개진다");
    let card = serde_json::json!({"approval": {"nonce": "n-card", "title": "기억 공유 승인", "lines": lines}});
    let mut a = binding(&run.attempt, "c1", "n-card");
    a.tool = ToolId("memory.share".into());
    a.args_sha256 = memory_share_hash(&id, &digest);
    calls(&db)
        .open_approval(OpenApproval {
            approval: a,
            reasons: vec!["irreversible".into()],
            card: JsonPayload(card.to_string()),
            delivery: None,
        })
        .await
        .unwrap();
    let view = AdminRead::approval(&db.store, &owner(), ApprovalId("n-card".into()))
        .await
        .unwrap();
    let bodies: Vec<_> = view
        .render
        .iter()
        .filter(|(label, _)| label == "본문")
        .collect();
    assert_eq!(bodies.len(), 1, "본문은 한 행이다");
    assert_eq!(bodies[0].1, body, "조각을 이어 푼 원문 전체");
    assert!(
        view.render.iter().all(|(label, _)| !label.is_empty()),
        "이어 쓴 조각 줄이 따로 남지 않는다"
    );
    let shown: [u8; 32] = sha2::Sha256::digest(bodies[0].1.as_bytes()).into();
    assert_eq!(
        memory_share_hash(&id, &shown),
        view.args_sha256,
        "보이는 본문 = 승인 지문"
    );
    // 다른 카드는 줄을 그대로 둔다(본문 규칙은 기억 공유 카드에만)
    assert!(
        view.render
            .iter()
            .any(|(l, v)| l == "기억 ID" && v == "m-1")
    );
}

#[tokio::test]
async fn run_pages_include_the_profile_so_equal_ids_and_times_are_not_skipped() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for profile in ["personal", "work"] {
        sqlx::query(
            "insert into bot_runs (profile, id, root, agent, principal, revision, binding, conv_kind, conv_id, target, input, updated_ms)
             values ($1, 'same', 'same', 'main', 'local:owner', 'r1', 'desk', 'cli', 'c1',
                     '{\"kind\":\"agent\",\"agent\":\"main\",\"instruction\":\"x\"}', '{}', 1000)",
        )
        .bind(profile)
        .execute(db.store.pool())
        .await
        .unwrap();
    }
    for size in [1, 2] {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = AdminRead::runs(
                &db.store,
                &operator(),
                RunQuery {
                    page: Page { limit: size, after },
                    ..RunQuery::default()
                },
            )
            .await
            .unwrap();
            seen.extend(
                page.items
                    .iter()
                    .map(|r| format!("{}/{}", r.profile.0, r.run.0)),
            );
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(
            seen,
            ["personal/same", "work/same"],
            "쪽 크기 {size}: 같은 ID·시각도 빠지지 않고 프로필 순"
        );
    }
}

#[tokio::test]
async fn a_card_with_two_body_blocks_or_an_undecodable_body_does_not_collapse_into_one_valid_row() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let id = MemoryId("m-1".into());
    let first = serde_json::to_string("첫 본문").unwrap();
    let second = serde_json::to_string("둘째 본문").unwrap();
    for (name, nonce, body_lines) in [
        (
            "dup",
            "n-dup",
            vec![format!("본문: {first}"), format!("본문: {second}")],
        ),
        ("bad", "n-bad", vec!["본문: \"끝나지 않는 글".to_owned()]),
        // 한쪽만 풀려도 둘 이상이면 승인할 수 없다: 풀린 하나가 유일한 본문 행으로 보이면 안 된다
        (
            "mixed",
            "n-mixed",
            vec![
                format!("본문: {first}"),
                "본문: \"끝나지 않는 글".to_owned(),
            ],
        ),
        (
            "mixed-last",
            "n-mixed-last",
            vec![
                "본문: \"끝나지 않는 글".to_owned(),
                format!("본문: {second}"),
            ],
        ),
    ] {
        let run = leased(&db, name).await;
        begin_call(&db, &run.attempt, "c1").await;
        let mut lines = vec!["도구: memory.share".to_owned(), "기억 ID: m-1".to_owned()];
        lines.extend(body_lines);
        let card = serde_json::json!({"approval": {"nonce": nonce, "title": "기억 공유 승인", "lines": lines}});
        let mut a = binding(&run.attempt, "c1", nonce);
        a.tool = ToolId("memory.share".into());
        a.args_sha256 = memory_share_hash(&id, &[0; 32]);
        calls(&db)
            .open_approval(OpenApproval {
                approval: a,
                reasons: vec![],
                card: JsonPayload(card.to_string()),
                delivery: None,
            })
            .await
            .unwrap();
    }
    let rows = |nonce: &str| {
        let nonce = ApprovalId(nonce.into());
        let db = &db;
        async move {
            AdminRead::approval(&db.store, &owner(), nonce)
                .await
                .unwrap()
                .render
                .into_iter()
                .filter(|(label, _)| label == "본문")
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
        }
    };
    for nonce in ["n-dup", "n-mixed", "n-mixed-last"] {
        assert!(
            rows(nonce).await.is_empty(),
            "{nonce}: 본문 묶음이 둘 이상이면 풀리는 것이 있어도 본문 행이 없다(승인 불가)"
        );
    }
    assert!(
        rows("n-bad").await.is_empty(),
        "풀 수 없는 본문은 행이 없다"
    );
}

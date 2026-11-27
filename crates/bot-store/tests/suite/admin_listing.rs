//! Admin read queries (PG): approval filtering/ordering/cursors, memory-share body visibility and rendering, and run/audit paging.

use crate::support::{admin::*, *};
use bot_core::{
    admin::*,
    cards::{LABEL_BODY, LABEL_MEMORY_ID, LABEL_SHARE_SCOPE, LABEL_TOOL},
    command::*,
    id::*,
    ledger::*,
    port::*,
    state::{Risk, RunContext},
};
use sha2::Digest;

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
    calls(&db)
        .supersede(ApprovalId("n-sup".into()), PrincipalId(OWNER.into()))
        .await
        .unwrap();
    assert!(
        db.scalar::<bool>("select decided_ms is null from bot_approvals where nonce = 'n-sup'")
            .await
    );
    let _ = sup;

    for size in [1, 2, 200] {
        assert_eq!(
            pages(&db, ApprovalFilter::Pending, size).await,
            ["n-soon", "n-mid", "n-mid2", "n-late"],
            "Pending orders by expires_at ascending"
        );
        assert_eq!(
            pages(&db, ApprovalFilter::Decided, size).await,
            ["n-approved", "n-denied", "n-sup"],
            "Decided orders by decided_ms descending, with null decided_ms last"
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
            "All lists decided rows first, then pending rows by earliest expiry"
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
    let card = r#"{"approval":{"nonce":"n-share","title":"Memory share approval","lines":["Content: secret body-SENTINEL"]}}"#;
    calls(&db)
        .open_approval(OpenApproval {
            approval: a,
            reasons: vec!["irreversible".into()],
            card: JsonPayload(card.into()),
            delivery: None,
        })
        .await
        .unwrap();
    let shows = |view: &ApprovalView| format!("{:?}", view.render).contains("body-SENTINEL");
    let nonce = ApprovalId("n-share".into());
    let mine = AdminRead::approval(&db.store, &owner(), nonce.clone())
        .await
        .unwrap();
    assert!(shows(&mine));

    // Another operator sees metadata only; rendered card lines stay empty.
    let other = AdminRead::approval(&db.store, &operator(), nonce.clone())
        .await
        .unwrap();
    assert!(!shows(&other));
    assert!(
        other.render.is_empty(),
        "non-owners receive an empty render list"
    );
    assert_eq!(other.tool.0, "memory.share");
    assert_eq!(other.args_sha256, mine.args_sha256);
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
        "audit query allows limit = page max (200) + 1 for lookahead"
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
    let body =
        "First line\n\"quotes\" and backslash \\ followed by a much longer sentence. ".repeat(6);
    let digest: [u8; 32] = sha2::Sha256::digest(body.as_bytes()).into();
    // Matches broker card layout: JSON-quoted body wrapped across 80-byte continuation lines.
    let quoted = serde_json::to_string(&body).unwrap();
    let mut lines = vec![
        format!("{LABEL_TOOL}: memory.share"),
        format!("{LABEL_MEMORY_ID}: m-1"),
        format!("{LABEL_SHARE_SCOPE}: Profile-wide (currently private to you)"),
    ];
    let (mut first, mut rest) = (true, quoted.as_str());
    while !rest.is_empty() {
        let mut end = rest.len().min(80);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        lines.push(format!(
            "{}{}",
            if first {
                format!("{LABEL_BODY}: ")
            } else {
                "  ".to_owned()
            },
            &rest[..end]
        ));
        (first, rest) = (false, &rest[end..]);
    }
    assert!(lines.len() > 5, "body should wrap across multiple lines");
    let card = serde_json::json!({
        "approval": {
            "nonce": "n-card",
            "title": "Memory share approval",
            "lines": lines,
        }
    });
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
        .filter(|(label, _)| label == LABEL_BODY)
        .collect();
    assert_eq!(bodies.len(), 1, "body must collapse into one row");
    assert_eq!(bodies[0].1, body, "decoded body must match original text");
    assert!(
        view.render.iter().all(|(label, _)| !label.is_empty()),
        "continuation chunks must not remain as separate rows"
    );
    let shown: [u8; 32] = sha2::Sha256::digest(bodies[0].1.as_bytes()).into();
    assert_eq!(
        memory_share_hash(&id, &shown),
        view.args_sha256,
        "rendered body must match approval digest"
    );
    assert!(
        view.render
            .iter()
            .any(|(l, v)| l == LABEL_MEMORY_ID && v == "m-1")
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
            "page size {size}: ties on id and timestamp order by profile without skipping"
        );
    }
}

#[tokio::test]
async fn a_card_with_two_body_blocks_or_an_undecodable_body_does_not_collapse_into_one_valid_row() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let id = MemoryId("m-1".into());
    let first = serde_json::to_string("first body").unwrap();
    let second = serde_json::to_string("second body").unwrap();
    for (name, nonce, body_lines) in [
        (
            "dup",
            "n-dup",
            vec![
                format!("{LABEL_BODY}: {first}"),
                format!("{LABEL_BODY}: {second}"),
            ],
        ),
        (
            "bad",
            "n-bad",
            vec![format!("{LABEL_BODY}: \"unterminated string")],
        ),
        // Even if one block decodes, multiple body blocks must not yield a single approvable row.
        (
            "mixed",
            "n-mixed",
            vec![
                format!("{LABEL_BODY}: {first}"),
                format!("{LABEL_BODY}: \"unterminated string"),
            ],
        ),
        (
            "mixed-last",
            "n-mixed-last",
            vec![
                format!("{LABEL_BODY}: \"unterminated string"),
                format!("{LABEL_BODY}: {second}"),
            ],
        ),
    ] {
        let run = leased(&db, name).await;
        begin_call(&db, &run.attempt, "c1").await;
        let mut lines = vec![
            format!("{LABEL_TOOL}: memory.share"),
            format!("{LABEL_MEMORY_ID}: m-1"),
        ];
        lines.extend(body_lines);
        let card = serde_json::json!({
            "approval": {
                "nonce": nonce,
                "title": "Memory share approval",
                "lines": lines,
            }
        });
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
                .filter(|(label, _)| label == LABEL_BODY)
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
        }
    };
    for nonce in ["n-dup", "n-mixed", "n-mixed-last"] {
        assert!(
            rows(nonce).await.is_empty(),
            "{nonce}: duplicate body blocks must emit no body row"
        );
    }
    assert!(
        rows("n-bad").await.is_empty(),
        "undecodable body must emit no body row"
    );
}

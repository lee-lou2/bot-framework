use super::*;

#[tokio::test]
async fn redelivery_keeps_the_original_transport() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut request = accept_as(
        "personal",
        "same",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    let first = runs(&db).accept(request.clone()).await.unwrap();
    let other = PushTransportId::new("test-push").unwrap();
    request.reply_transport = Some(other.clone());
    assert_eq!(
        runs(&db)
            .accept_with(request, AcceptExtras::default())
            .await
            .unwrap(),
        first
    );
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(first, ctx.attempt.run);
    runs(&db).finish(ctx.attempt, done("답")).await.unwrap();
    assert!(
        ob(&db)
            .claim(&DeliveryScope::all(other), 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(ob(&db).claim(&SLACK, 1).await.unwrap()[0].key.run, first);
}

#[tokio::test]
async fn external_push_and_pull_accepts_require_the_matching_transport_shape() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut direct = accept_as(
        "personal",
        "missing",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    direct.reply_transport = None;
    let mut pull = accept("personal", "extra", "hi");
    pull.reply_transport = Some(PushTransportId::SLACK);
    for command in [direct, pull] {
        assert!(matches!(
            runs(&db).accept(command).await,
            Err(Error::Invalid)
        ));
    }
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );
}

#[tokio::test]
async fn a_later_reply_cannot_pass_an_earlier_reply_owned_by_another_transport() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    two_replies(&db).await;
    // 같은 실행에서 먼저 원래 대화로, 나중에 별도 목적지로 보내는 순서다.
    db.sql("update bot_outbox set transport = 'test-push' where seq = 1")
        .await;
    let other = DeliveryScope::all(PushTransportId::new("test-push").unwrap());
    assert!(ob(&db).claim(&other, 1).await.unwrap().is_empty());
    let first = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0);
    ob(&db).begin(first.key.clone(), rev("r1")).await.unwrap();
    assert!(ob(&db).claim(&other, 1).await.unwrap().is_empty());
    ob(&db)
        .settle(
            first.key,
            DeliveryOutcome::Confirmed {
                receipt: "first".into(),
            },
        )
        .await
        .unwrap();
    let next = ob(&db).claim(&other, 1).await.unwrap().remove(0);
    assert_eq!(next.key.seq, 1);
    assert_eq!(next.transport, other.transport);
}

#[tokio::test]
async fn upgrade_preserves_all_legacy_delivery_states_and_isolates_unclassified_rows() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let original = accept_as(
        "personal",
        "legacy",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    let slack = runs(&db).accept(original.clone()).await.unwrap();
    let pull = runs(&db)
        .accept(accept("personal", "pull", "hi"))
        .await
        .unwrap();
    for sql in [
        "drop index bot_outbox_pending",
        "alter table bot_outbox drop column transport",
        "alter table bot_runs drop column reply_transport",
        "create index bot_outbox_pending on bot_outbox (created_ms, profile, run, seq) where state = 'pending'",
        "delete from _sqlx_migrations where version = 26",
    ] {
        db.sql(sql).await;
    }
    sqlx::query(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, body,
            state, generation, attempts, not_before_ms, lease_until_ms, receipt, reason)
         select 'personal', $1, n, 'legacy:' || n, 'desk', 'slack:home/U1', 'direct', 'D1', '{\"text\":\"원문\"}',
                state, n + 7, n, 12, case when state = 'sending' then 1900000000000 end,
                case when state = 'confirmed' then 'kept-receipt' end,
                case when state = 'held' then 'owner_lost' end
         from unnest(array['pending', 'sending', 'held', 'confirmed', 'rejected']) with ordinality as o(state, n)",
    ).bind(&slack.0).execute(db.store.pool()).await.unwrap();
    sqlx::query(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, body)
         values ('personal', $1, 0, 'unknown', 'desk', 'local:owner', 'direct', 'unknown', '{}')",
    ).bind(&pull.0).execute(db.store.pool()).await.unwrap();
    let snapshot = || {
        db.scalar::<serde_json::Value>(
            "select jsonb_agg(to_jsonb(o) - 'transport' order by o.call) from bot_outbox o",
        )
    };
    let before = snapshot().await;

    db.store.migrate().await.unwrap();
    assert_eq!(snapshot().await, before);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_outbox where transport = 'slack'")
            .await,
        5
    );
    assert!(
        db.scalar::<bool>("select transport = '' from bot_outbox where call = 'unknown'")
            .await
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where reply_transport = 'slack'")
            .await,
        1
    );
    let item = ob(&db).claim(&SLACK, 10).await.unwrap().remove(0);
    assert_eq!(
        (item.key.run, item.key.generation),
        (slack.clone(), Generation(9))
    );
    assert_eq!(item.body.0, r#"{"text":"원문"}"#);
    assert!(ob(&db).claim(&SLACK, 10).await.unwrap().is_empty());
    assert_eq!(runs(&db).accept(original).await.unwrap(), slack);
    assert!(matches!(PushTransportId::new(""), Err(Error::Invalid)));
}

async fn channel_child(db: &TestDb, transport: PushTransportId) -> RunContext {
    let mut request = accept_as(
        "personal",
        "parent",
        "local:owner",
        ConversationKind::Channel,
        "C1",
        "hi",
    );
    request.reply_transport = Some(transport);
    runs(db).accept(request).await.unwrap();
    let parent = runs(db).claim().await.unwrap().unwrap().attempt;
    let child = db
        .store
        .delegate(Delegate {
            parent,
            step: "child".into(),
            child: AgentId("main/child".into()),
            instruction: "확인".into(),
            scopes: scopes(&["run"]),
            checked_revision: rev("r1"),
        })
        .await
        .unwrap();
    let ctx = runs(db).claim().await.unwrap().unwrap();
    assert_eq!(ctx.attempt.run, child);
    ctx
}

#[tokio::test]
async fn a_child_approval_and_its_owner_dm_notice_keep_the_original_transport() {
    use bot_core::{
        cards::{CardOutbox, DM_SENT_NOTICE},
        ledger::CallLedger,
    };

    let Some(db) = TestDb::new().await else {
        return;
    };
    let custom = PushTransportId::new("test-push").unwrap();
    let child = channel_child(&db, custom.clone()).await;
    begin_call(&db, &child.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&child.attempt, "c1", "plain")))
        .await
        .unwrap();
    assert!(ob(&db).claim(&SLACK, 1).await.unwrap().is_empty());
    let item = ob(&db)
        .claim(&DeliveryScope::all(custom), 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(item.key.run, child.attempt.run);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&item.body.0).unwrap()["approval"]["nonce"],
        "plain"
    );

    let db = TestDb::new().await.unwrap();
    let child = channel_child(&db, PushTransportId::SLACK).await;
    let (_, open) = cards::create_card(&db, &child, "owner-dm").await;
    let pinned = db.store.for_activation(rev("set-1"));
    CallLedger::open_approval(&pinned, open).await.unwrap();
    assert!(ob(&db).claim(&SLACK, 1).await.unwrap().is_empty());
    let job = cards::sending(&db).await;
    CardOutbox::settle(&db.store, job.key, cards::posted())
        .await
        .unwrap();
    let notice = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0);
    assert_eq!(notice.key.run, child.attempt.run);
    assert_eq!(notice.transport, PushTransportId::SLACK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&notice.body.0).unwrap()["text"],
        DM_SENT_NOTICE
    );
}

#[tokio::test]
async fn a_non_slack_origin_cannot_open_the_slack_only_owner_dm_path() {
    use bot_core::ledger::CallLedger;

    let Some(db) = TestDb::new().await else {
        return;
    };
    let child = channel_child(&db, PushTransportId::new("test-push").unwrap()).await;
    let (_, open) = cards::create_card(&db, &child, "owner-dm").await;
    let pinned = db.store.for_activation(rev("set-1"));
    assert!(matches!(
        CallLedger::open_approval(&pinned, open).await,
        Err(Error::Denied)
    ));
    for table in ["bot_approvals", "bot_card_jobs", "bot_outbox"] {
        assert_eq!(
            db.scalar::<i64>(&format!("select count(*) from {table}"))
                .await,
            0
        );
    }
}

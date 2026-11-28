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
    runs(&db).finish(ctx.attempt, done("reply")).await.unwrap();
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
         select 'personal', $1, n, 'legacy:' || n, 'desk', 'slack:home/U1', 'direct', 'D1', '{\"text\":\"original\"}',
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
    assert_eq!(item.body.0, r#"{"text":"original"}"#);
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
            instruction: "check".into(),
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

    // Telegram group-origin approval stages an owner-DM card job claimed only by Telegram transport.
    let db = TestDb::new().await.unwrap();
    let tg_child = channel_child(&db, PushTransportId::TELEGRAM).await;
    begin_call(&db, &tg_child.attempt, "tg-c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&tg_child.attempt, "tg-c1", "tg-dm")))
        .await
        .unwrap();
    assert!(
        ob(&db)
            .claim(&DeliveryScope::all(PushTransportId::TELEGRAM), 1)
            .await
            .unwrap()
            .is_empty(),
        "Telegram group approvals must not enter the group outbox directly"
    );
    assert!(
        CardOutbox::claim(&db.store, 8).await.unwrap().is_empty(),
        "Slack card claim must ignore Telegram card jobs"
    );
    let tg_job = CardOutbox::claim_for(&db.store, &PushTransportId::TELEGRAM, 8)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(tg_job.key.nonce.0, "tg-dm");
    CardOutbox::begin(&db.store, tg_job.key.clone(), rev("r1"))
        .await
        .unwrap();
    CardOutbox::settle(&db.store, tg_job.key, cards::posted())
        .await
        .unwrap();
    let tg_notice = ob(&db)
        .claim(&DeliveryScope::all(PushTransportId::TELEGRAM), 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(tg_notice.key.run, tg_child.attempt.run);
    assert_eq!(tg_notice.transport, PushTransportId::TELEGRAM);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&tg_notice.body.0).unwrap()["text"],
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

#[tokio::test]
async fn missing_transport_reply_is_held_with_open_hold_and_does_not_block_later_replies() {
    use bot_core::admin::{AdminRead, HoldKind};

    let Some(db) = TestDb::new().await else {
        return;
    };
    let req = accept_as(
        "personal",
        "legacy-null",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    let run = runs(&db).accept(req).await.unwrap();
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(ctx.attempt.run, run);
    // Reproduce a legacy push run with `reply_transport IS NULL`.
    sqlx::query(
        "update bot_runs set reply_transport = null where profile = 'personal' and id = $1",
    )
    .bind(&run.0)
    .execute(db.store.pool())
    .await
    .unwrap();

    // Run completion succeeds and records the reply directly as `held` instead of failing the queue.
    assert_eq!(
        runs(&db).finish(ctx.attempt, done("reply")).await.unwrap(),
        Completion::Succeeded
    );
    assert_eq!(state_of_run(&db, &run).await, "succeeded");
    let row: (String, Option<String>, String) = sqlx::query_as(
        "select state, reason, transport from bot_outbox where profile = 'personal' and run = $1 and seq = 0",
    )
    .bind(&run.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "held".to_owned(),
            Some("missing_transport".to_owned()),
            String::new()
        )
    );
    assert!(ob(&db).claim(&SLACK, 10).await.unwrap().is_empty());

    let holds = AdminRead::holds(
        &db.store,
        &crate::support::admin::operator(),
        Some(ProfileId("personal".into())),
    )
    .await
    .unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].kind, HoldKind::Reply);
    assert_eq!(holds[0].run, run);
    assert_eq!(holds[0].reason, "missing_transport");
    assert!(holds[0].hold_id.is_some());
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'outbox.held' and verdict = 'missing_transport'",
        )
        .await,
        1
    );

    // An earlier `held` row must not head-of-line block a later valid reply (`seq = 1`) for the same run.
    sqlx::query(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, body, transport)
         values ('personal', $1, 1, 'reply:' || $1 || ':1', 'desk', 'slack:home/U1', 'direct', 'D1', '{\"text\":\"later\"}', 'slack')",
    )
    .bind(&run.0)
    .execute(db.store.pool())
    .await
    .unwrap();
    let claimed = ob(&db).claim(&SLACK, 10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].key.seq, 1);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn resolve_reply_refuses_not_executed_for_empty_transport_but_allows_confirmed() {
    use bot_core::admin::{AdminAct, AdminRead, HoldDecision, HoldRef, HoldVerdict};

    let Some(db) = TestDb::new().await else {
        return;
    };
    let req = accept_as(
        "personal",
        "empty-hold",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    let run = runs(&db).accept(req).await.unwrap();
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    sqlx::query(
        "update bot_runs set reply_transport = null where profile = 'personal' and id = $1",
    )
    .bind(&run.0)
    .execute(db.store.pool())
    .await
    .unwrap();
    runs(&db).finish(ctx.attempt, done("reply")).await.unwrap();

    let view = AdminRead::holds(
        &db.store,
        &crate::support::admin::operator(),
        Some(ProfileId("personal".into())),
    )
    .await
    .unwrap()
    .remove(0);
    let hold = HoldRef {
        profile: view.profile,
        run: view.run.clone(),
        call: view.call,
        effect: None,
        hold_id: view.hold_id.expect("open hold ID"),
    };
    let actor = PrincipalId("local:operator".into());
    // Re-queueing an empty-transport reply via `NotExecuted` is refused to avoid an unsendable row.
    assert!(matches!(
        AdminAct::resolve_hold(
            &db.store,
            actor.clone(),
            hold.clone(),
            HoldVerdict::NotExecuted {
                evidence: "confirmed unsent".into(),
            },
        )
        .await,
        Err(Error::Conflict)
    ));
    assert_eq!(state_of(&db, 0).await, "held");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where closed_ms is null")
            .await,
        1
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'hold.resolve'")
            .await,
        0
    );
    assert_eq!(
        AdminAct::resolve_hold(
            &db.store,
            actor,
            hold,
            HoldVerdict::Confirmed {
                receipt: "manual-receipt-1".into(),
            },
        )
        .await
        .unwrap(),
        HoldDecision::Applied
    );
    assert_eq!(state_of(&db, 0).await, "confirmed");
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn bot_runs_check_rejects_empty_reply_transport() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let req = accept_as(
        "personal",
        "empty-check",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    let run = runs(&db).accept(req).await.unwrap();
    let err = sqlx::query(
        "update bot_runs set reply_transport = '' where profile = 'personal' and id = $1",
    )
    .bind(&run.0)
    .execute(db.store.pool())
    .await
    .unwrap_err();
    let code = err
        .as_database_error()
        .and_then(|db_err| db_err.code())
        .map(|c| c.into_owned());
    assert_eq!(code.as_deref(), Some("23514"));
}

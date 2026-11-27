//! Slack event deduplication: message `(team, channel, ts)` and reaction `(team, event_id)` converge across renamed bindings and duplicate deliveries.

use crate::support::*;
use bot_core::{
    command::*,
    id::*,
    slack::{SlackSource, SourceKind},
    state::ConversationKind,
};

fn source(kind: SourceKind, team: &str, channel: &str, ts: &str, event_id: &str) -> SlackSource {
    SlackSource {
        kind,
        team: team.into(),
        channel: channel.into(),
        ts: ts.into(),
        thread_ts: None,
        event_id: event_id.into(),
        reaction: (kind == SourceKind::Reaction).then(|| "white_check_mark".into()),
    }
}

fn slack_accept(binding: &str, request: &str, principal: &str) -> Accept {
    let mut a = accept_as(
        "personal",
        request,
        principal,
        ConversationKind::Channel,
        "C1",
        "question",
    );
    a.event.binding = BindingId(binding.into());
    a
}

async fn take(db: &TestDb, a: Accept, source: Option<SlackSource>) -> RunId {
    runs(db)
        .accept_with(
            a,
            AcceptExtras {
                source,
                chain: None,
            },
        )
        .await
        .unwrap()
}

async fn rows(db: &TestDb) -> (i64, i64) {
    (
        db.scalar::<i64>("select count(*) from bot_runs").await,
        db.scalar::<i64>("select count(*) from bot_requests").await,
    )
}

#[tokio::test]
async fn a_post_redelivered_under_another_binding_or_another_delivery_is_one_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let user = "slack:home/U1";
    let post = |event_id: &str| source(SourceKind::Message, "T1", "C1", "100.1", event_id);
    let first = take(
        &db,
        slack_accept("team", "C1.100.1", user),
        Some(post("Ev1")),
    )
    .await;
    let renamed = take(
        &db,
        slack_accept("team2", "C1.100.1", user),
        Some(post("Ev1")),
    )
    .await;
    assert_eq!(
        renamed, first,
        "same event under a renamed binding must converge on the first run"
    );
    // Same message arriving under another event/request ID (e.g. app_mention vs message.channels) converges.
    let other_delivery = take(
        &db,
        slack_accept("team", "C1.100.1.mention", user),
        Some(source(SourceKind::ThreadReply, "T1", "C1", "100.1", "Ev2")),
    )
    .await;
    assert_eq!(other_delivery, first);
    assert_eq!(rows(&db).await, (1, 1), "no extra run or request rows");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1,
        "deduplicated redelivery must not append audit"
    );
}

#[tokio::test]
async fn a_different_post_or_reaction_event_is_a_new_run_and_a_reaction_is_keyed_by_its_event_id() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let user = "slack:home/U1";
    let base = take(
        &db,
        slack_accept("team", "C1.100.1", user),
        Some(source(SourceKind::Message, "T1", "C1", "100.1", "Ev1")),
    )
    .await;
    for (n, (team, channel, ts)) in [
        ("T1", "C1", "100.2"),
        ("T1", "C2", "100.1"),
        ("T2", "C1", "100.1"),
    ]
    .into_iter()
    .enumerate()
    {
        let run = take(
            &db,
            slack_accept("team", &format!("{channel}.{ts}.{n}"), user),
            Some(source(
                SourceKind::Message,
                team,
                channel,
                ts,
                &format!("Evx{n}"),
            )),
        )
        .await;
        assert_ne!(run, base, "{team}/{channel}/{ts}");
    }
    let reaction = |event_id: &str| source(SourceKind::Reaction, "T1", "C1", "100.1", event_id);
    let first = take(
        &db,
        slack_accept("team", "Ev-R1", user),
        Some(reaction("Ev-R1")),
    )
    .await;
    let again = take(
        &db,
        slack_accept("team2", "Ev-R1", user),
        Some(reaction("Ev-R1")),
    )
    .await;
    assert_eq!(
        again, first,
        "same reaction event must converge across renamed bindings"
    );
    assert_ne!(first, base, "message and reaction keys are distinct");
    let second = take(
        &db,
        slack_accept("team", "Ev-R2", user),
        Some(reaction("Ev-R2")),
    )
    .await;
    assert_ne!(
        second, first,
        "distinct reaction event on same message starts a new run"
    );
}

#[tokio::test]
async fn without_a_slack_source_the_request_key_alone_decides_as_before() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let user = "slack:home/U1";
    let one = take(&db, slack_accept("team", "same", user), None).await;
    let two = take(&db, slack_accept("team2", "same", user), None).await;
    assert_ne!(one, two);
    let again = take(&db, slack_accept("team", "same", user), None).await;
    assert_eq!(again, one);
    let event = source(SourceKind::Message, "T1", "C1", "200.1", "Ev1");
    let keyed = take(
        &db,
        slack_accept("team", "C1.200.1", user),
        Some(event.clone()),
    )
    .await;
    let mut changed = slack_accept("team", "C1.200.9", user);
    changed.event.body = JsonPayload(r#"{"text":"other message"}"#.into());
    assert_eq!(take(&db, changed, Some(event)).await, keyed);
    assert_eq!(rows(&db).await.0, 3);
}

#[tokio::test]
async fn a_post_received_before_the_upgrade_is_not_received_again_after_it() {
    let Some(mut db) = TestDb::bare().await else {
        return;
    };
    db.publish().await;
    let user = "slack:home/U1";
    // Simulate legacy rows received before migration 0022 added `slack_key`.
    let channel_post = take(&db, slack_accept("team", "C1.100.1", user), None).await;
    let doubled = take(&db, slack_accept("team2", "C1.100.1", user), None).await;
    let mut dm = accept_as(
        "personal",
        "D1.200.1",
        user,
        ConversationKind::Direct,
        "D1",
        "question",
    );
    dm.event.binding = BindingId("direct".into());
    let dm_run = take(&db, dm, None).await;
    let cli = take(
        &db,
        accept_as(
            "personal",
            "C1.300.1",
            "local:owner",
            ConversationKind::Cli,
            "owner",
            "x",
        ),
        None,
    )
    .await;
    assert_ne!(
        channel_post, doubled,
        "pre-upgrade rows under renamed bindings created separate runs"
    );
    for sql in [
        "drop index bot_requests_slack_key",
        "alter table bot_requests drop column slack_key",
        "delete from _sqlx_migrations where version = 22",
    ] {
        db.sql(sql).await;
    }
    db.store
        .migrate()
        .await
        .expect("0022 migration should apply cleanly");
    db.store
        .migrate()
        .await
        .expect("re-running migrate must be idempotent");
    db.reconnect().await;

    // Backfill attaches temporary legacy keys only to the earliest Slack row per message, never CLI/HTTP.
    let keyed: Vec<(String, Option<String>)> = sqlx::query_as(
        "select q.request || '@' || q.binding, q.slack_key from bot_requests q order by q.created_ms, q.binding",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    let key_of = |what: &str| {
        keyed
            .iter()
            .find(|(name, _)| name == what)
            .unwrap_or_else(|| panic!("{what}: {keyed:?}"))
            .1
            .clone()
    };
    assert_eq!(key_of("C1.100.1@team"), Some("l\u{1f}C1.100.1".into()));
    assert_eq!(
        key_of("C1.100.1@team2"),
        None,
        "later duplicate row must not get a legacy key"
    );
    assert_eq!(key_of("D1.200.1@direct"), Some("l\u{1f}D1.200.1".into()));
    assert_eq!(
        key_of("C1.300.1@desk"),
        None,
        "CLI request must not get a Slack key"
    );
    let runs_before = db.scalar::<i64>("select count(*) from bot_runs").await;

    // Redelivery promotes the legacy key to the canonical event key without creating a new run.
    let post = |ts: &str, event_id: &str| source(SourceKind::Message, "T1", "C1", ts, event_id);
    let again = take(
        &db,
        slack_accept("team3", "C1.100.1", user),
        Some(post("100.1", "Ev1")),
    )
    .await;
    assert_eq!(again, channel_post);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs").await,
        runs_before
    );
    let adopted: Option<String> = sqlx::query_scalar(
        "select slack_key from bot_requests where binding = 'team' and request = 'C1.100.1'",
    )
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(adopted, Some(post("100.1", "Ev1").event_key()));
    let later = take(
        &db,
        slack_accept("team4", "C1.100.1.again", user),
        Some(source(SourceKind::ThreadReply, "T1", "C1", "100.1", "Ev2")),
    )
    .await;
    assert_eq!(later, channel_post);
    let mut dm_again = accept_as(
        "personal",
        "D1.200.1",
        user,
        ConversationKind::Direct,
        "D1",
        "question",
    );
    dm_again.event.binding = BindingId("direct2".into());
    let dm_source = source(SourceKind::Message, "T1", "D1", "200.1", "Ev3");
    assert_eq!(take(&db, dm_again, Some(dm_source)).await, dm_run);
    let other = take(
        &db,
        slack_accept("team", "C1.300.1", user),
        Some(source(SourceKind::Message, "T1", "C1", "300.1", "Ev4")),
    )
    .await;
    assert_ne!(other, cli);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs").await,
        runs_before + 1
    );
}

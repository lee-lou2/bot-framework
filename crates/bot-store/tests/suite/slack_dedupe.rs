//! Slack 이벤트 중복 제거: Slack 이벤트 한 건은 접수 하나다. 접수 키(프로필·바인딩·주체·요청)가 바뀌는 재전달 — 바인딩 이름이 바뀐 뒤, 같은 글이 `app_mention`과 `message`로 다른
//! 이벤트 ID·요청 ID로 오는 경우 — 도 어댑터가 인증한 이벤트 식별(글: 팀·채널·ts, 반응: 팀·event_id)로 첫 접수의 run으로 수렴한다.
//! 식별이 다른 이벤트·원천이 없는 접수(CLI·HTTP)는 기존 규칙 그대로다.

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

/// Slack 채널의 접수 명령: 바인딩·요청 ID는 호출마다 정한다.
fn slack_accept(binding: &str, request: &str, principal: &str) -> Accept {
    let mut a = accept_as(
        "personal",
        request,
        principal,
        ConversationKind::Channel,
        "C1",
        "묻는다",
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
    // 바인딩 이름이 바뀐 뒤의 재전달(접수 키가 달라진다)
    let renamed = take(
        &db,
        slack_accept("team2", "C1.100.1", user),
        Some(post("Ev1")),
    )
    .await;
    assert_eq!(
        renamed, first,
        "바인딩이 바뀌어도 같은 이벤트는 첫 run으로 수렴한다"
    );
    // 같은 글이 다른 이벤트 ID·다른 요청 ID로(`app_mention`과 `message.channels`) 오고 답글로 분류돼도 하나다
    let other_delivery = take(
        &db,
        slack_accept("team", "C1.100.1.mention", user),
        Some(source(SourceKind::ThreadReply, "T1", "C1", "100.1", "Ev2")),
    )
    .await;
    assert_eq!(other_delivery, first);
    assert_eq!(rows(&db).await, (1, 1), "run도 접수 행도 늘지 않는다");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1,
        "수렴한 재전달은 감사도 남기지 않는다"
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
    // 글이 다르다: ts·채널·팀 중 하나라도 다르면 다른 이벤트다
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
    // 반응: (팀, event_id)로 한 건이다. 같은 글에 달린 다른 반응 이벤트는 다른 접수다
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
    assert_eq!(again, first, "같은 반응 이벤트는 바인딩이 바뀌어도 하나다");
    assert_ne!(first, base, "글 접수와 반응 접수는 식별이 다르다");
    let second = take(
        &db,
        slack_accept("team", "Ev-R2", user),
        Some(reaction("Ev-R2")),
    )
    .await;
    assert_ne!(second, first, "같은 글에 달린 다른 반응 이벤트는 새 접수다");
}

#[tokio::test]
async fn without_a_slack_source_the_request_key_alone_decides_as_before() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let user = "slack:home/U1";
    // 원천이 없으면(CLI·HTTP·옛 접수) 바인딩이 다른 같은 요청 ID는 다른 접수다
    let one = take(&db, slack_accept("team", "same", user), None).await;
    let two = take(&db, slack_accept("team2", "same", user), None).await;
    assert_ne!(one, two);
    // 같은 키·같은 내용은 기존 run, 같은 키·다른 내용은 충돌이다(Slack 원천이 있어도 새 이벤트면 마찬가지)
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
    changed.event.body = JsonPayload(r#"{"text":"다른 글"}"#.into());
    // 같은 이벤트면 본문이 달라도(다른 접수 키) 첫 접수로 수렴한다
    assert_eq!(take(&db, changed, Some(event)).await, keyed);
    assert_eq!(rows(&db).await.0, 3);
}

/// 이 열이 생기기 전(구판)에 받은 접수 행: 마이그레이션이 같은 글의 가장 먼저 받은 행에 임시 키를 달고(CLI·HTTP 행은 건드리지 않는다), 그 글이 다시 오면
/// 새 접수를 만들지 않고 그 행을 정식 키로 채택해 그 run으로 수렴한다(바인딩 이름을 바꿔 다시 보내도 같다).
#[tokio::test]
async fn a_post_received_before_the_upgrade_is_not_received_again_after_it() {
    let Some(mut db) = TestDb::bare().await else {
        return;
    };
    db.publish().await;
    let user = "slack:home/U1";
    // 구판이 받은 행(원천이 없어 키가 비어 있다): 채널 글·같은 글을 바뀐 바인딩으로 다시 받은 행·DM 글, 그리고 Slack이 아닌 접수
    let channel_post = take(&db, slack_accept("team", "C1.100.1", user), None).await;
    let doubled = take(&db, slack_accept("team2", "C1.100.1", user), None).await;
    let mut dm = accept_as(
        "personal",
        "D1.200.1",
        user,
        ConversationKind::Direct,
        "D1",
        "질문",
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
        "구판에서는 바인딩이 바뀌면 같은 글이 두 번 접수됐다"
    );
    // 이 열이 없던 모양으로 되돌린다(색인·열 제거, 적용 기록 삭제)
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
        .expect("0022가 제자리에서 올라간다");
    db.store.migrate().await.expect("다시 돌려도 그대로다");
    db.reconnect().await;

    // 임시 키: 같은 글의 가장 먼저 받은 행과 DM 글에만 달린다. CLI 접수는 요청 ID가 비슷해 보여도 건드리지 않는다
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
        "같은 글의 뒤 행에는 달지 않는다"
    );
    assert_eq!(key_of("D1.200.1@direct"), Some("l\u{1f}D1.200.1".into()));
    assert_eq!(key_of("C1.300.1@desk"), None, "CLI 접수는 건드리지 않는다");
    let runs_before = db.scalar::<i64>("select count(*) from bot_runs").await;

    // 같은 글이 (바인딩 이름이 바뀐 채) 다시 온다: 새 접수 없이 가장 먼저 받은 구판 run으로 수렴하고 그 행이 정식 키를 얻는다
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
    // 채택한 뒤의 재전달(다른 전달 ID·분류)도 같은 run이다
    let later = take(
        &db,
        slack_accept("team4", "C1.100.1.again", user),
        Some(source(SourceKind::ThreadReply, "T1", "C1", "100.1", "Ev2")),
    )
    .await;
    assert_eq!(later, channel_post);
    // DM 글도 같다
    let mut dm_again = accept_as(
        "personal",
        "D1.200.1",
        user,
        ConversationKind::Direct,
        "D1",
        "질문",
    );
    dm_again.event.binding = BindingId("direct2".into());
    let dm_source = source(SourceKind::Message, "T1", "D1", "200.1", "Ev3");
    assert_eq!(take(&db, dm_again, Some(dm_source)).await, dm_run);
    // 요청 ID가 같아 보이는 CLI 접수는 다른 Slack 글을 막지 못한다
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

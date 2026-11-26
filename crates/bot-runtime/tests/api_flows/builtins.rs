use super::*;
use bot_core::{
    command::MemorySeed,
    id::{AgentId, ProfileId},
    port::{Error, MemoryStore, RunStore},
    schedule::{DeliveryTarget, ScheduleRoutes},
    state::{ConversationKind, Visibility},
};
use bot_runtime::{Delivery, Scheduler, SchedulerConfig, SchedulerDeps};

const SCOPES: [&str; 5] = [
    "run",
    "memory.share",
    "schedule.create",
    "schedule.list",
    "schedule.cancel",
];

/// 수신자가 등록된 다른 Slack 채널 하나인 시험 호스트.
struct Routes;

impl ScheduleRoutes for Routes {
    fn resolve(&self, _: &ProfileId, target: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        if target.binding.0 == "team"
            && target.conversation.kind == ConversationKind::Channel
            && target.conversation.id == "T1/C9"
        {
            Ok(target.clone())
        } else {
            Err(Error::Denied)
        }
    }
}

async fn builtin_world() -> Option<World> {
    let w = world_with_routes(
        Arc::new(bot_broker::testing::FakeSealer),
        Some(Arc::new(Routes)),
    )
    .await?;
    w.catalog.publish(
        bot_broker::testing::snapshot("personal", "r1")
            .role("owner", &SCOPES)
            .member(OWNER, &["owner"])
            .policy_scopes(&SCOPES)
            .slack_dm_binding("desk", "main")
            .slack_channel_binding("team", "main")
            .build(),
    );
    Some(w)
}

async fn accept(w: &World, request: &str, kind: ConversationKind) -> RunId {
    let mut cmd = command(request, "요청을 처리해 주세요");
    cmd.event.conversation.kind = kind;
    cmd.scopes = SCOPES.iter().map(|s| (*s).into()).collect();
    w.pg.store.accept(cmd).await.unwrap()
}

async fn pending(w: &World, tool: &str) -> (ApprovalId, String) {
    let (nonce, card, revision): (String, String, String) = sqlx::query_as(
        "select nonce, card, tool_revision from bot_approvals where tool = $1 and state = 'pending' and requester = $2 and approver = $2"
    ).bind(tool).bind(OWNER).fetch_one(w.pg.store.pool()).await.unwrap();
    assert_eq!(revision, "builtin-1");
    (ApprovalId(nonce), card)
}

async fn approve(w: &World, nonce: ApprovalId) {
    w.broker
        .decide(nonce, PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn builtin_listings_follow_the_agent_ceiling_for_both_dialects() {
    for kind in KINDS {
        let Some(w) = builtin_world().await else {
            return;
        };
        let api = FakeApi::start().await;
        api.script(text(kind, "확인했어요", None));
        accept(&w, "list", ConversationKind::Cli).await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        let wire = &api.seen()[0];
        assert_eq!(wire.tool_names().len(), 4, "{kind:?}");
        for tool in &SCOPES[1..] {
            wire.alias(tool);
        }
        assert!(
            !wire
                .tool_names()
                .iter()
                .any(|alias| alias.starts_with("schedule_deliver"))
        );
    }
}

#[tokio::test]
async fn both_dialects_share_only_the_owners_memory_after_private_approval() {
    for kind in KINDS {
        let Some(w) = builtin_world().await else {
            return;
        };
        let api = FakeApi::start().await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        for (i, (channel, owner)) in [
            (ConversationKind::Channel, OWNER),
            (ConversationKind::Direct, "local:other"),
            (ConversationKind::Direct, OWNER),
            (ConversationKind::Cli, OWNER),
        ]
        .into_iter()
        .enumerate()
        {
            let body = format!("소유자의 기억 {i}");
            w.pg.store
                .seed(MemorySeed {
                    profile: ProfileId("personal".into()),
                    agent: AgentId("main".into()),
                    owner: PrincipalId(owner.into()),
                    visibility: Visibility::Private,
                    text: body.clone(),
                    path: format!("m{i}.md"),
                    sha256: [1; 32],
                })
                .await
                .unwrap();
            let id: String = sqlx::query_scalar("select id from bot_memories where body = $1")
                .bind(&body)
                .fetch_one(w.pg.store.pool())
                .await
                .unwrap();
            api.script(calls(
                kind,
                "",
                vec![call("memory.share", json!({"id": id}))],
                None,
            ));
            api.script(text(kind, "확인했어요", None));
            let refused = channel == ConversationKind::Channel || owner != OWNER;
            let run = accept(&w, &format!("share-{i}"), channel).await;
            if refused {
                assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
                assert_eq!(view::marks(&w, &run).await[0].1, r#""denied""#);
                assert_eq!(
                    w.pg.scalar::<i64>("select count(*) from bot_approvals")
                        .await,
                    0
                );
            } else {
                assert_eq!(worker.run_once().await, Some(Outcome::Parked), "{kind:?}");
                let (nonce, card) = pending(&w, "memory.share").await;
                assert!(card.contains(&body) && card.contains(&id));
                assert!(matches!(
                    w.broker
                        .decide(nonce.clone(), PrincipalId("local:other".into()), true)
                        .await,
                    Err(Error::Denied)
                ));
                assert_eq!(
                    w.pg.scalar::<String>(&format!(
                        "select visibility from bot_memories where id = '{id}'"
                    ))
                    .await,
                    "private"
                );
                approve(&w, nonce).await;
                assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
            }
            let visibility: String =
                sqlx::query_scalar("select visibility from bot_memories where id = $1")
                    .bind(&id)
                    .fetch_one(w.pg.store.pool())
                    .await
                    .unwrap();
            assert_eq!(visibility, if refused { "private" } else { "profile" });
        }
        assert!(w.runner.jobs().is_empty());
    }
}

#[tokio::test]
async fn both_dialects_approve_the_destination_then_each_draft_and_can_list_and_cancel() {
    for kind in KINDS {
        let Some(w) = builtin_world().await else {
            return;
        };
        let api = FakeApi::start().await;
        let worker = w
            .worker(&w.pg.store, models(kind, &api), config())
            .with_delivery(Delivery {
                replies: w.pg.store.clone(),
                release: w.broker.clone(),
            });
        let instruction = "팀에 마감 건수 알리기";
        let invalid = Call {
            id: "bad-list",
            ..call("schedule.list", json!({"typo": 1}))
        };
        let create = call(
            "schedule.create",
            json!({
                "cron": "* * * * *", "instruction": instruction,
                "destination": {"binding": "team", "conversation": {"kind": "channel", "id": "T1/C9"}}
            }),
        );
        api.script(calls(kind, "", vec![invalid, create], None));
        api.script(text(kind, "예약했어요", None));
        accept(&w, "create", ConversationKind::Direct).await;
        assert_eq!(worker.run_once().await, Some(Outcome::Parked), "{kind:?}");
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_user_schedules")
                .await,
            0
        );
        let (nonce, card) = pending(&w, "schedule.create").await;
        assert!(card.contains("T1/C9") && card.contains(instruction));
        approve(&w, nonce).await;
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        let id: String =
            w.pg.scalar("select id from bot_user_schedules where state = 'active'")
                .await;

        let invalid = "select count(*) from bot_audit where action = 'call.invalid' and target = 'schedule.list'";
        assert_eq!(w.pg.scalar::<i64>(invalid).await, 1);
        let scheduler = Scheduler::new(
            SchedulerDeps {
                schedule: w.pg.store.clone(),
                user: w.pg.store.clone(),
                catalog: w.catalog.clone(),
                tracer: w.spans.clone(),
            },
            SchedulerConfig::default(),
        )
        .with_routes(Arc::new(Routes));
        let minute: i64 =
            w.pg.scalar("select cursor_ms / 60000 from bot_user_schedules")
                .await;
        let sent = "select count(*) from bot_outbox where conv_id = 'T1/C9'";
        for i in 1..=2 {
            // 이 스키마의 DB 시계로 실제 cron 경계를 한 분씩 지난다.
            let now = (minute + i) * 60000;
            w.pg.sql(&format!("create or replace function bot_now_ms() returns bigint language sql volatile as 'select {now}::bigint'")).await;
            assert_eq!(scheduler.tick_users().await.unwrap().fired, 1);
            let draft = format!("{i}회차에 보낼 팀 요약");
            api.script(text(kind, &draft, None));
            assert_eq!(worker.run_once().await, Some(Outcome::Parked));
            assert_eq!(w.pg.scalar::<i64>(sent).await, i - 1);
            let (nonce, card) = pending(&w, "schedule.deliver").await;
            assert!(card.contains("T1/C9") && card.contains(&draft));
            approve(&w, nonce).await;
            let before = api.seen().len();
            assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
            assert_eq!(api.seen().len(), before, "승인 후에는 동결한 초안을 쓴다");
            assert_eq!(w.pg.scalar::<i64>(sent).await, i);
            let body: String = w.pg.scalar("select body from bot_outbox where conv_id = 'T1/C9' order by created_ms desc limit 1").await;
            assert!(body.contains(&draft));
        }
        let before = api.seen().len();
        api.script(calls(
            kind,
            "",
            vec![call("schedule.list", json!({}))],
            None,
        ));
        api.script(calls(
            kind,
            "",
            vec![call("schedule.cancel", json!({"id":id}))],
            None,
        ));
        api.script(calls(
            kind,
            "",
            vec![call("schedule.list", json!({}))],
            None,
        ));
        api.script(text(kind, "취소했어요", None));
        accept(&w, "list-cancel", ConversationKind::Direct).await;
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        let seen = api.seen();
        assert!(
            seen[before + 1].json()["messages"]
                .to_string()
                .contains(&id)
        );
        let last = seen.last().unwrap().json();
        let content = &last["messages"].as_array().unwrap().last().unwrap()["content"];
        let result = content
            .as_str()
            .or_else(|| content[0]["content"].as_str())
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result).unwrap(),
            json!({"schedules":[]})
        );
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_user_schedules where state = 'active'")
                .await,
            0
        );
        assert!(w.runner.jobs().is_empty());
    }
}

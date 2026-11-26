#[path = "outbox/routing.rs"]
mod routing;
mod support;

use bot_core::{command::*, id::*, intake::*, port::*, profile::ActiveRevisions, state::*};
use std::{collections::BTreeMap, time::Duration};
use support::*;

const SLACK: DeliveryScope = DeliveryScope::all(PushTransportId::SLACK);

// Store는 RunStore와 Outbox를 모두 구현해 `claim`이 겹친다. 시험은 트레이트 객체로 경계를 그대로 쓴다.
fn runs(db: &TestDb) -> &dyn RunStore {
    &db.store
}

fn ob(db: &TestDb) -> &dyn Outbox {
    &db.store
}

fn rev(s: &str) -> Revision {
    Revision(s.into())
}

/// Slack DM 회신 두 건이 쌓인 run을 만든다. 활성 판본은 personal=r1(시험 저장소가 미리 게시한다).
async fn two_replies(db: &TestDb) -> RunId {
    let run = runs(db)
        .accept(accept_as(
            "personal",
            "r",
            "slack:home/U1",
            ConversationKind::Direct,
            "D1",
            "hi",
        ))
        .await
        .unwrap();
    let got = runs(db).claim().await.unwrap().unwrap();
    let finish = Finish {
        result: JsonPayload("{}".into()),
        answers: vec![
            JsonPayload(r#"{"text":"첫째"}"#.into()),
            JsonPayload(r#"{"text":"둘째"}"#.into()),
        ],
    };
    runs(db).finish(got.attempt, finish).await.unwrap();
    run
}

async fn state_of(db: &TestDb, seq: i32) -> String {
    db.scalar::<String>(&format!("select state from bot_outbox where seq = {seq}"))
        .await
}

#[tokio::test]
async fn claims_in_order_and_sends_only_after_begin_commits() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = two_replies(&db).await;
    assert!(
        ob(&db)
            .claim(
                &DeliveryScope::all(PushTransportId::new("test-push").unwrap()),
                10
            )
            .await
            .unwrap()
            .is_empty(),
        "자기 어댑터의 회신만 점유한다"
    );
    let sent = ob(&db).claim(&SLACK, 10).await.unwrap();
    assert_eq!(
        sent.len(),
        1,
        "앞선 회신이 나가기 전에는 뒤의 것을 잡지 않는다"
    );
    let first = &sent[0];
    assert_eq!((first.key.seq, first.key.generation), (0, Generation(1)));
    assert_eq!(first.call.0, format!("reply:{}:0", run.0));
    assert_eq!(
        (
            first.binding.0.as_str(),
            first.principal.0.as_str(),
            first.destination.id.as_str()
        ),
        ("desk", "slack:home/U1", "D1")
    );
    assert_eq!(first.body.0, r#"{"text":"첫째"}"#);
    assert!(
        ob(&db).claim(&SLACK, 10).await.unwrap().is_empty(),
        "점유 중인 건은 다시 잡히지 않는다"
    );

    // begin: 활성 판본과 같아야 하고, 최초 호출만 Ok다
    assert!(
        matches!(
            ob(&db).begin(first.key.clone(), rev("r0")).await,
            Err(Error::Stale)
        ),
        "판본이 달라졌다"
    );
    ob(&db).begin(first.key.clone(), rev("r1")).await.unwrap();
    assert_eq!(state_of(&db, 0).await, "sending");
    assert!(
        matches!(
            ob(&db).begin(first.key.clone(), rev("r1")).await,
            Err(Error::Conflict)
        ),
        "재호출은 보낼 수 없다"
    );
    assert!(
        ob(&db).claim(&SLACK, 10).await.unwrap().is_empty(),
        "sending 건은 자동 claim에서 빠진다"
    );

    ob(&db)
        .settle(
            first.key.clone(),
            DeliveryOutcome::Confirmed {
                receipt: "1700.0001".into(),
            },
        )
        .await
        .unwrap();
    ob(&db)
        .settle(
            first.key.clone(),
            DeliveryOutcome::Confirmed {
                receipt: "1700.0001".into(),
            },
        )
        .await
        .unwrap();
    let other = ob(&db)
        .settle(
            first.key.clone(),
            DeliveryOutcome::Confirmed {
                receipt: "다른 영수증".into(),
            },
        )
        .await;
    assert!(
        matches!(other, Err(Error::Conflict)),
        "정산된 영수증은 바뀌지 않는다"
    );
    assert_eq!(
        db.scalar::<String>("select receipt from bot_outbox where seq = 0")
            .await,
        "1700.0001"
    );

    let next = ob(&db).claim(&SLACK, 10).await.unwrap();
    assert_eq!(next[0].key.seq, 1, "앞의 건이 끝나면 다음 건을 잡는다");
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn lease_expiry_before_begin_reclaims_with_a_new_delivery_generation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    two_replies(&db).await;
    let old = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0);
    db.sql("update bot_outbox set lease_until_ms = bot_now_ms() - 1 where seq = 0")
        .await;
    let fresh = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0);
    assert_eq!((fresh.key.seq, fresh.key.generation), (0, Generation(2)));
    assert_eq!(fresh.call, old.call, "같은 호출·같은 순번");
    assert!(
        matches!(
            ob(&db).begin(old.key.clone(), rev("r1")).await,
            Err(Error::Stale)
        ),
        "옛 점유자는 보낼 수 없다"
    );
    ob(&db).begin(fresh.key.clone(), rev("r1")).await.unwrap();
}

#[tokio::test]
async fn lost_response_after_begin_is_held_and_late_evidence_confirms_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    two_replies(&db).await;
    let sent = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0);
    ob(&db).begin(sent.key.clone(), rev("r1")).await.unwrap();
    db.sql("update bot_outbox set lease_until_ms = bot_now_ms() - 1 where seq = 0")
        .await;
    // 불명 건은 자동으로 다시 보내지 않는다. 보류(held)는 순서를 막지 않으므로 다음 건(seq 1)만 점유된다
    let next = ob(&db).claim(&SLACK, 10).await.unwrap();
    assert_eq!(next.iter().map(|o| o.key.seq).collect::<Vec<_>>(), [1]);
    assert_eq!(state_of(&db, 0).await, "held");
    assert_eq!(
        db.scalar::<String>("select reason from bot_outbox where seq = 0")
            .await,
        "owner_lost"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'outbox.held'")
            .await,
        1
    );

    ob(&db)
        .settle(
            sent.key,
            DeliveryOutcome::Confirmed {
                receipt: "1700.0002".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 0).await, "confirmed");
}

#[tokio::test]
async fn not_sent_failures_retry_with_the_longer_of_backoff_and_retry_after_then_hold() {
    let cfg = bot_store::Config {
        max_attempts: 3,
        backoff_base: Duration::from_secs(10),
        ..Default::default()
    };
    let Some(db) = TestDb::with(cfg).await else {
        return;
    };
    two_replies(&db).await;
    let mut key = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0).key;
    ob(&db).begin(key.clone(), rev("r1")).await.unwrap();
    ob(&db)
        .settle(
            key.clone(),
            DeliveryOutcome::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 0).await, "pending");
    let wait = db
        .scalar::<i64>("select not_before_ms - bot_now_ms() from bot_outbox where seq = 0")
        .await;
    assert!(
        (7_000..=12_600).contains(&wait),
        "백오프 약 10초 ±25%: {wait}"
    );
    assert!(
        ob(&db).claim(&SLACK, 10).await.unwrap().is_empty(),
        "대기 시간 전에는 잡지 않는다"
    );

    // 수신처가 더 긴 대기를 요구하면 그쪽이 하한이다
    db.sql("update bot_outbox set not_before_ms = 0 where seq = 0")
        .await;
    key = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0).key;
    assert_eq!(key.generation, Generation(2), "새 발송 세대, 같은 순번");
    ob(&db).begin(key.clone(), rev("r1")).await.unwrap();
    let retry_after = Some(Duration::from_secs(120));
    ob(&db)
        .settle(
            key.clone(),
            DeliveryOutcome::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after,
            },
        )
        .await
        .unwrap();
    let wait = db
        .scalar::<i64>("select not_before_ms - bot_now_ms() from bot_outbox where seq = 0")
        .await;
    assert!(wait >= 119_000, "Retry-After가 백오프보다 길다: {wait}");

    // 재시도를 다 쓰면 운영자에게 최종 실패(보류)로 보인다
    db.sql("update bot_outbox set not_before_ms = 0 where seq = 0")
        .await;
    key = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0).key;
    ob(&db).begin(key.clone(), rev("r1")).await.unwrap();
    ob(&db)
        .settle(
            key,
            DeliveryOutcome::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 0).await, "held");
    assert_eq!(
        db.scalar::<String>("select reason from bot_outbox where seq = 0")
            .await,
        "retries_exhausted"
    );
}

#[tokio::test]
async fn stale_and_unsent_settlements_change_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    two_replies(&db).await;
    let key = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0).key;
    // begin 전의 settle은 받지 않는다
    let early = ob(&db).settle(key.clone(), DeliveryOutcome::Rejected).await;
    assert!(matches!(early, Err(Error::Stale)));
    ob(&db).begin(key.clone(), rev("r1")).await.unwrap();
    let old = DeliveryKey {
        generation: Generation(99),
        ..key.clone()
    };
    assert!(matches!(
        ob(&db).settle(old, DeliveryOutcome::Rejected).await,
        Err(Error::Stale)
    ));
    assert_eq!(state_of(&db, 0).await, "sending");
    ob(&db)
        .settle(key.clone(), DeliveryOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(state_of(&db, 0).await, "rejected");
    assert!(
        matches!(
            ob(&db).settle(key.clone(), DeliveryOutcome::Held).await,
            Err(Error::Stale)
        ),
        "확정 거절은 최종이다"
    );
    let missing = DeliveryKey { seq: 7, ..key };
    assert!(matches!(
        ob(&db).settle(missing, DeliveryOutcome::Rejected).await,
        Err(Error::Missing)
    ));
}

#[tokio::test]
async fn publishing_a_new_active_revision_invalidates_a_checked_begin() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    two_replies(&db).await;
    let key = ob(&db).claim(&SLACK, 1).await.unwrap().remove(0).key;
    // 게이트웨이가 r1로 자격을 검사한 뒤 판본이 r2로 교체되면 begin은 Stale이다
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            BTreeMap::from([(ProfileId("personal".into()), rev("r2"))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        ob(&db).begin(key.clone(), rev("r1")).await,
        Err(Error::Stale)
    ));
    ob(&db).begin(key, rev("r2")).await.unwrap();
}

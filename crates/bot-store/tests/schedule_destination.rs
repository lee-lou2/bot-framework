//! 명시 목적지(다른 수신자) 예약: 승인한 호출의 명세로만 만들고, 발화할 때마다 목적지를 그 run에 고정한다.

mod support;

use bot_core::{id::*, ledger::*, port::*, schedule::*, state::*};
use support::{schedule::*, *};

const OWNER: &str = "local:owner";
/// 슬랙의 `팀ID/채널ID` 꼴: 예약을 만들 때부터 발화·고정까지 바뀌지 않는다
const DEST: &str = "T123/C456";

fn target(kind: ConversationKind) -> DeliveryTarget {
    DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind,
            id: DEST.into(),
            thread: Some("t1".into()),
        },
    }
}

#[tokio::test]
async fn a_destination_schedule_needs_the_approved_call_and_each_firing_fixes_the_destination() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    // 푸시 대화(direct·channel)가 아닌 목적지는 DB를 만지기 전에 거절한다
    for kind in [
        ConversationKind::Http,
        ConversationKind::Cli,
        ConversationKind::Scheduled,
    ] {
        let mut spec = spec_in(&db, 60_000).await;
        spec.destination = Some(target(kind));
        assert!(matches!(
            make(&db, &run, "bad", spec).await,
            Err(Error::Invalid)
        ));
    }
    let mut spec = spec_in(&db, 3_600_000).await;
    spec.destination = Some(target(ConversationKind::Direct));
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    let for_create = |attempt: &Attempt| Approval {
        tool: tool("schedule.create"),
        args_sha256: hash,
        ..binding(attempt, "c1", "n1")
    };
    // 수신자를 지정한 생성 카드는 확인한 등록부의 집합을 고정한 핸들로만 열린다
    let pinned = db.store.for_activation(Revision("set-1".into()));
    CallLedger::open_approval(&pinned, open_create(for_create(&run.attempt), &spec))
        .await
        .unwrap();
    calls(&db)
        .decide(Decision {
            nonce: ApprovalId("n1".into()),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let approval = || {
        Some((
            for_create(&resumed.attempt),
            vec!["irreversible".to_owned()],
        ))
    };
    // 승인한 호출이 아니면 만들지 않고, 승인한 명세와 다른 목적지로도 만들지 않는다
    assert!(matches!(
        users(&db)
            .create(start_call(&resumed.attempt, "c1", None), spec.clone())
            .await,
        Err(Error::Denied)
    ));
    let mut elsewhere = spec.clone();
    elsewhere.destination = Some(target(ConversationKind::Channel));
    assert!(matches!(
        users(&db)
            .create(start_call(&resumed.attempt, "c1", approval()), elsewhere)
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );
    let id = users(&db)
        .create(start_call(&resumed.attempt, "c1", approval()), spec)
        .await
        .unwrap();
    assert_eq!(column::<String>(&db, &id, "dest_binding").await, "slack");
    assert_eq!(column::<String>(&db, &id, "dest_kind").await, "direct");
    assert_eq!(column::<String>(&db, &id, "dest_id").await, DEST);

    // 발화: 접수와 같은 커밋에서 목적지를 run에 고정한다(원래 대화는 그대로다)
    make_due(&db, &id, 5_000).await;
    let at_ms = column::<i64>(&db, &id, "at_ms").await;
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let fired = users(&db)
        .advance(AdvanceSchedule {
            profile: profile_id(),
            id: id.clone(),
            expected_cursor: at(cursor),
            checked_revision: Some(Revision("r1".into())),
            next: NextOccurrence::Fire {
                at: at(at_ms),
                next: None,
                scopes: run_scopes(),
            },
        })
        .await
        .unwrap()
        .expect("발화한 run");
    let row: (String, String, String, Option<String>, String) = sqlx::query_as(
        "select dest_binding, dest_kind, dest_id, dest_thread, state from bot_deliveries where run = $1",
    )
    .bind(&fired.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "slack".into(),
            "direct".into(),
            DEST.into(),
            Some("t1".into()),
            "pending".into()
        )
    );
    assert_eq!(
        db.scalar::<String>(&format!(
            "select conv_id from bot_runs where id = '{}'",
            fired.0
        ))
        .await,
        "c1",
        "원래 대화에서 발화한다"
    );
    // 이미 접수한 실행의 목적지는 예약이 바뀌어도 그대로다
    db.sql("update bot_user_schedules set dest_id = 'D1'").await;
    assert_eq!(
        db.scalar::<String>("select dest_id from bot_deliveries")
            .await,
        DEST
    );
}

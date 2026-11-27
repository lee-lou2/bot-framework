//! 민감 DM 전용 원장의 리스 만료(PG): 만료된 접수는 시작 여부와 관계없이 내구 보류다.
//! 결과 기록이 재전달보다 먼저 와도, 재전달이 먼저 와도 같은 상태가 되고, 보류에서 반납·거절로 되돌리지 않는다.

use crate::support::{private::*, *};
use bot_core::{id::*, ledger::*, port::*};

#[tokio::test]
async fn an_expired_lease_is_held_whether_or_not_it_started_and_is_never_rerun() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 시작 전 만료: 단순 재점유가 아니라 보류다
    let at = reserve(&db, "a", 1).await;
    db.expire_lease(&at.run).await;
    assert!(matches!(
        ledger(&db).begin(request("a", 1)).await.unwrap(),
        PrivatePermit::Held
    ));
    assert_eq!(row(&db).await, "waiting:hold:1:1:false");
    for outcome in [
        PrivateOutcome::Completed,
        PrivateOutcome::NotSent,
        PrivateOutcome::Rejected,
        PrivateOutcome::RetryBeforeStart,
    ] {
        assert!(
            matches!(
                ledger(&db).finish(at.clone(), outcome).await,
                Err(Error::Conflict)
            ),
            "시작이 확인되지 않은 보류에서 늦게 오는 결과는 받지 않는다"
        );
    }
    assert!(matches!(
        ledger(&db).start(start_of(&at, 1)).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.held'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_started_row_that_lost_its_lease_is_swept_without_redelivery() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = started(&db, "a", 1).await;
    db.expire_lease(&at.run).await;
    let viewer = bot_core::admin::Viewer {
        principal: PrincipalId("local:owner".into()),
        operator: true,
    };
    let holds = bot_core::admin::AdminRead::holds(&db.store, &viewer, None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1, "재전달 없이도 보류로 보인다");
    assert_eq!(holds[0].call.0, format!("private:{}", at.run.0));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.held'")
            .await,
        1
    );
    assert_eq!(row(&db).await, "waiting:hold:1:1:true");
    assert!(matches!(
        ledger(&db).begin(request("a", 1)).await.unwrap(),
        PrivatePermit::Held
    ));
    // 실제 시작이 확인된 행의 늦은 성공만 받는다
    ledger(&db)
        .finish(at, PrivateOutcome::Completed)
        .await
        .unwrap();
    assert_eq!(row(&db).await, "succeeded:-:1:1:true");
}

/// 리스가 끝난 접수에 대해 결과 기록이 먼저 오든 재전달이 먼저 오든 같은 상태(보류)가 된다.
async fn expired_case(db: &TestDb, name: &str, fp: u8, started_first: bool) -> Attempt {
    let at = if started_first {
        started(db, name, fp).await
    } else {
        reserve(db, name, fp).await
    };
    db.expire_lease(&at.run).await;
    at
}

type Case = (&'static str, bool, fn() -> PrivateOutcome);

fn outcomes() -> [Case; 6] {
    [
        ("retry", false, || PrivateOutcome::RetryBeforeStart),
        ("not-sent", true, || PrivateOutcome::NotSent),
        ("rejected-before", false, || PrivateOutcome::Rejected),
        ("rejected-after", true, || PrivateOutcome::Rejected),
        ("completed-unstarted", false, || PrivateOutcome::Completed),
        ("held", true, || PrivateOutcome::Held),
    ]
}

#[tokio::test]
async fn a_result_arriving_after_lease_expiry_before_any_redelivery_holds_instead_of_releasing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (i, (name, started_first, outcome)) in outcomes().into_iter().enumerate() {
        let fp = i as u8 + 1;
        let at = expired_case(&db, name, fp, started_first).await;
        let held_before = db
            .scalar::<i64>("select count(*) from bot_audit where action = 'private.held'")
            .await;
        let result = ledger(&db).finish(at.clone(), outcome()).await;
        if name == "held" {
            assert!(result.is_ok(), "{name}: 보류 보고는 받는다");
        } else {
            assert!(
                matches!(result, Err(Error::Conflict)),
                "{name}: 만료 뒤 반납·거절·미시작 완료는 거절"
            );
        }
        assert_eq!(
            db.scalar::<String>(&format!(
                "select state || ':' || wait_kind from bot_runs where id = '{}'",
                at.run.0
            ))
            .await,
            "waiting:hold",
            "{name}: 내구 보류 상태"
        );
        assert_eq!(
            db.scalar::<i64>("select count(*) from bot_audit where action = 'private.held'")
                .await,
            held_before + 1,
            "{name}: 보류 감사 한 줄"
        );
        // 그 뒤의 재전달은 새 Start가 아니라 Held다
        assert!(
            matches!(
                ledger(&db).begin(request(name, fp)).await.unwrap(),
                PrivatePermit::Held
            ),
            "{name}: 재전달은 Held"
        );
    }
    assert!(
        db.scalar::<i64>("select count(*) from bot_runs where state = 'ready'")
            .await
            == 0,
        "반납된 행이 없다"
    );
}

#[tokio::test]
async fn the_redelivery_first_order_gives_the_same_hold_and_rejects_the_late_result() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (i, (name, started_first, outcome)) in outcomes().into_iter().enumerate() {
        let fp = i as u8 + 1;
        let at = expired_case(&db, name, fp, started_first).await;
        assert!(
            matches!(
                ledger(&db).begin(request(name, fp)).await.unwrap(),
                PrivatePermit::Held
            ),
            "{name}: 재전달이 먼저면 Held"
        );
        assert!(
            matches!(
                ledger(&db).finish(at.clone(), outcome()).await,
                Err(Error::Conflict)
            ),
            "{name}: 그 뒤의 결과 기록은 거절"
        );
        assert_eq!(
            db.scalar::<String>(&format!(
                "select state || ':' || wait_kind from bot_runs where id = '{}'",
                at.run.0
            ))
            .await,
            "waiting:hold"
        );
    }
}

#[tokio::test]
async fn a_late_success_is_accepted_after_expiry_only_when_the_start_was_recorded() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 시작이 기록된 행: 만료 뒤 결과가 먼저 와도, 재전달·청소로 보류된 뒤에 와도 완료로 받는다
    let first = expired_case(&db, "a", 1, true).await;
    ledger(&db)
        .finish(first.clone(), PrivateOutcome::Completed)
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<String>(&format!(
            "select state from bot_runs where id = '{}'",
            first.run.0
        ))
        .await,
        "succeeded"
    );
    let second = expired_case(&db, "b", 2, true).await;
    assert!(matches!(
        ledger(&db).begin(request("b", 2)).await.unwrap(),
        PrivatePermit::Held
    ));
    ledger(&db)
        .finish(second, PrivateOutcome::Completed)
        .await
        .unwrap();
    // 시작이 기록되지 않은 행은 늦은 성공도 받지 않는다
    let third = expired_case(&db, "c", 3, false).await;
    assert!(matches!(
        ledger(&db).finish(third, PrivateOutcome::Completed).await,
        Err(Error::Conflict)
    ));
}

//! 호출 원장의 소유자 확인에서 run ID는 프로필마다 따로 쓰인다: 같은 run·call ID를 두 프로필이 쓰면 자기 프로필의 낡은 claim은 어느 쪽 행을 먼저 읽었든 `Stale`이고,
//! 남의 프로필에만 있는 run은 `Denied`, 어디에도 없으면 `Missing`이다.

const OWNER: &str = "local:owner";

use crate::support::*;
use bot_core::{id::*, port::Error};

/// 자기 프로필(work)의 run과 같은 ID를 남의 프로필(personal)에도 만든다(열은 스키마에서 읽어 그대로 복사하고 프로필만 바꾼다).
/// 남의 프로필 이름이 사전순으로 앞서고(인덱스 순서) 힙에서도 앞서게(자기 행을 한 번 갱신해 새 버전을 뒤로 보낸다) 해서, run ID만으로 아무 행이나 고르던 옛 분류가
/// 어떤 계획(순차 읽기·인덱스 읽기)으로 읽어도 남의 행을 먼저 집도록 한다.
async fn same_id_in_the_other_profile(db: &TestDb, run: &str) {
    let columns: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns
         where table_schema = current_schema() and table_name = 'bot_runs' order by ordinal_position",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    let list = columns.join(", ");
    let values = columns
        .iter()
        .map(|c| {
            if c == "profile" {
                "'personal'"
            } else {
                c.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    db.sql(&format!(
        "insert into bot_runs ({list}) select {values} from bot_runs where profile = 'work' and id = '{run}'"
    ))
    .await;
    db.sql(&format!(
        "update bot_runs set updated_ms = updated_ms where profile = 'work' and id = '{run}'"
    ))
    .await;
    for plan in [
        "set enable_indexscan = off; set enable_bitmapscan = off",
        "set enable_seqscan = off",
    ] {
        let mut conn = db.raw().await;
        for statement in plan.split(';') {
            sqlx::query(statement.trim())
                .execute(&mut conn)
                .await
                .unwrap();
        }
        let first: String = sqlx::query_scalar(&format!(
            "select profile from bot_runs where id = '{run}' limit 1"
        ))
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(first, "personal", "{plan}: 남의 프로필 행이 먼저 읽힌다");
    }
}

#[tokio::test]
async fn a_stale_claim_stays_stale_when_another_profile_has_the_same_run_id() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 자기 프로필은 `work`다
    runs(&db)
        .accept(accept_as(
            "work",
            "r1",
            OWNER,
            bot_core::state::ConversationKind::Direct,
            "c1",
            "hi",
        ))
        .await
        .unwrap();
    let run = runs(&db).claim().await.unwrap().expect("집는다");
    assert_eq!(run.attempt.profile.0, "work");
    same_id_in_the_other_profile(&db, &run.attempt.run.0).await;
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_runs where id = '{}'",
            run.attempt.run.0
        ))
        .await,
        2,
        "두 프로필에 같은 run ID가 있다"
    );
    // 낡은 claim(리스 만료 뒤 다른 워커가 집었다): 자기 프로필의 run이 있으므로 어느 행을 먼저 읽든 Stale이다
    db.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where profile = 'work' and id = '{}'",
        run.attempt.run.0
    ))
    .await;
    let taken = runs(&db).claim().await.unwrap().expect("인계한다");
    assert_eq!(taken.attempt.profile.0, "work");
    assert_ne!(taken.attempt.claim, run.attempt.claim);
    for _ in 0..5 {
        let got = calls(&db)
            .begin(
                run.attempt.clone(),
                call("c1"),
                tool("agenda.list"),
                [1; 32],
            )
            .await;
        assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    }
    // 같은 run ID가 남의 프로필에만 있으면 Denied
    let foreign = Attempt {
        profile: ProfileId("other".into()),
        ..run.attempt.clone()
    };
    let got = calls(&db)
        .begin(foreign, call("c1"), tool("agenda.list"), [1; 32])
        .await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    // 어느 프로필에도 없으면 Missing
    let ghost = Attempt {
        run: RunId("ghost".into()),
        ..run.attempt.clone()
    };
    let got = calls(&db)
        .begin(ghost, call("c1"), tool("agenda.list"), [1; 32])
        .await;
    assert!(matches!(got, Err(Error::Missing)), "{got:?}");
}

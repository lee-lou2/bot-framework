mod support;

use bot_core::{command::*, id::*, port::*};
use support::*;

async fn some_traffic(db: &TestDb, n: usize) {
    for i in 0..n {
        db.store
            .accept(accept("personal", &format!("r{i}"), "hi"))
            .await
            .unwrap();
    }
    let got = db.store.claim().await.unwrap().unwrap();
    db.store.finish(got.attempt, done("ok")).await.unwrap();
}

fn profile() -> ProfileId {
    ProfileId("personal".into())
}

#[tokio::test]
async fn audit_chain_links_every_state_change() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let initial = db.store.verify().await.unwrap().entries;
    some_traffic(&db, 3).await;
    let report = db.store.verify().await.unwrap();
    assert_eq!(
        (report.entries, report.first_bad),
        (initial + 4, None),
        "접수 3건 + 성공 1건"
    );
}

#[tokio::test]
async fn audit_chain_detects_content_tampering() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    db.sql("update bot_audit set verdict = 'forged' where seq = 2")
        .await;
    assert_eq!(db.store.verify().await.unwrap().first_bad, Some(2));
}

#[tokio::test]
async fn audit_chain_detects_a_rewritten_previous_hash_column_even_if_the_row_hash_is_intact() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    // 행의 내용과 `hash`는 그대로이고 `prev_hash` 열만 다른 값으로 고쳐 쓴다: 해시 계산은 직전 행의 실제 해시를 쓰므로 `hash`만 비교하면 놓친다
    db.sql("update bot_audit set prev_hash = decode(repeat('ab', 32), 'hex') where seq = 3")
        .await;
    assert_eq!(db.store.verify().await.unwrap().first_bad, Some(3));
}

#[tokio::test]
async fn audit_chain_detects_removed_rows_and_truncated_tail() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    let total = db.store.verify().await.unwrap().entries;
    db.sql("delete from bot_audit where seq = 2").await;
    assert_eq!(
        db.store.verify().await.unwrap().first_bad,
        Some(2),
        "순번 공백"
    );

    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 3).await;
    db.sql(&format!("delete from bot_audit where seq = {total}"))
        .await;
    assert_eq!(
        db.store.verify().await.unwrap().first_bad,
        Some(total),
        "꼬리 삭제는 head와 어긋난다"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_changes_keep_one_contiguous_chain() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let initial = db.store.verify().await.unwrap().entries;
    let mut tasks = Vec::new();
    for n in 0..16 {
        let store = db.peer(&format!("w{n}")).await;
        tasks.push(tokio::spawn(async move {
            store
                .accept(accept("personal", &format!("r{n}"), "hi"))
                .await
                .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let report = db.store.verify().await.unwrap();
    assert_eq!((report.entries, report.first_bad), (initial + 16, None));
}

#[tokio::test]
async fn standalone_append_extends_the_same_chain() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 1).await;
    let before = db.store.verify().await.unwrap().entries;
    let entry = AuditEntry {
        profile: profile(),
        actor: "broker".into(),
        action: "call.denied".into(),
        target: "c-1".into(),
        args_sha256: Some([7; 32]),
        verdict: "scope".into(),
    };
    assert_eq!(db.store.append(entry).await.unwrap(), before + 1);
    let report = db.store.verify().await.unwrap();
    assert_eq!((report.entries, report.first_bad), (before + 1, None));
}

/// 다른 연결이 쉬지 않고 한 줄씩 잇는 동안 검증을 반복한다. 행(쪽마다)과 head를 서로 다른 스냅숏에서 읽으면 검증이 도는 사이에 이어 붙은 항목이
/// head에만(또는 마지막 쪽 뒤에만) 보여 변조가 없는데도 «변조»로 나온다 — 한 `REPEATABLE READ` 스냅숏이면 한 번도 나오지 않는다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verifying_while_entries_are_appended_never_reports_tampering_that_did_not_happen() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 30).await;
    let writer_store = db.peer("appender").await;
    let (stop, appended) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
    );
    let writer = {
        let (stop, appended) = (stop.clone(), appended.clone());
        tokio::spawn(async move {
            // 검증보다 느리게(체인이 한없이 길어져 검증이 끝나지 않는 일이 없게) 하지만 검증 한 번 사이에 여러 줄이 이어 붙을 만큼 계속한다
            while !stop.load(Ordering::SeqCst) && appended.load(Ordering::SeqCst) < 1500 {
                writer_store
                    .append(AuditEntry {
                        profile: profile(),
                        actor: "system".into(),
                        action: "test.append".into(),
                        target: format!("t{}", appended.load(Ordering::SeqCst)),
                        args_sha256: None,
                        verdict: "ok".into(),
                    })
                    .await
                    .unwrap();
                appended.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
    };
    // 쓰는 쪽이 끝날 때까지(적어도 250번) 계속 검증한다: 이어 붙는 동안의 검증이 대상이다
    let mut false_alarms = Vec::new();
    let mut round = 0;
    while round < 250 || !writer.is_finished() {
        let report = db.store.verify().await.unwrap();
        if let Some(bad) = report.first_bad {
            false_alarms.push((round, bad, report.entries));
        }
        round += 1;
    }
    stop.store(true, Ordering::SeqCst);
    writer.await.unwrap();
    assert!(
        appended.load(Ordering::SeqCst) > 500,
        "검증이 도는 동안 실제로 이어 붙었다"
    );
    assert!(
        false_alarms.is_empty(),
        "변조가 없는데 {}회 변조로 나왔다(라운드·순번·항목 수): {:?}",
        false_alarms.len(),
        &false_alarms[..false_alarms.len().min(5)]
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

/// 같은 스냅숏에서 읽어도 진짜 변조는 그대로 잡힌다(이어 붙이는 동안에도).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampering_is_still_found_while_entries_are_appended() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let Some(db) = TestDb::new().await else {
        return;
    };
    some_traffic(&db, 10).await;
    db.sql("update bot_audit set verdict = 'forged' where seq = 3")
        .await;
    let writer_store = db.peer("appender").await;
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut count = 0;
            while !stop.load(Ordering::SeqCst) && count < 500 {
                writer_store
                    .append(AuditEntry {
                        profile: profile(),
                        actor: "system".into(),
                        action: "test.append".into(),
                        target: "t".into(),
                        args_sha256: None,
                        verdict: "ok".into(),
                    })
                    .await
                    .unwrap();
                count += 1;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
    };
    for _ in 0..50 {
        assert_eq!(db.store.verify().await.unwrap().first_bad, Some(3));
    }
    stop.store(true, Ordering::SeqCst);
    writer.await.unwrap();
}

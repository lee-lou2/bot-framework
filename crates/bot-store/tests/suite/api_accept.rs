//! 키로 이어지는 긴 요청 입구(OpenAI 호환 HTTP)의 원자 accept-or-replay(PG).
//! - 접수: 키 예약·진행 중 작업 수 한도·실제 run·감사가 한 커밋이다(미커밋 키만, 또는 run만 남지 않는다). 같은 키·같은 지문은 같은 작업(새 슬롯·새 실행 0),
//!   다른 지문은 모델·바인딩이 달라도 충돌, 동시 요청도 run 1건, 한도는 동시 접수에서도 정확하다.
//! - 조회 범위: (프로필, 입구, 주체, 키)로 좁혀지고, 접수 때 고정한 모델·바인딩·에이전트·판본·최초 시각이 유지된다.
//! - 확정 못 한 커밋은 `Unknown`, 커밋 전 실패는 `Err(Unavailable)`이다. 상태·승인 판정은 `api_status.rs`, 승인 카드는 `api_approval_cards.rs`.

use crate::support::{api::*, *};
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiRuns, ApiState},
    command::JsonPayload,
    id::*,
    port::Error,
    state::*,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Barrier;

#[tokio::test]
async fn a_fresh_key_commits_the_request_the_run_and_the_audit_together_with_the_fixed_values() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 4)).await);
    assert_eq!(
        (
            run.model.as_str(),
            run.binding.0.as_str(),
            run.agent.0.as_str(),
            run.revision.0.as_str()
        ),
        ("assistant", "b-assistant", "main", "r1")
    );
    assert_eq!(run.digest, [1; 32]);
    assert_eq!(run.state, ApiState::Active);
    // 최초 시각은 DB 시계의 Unix 초다
    let created: i64 = db
        .scalar(&format!(
            "select created_ms / 1000 from bot_runs where id = '{}'",
            run.run.0
        ))
        .await;
    assert_eq!(run.created as i64, created);
    // 접수 행·run·감사가 하나씩이고, 출처는 인증된 HTTP다
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
    assert_eq!(
        db.scalar::<String>(&format!(
            "select run_origin from bot_runs where id = '{}'",
            run.run.0
        ))
        .await,
        "http"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );
    // 같은 접수를 조회로도 읽는다
    let seen = lookup(&db, "k1", "build").await.expect("조회");
    assert_eq!((seen.run.clone(), seen.created), (run.run, run.created));
}

#[tokio::test]
async fn the_same_key_and_digest_replays_the_run_without_a_new_run_or_slot() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = accepted(admit(&db, request("k1", "build", "assistant", 1, 1)).await);
    // 한도(1)가 찼어도 같은 키·같은 지문은 같은 작업에 붙는다(대화·스레드는 매번 다르다)
    for _ in 0..3 {
        match admit(&db, request("k1", "build", "assistant", 1, 1)).await {
            ApiAdmission::Replayed(run) => assert_eq!(run.run, first.run),
            other => panic!("재전달이어야 한다: {other:?}"),
        }
    }
    assert!(matches!(
        admit(&db, request("k2", "build", "assistant", 1, 1)).await,
        ApiAdmission::Capacity
    ));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1,
        "재전달은 감사를 남기지 않는다"
    );
}

#[tokio::test]
async fn another_digest_conflicts_even_for_another_model_or_binding_and_leaves_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = accepted(admit(&db, request("k1", "build", "assistant", 1, 4)).await);
    // 같은 키의 다른 입력, 모델이 달라진 같은 키(바인딩도 다르다)는 모두 충돌이다: 같은 키의 의미가 둘로 나뉘지 않는다
    for (model, digest) in [("assistant", 2u8), ("other", 1), ("other", 3)] {
        assert!(
            matches!(
                admit(&db, request("k1", "build", model, digest, 4)).await,
                ApiAdmission::Conflict
            ),
            "{model} {digest}"
        );
    }
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
    assert_eq!(lookup(&db, "k1", "build").await.unwrap().run, first.run);
}

#[tokio::test]
async fn the_limit_counts_unfinished_work_and_a_finished_run_frees_its_slot() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let one = accepted(admit(&db, request("k1", "build", "assistant", 1, 2)).await);
    let _two = accepted(admit(&db, request("k2", "build", "assistant", 2, 2)).await);
    // 셋째는 한도 때문에 기록 0이다
    assert!(matches!(
        admit(&db, request("k3", "build", "assistant", 3, 2)).await,
        ApiAdmission::Capacity
    ));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (2, 2));
    // 기다리는 작업(승인·보류)도 자리를 쓴다
    db.sql(&format!(
        "update bot_runs set state = 'waiting', wait_kind = 'hold' where id = '{}'",
        one.run.0
    ))
    .await;
    assert!(matches!(
        admit(&db, request("k3", "build", "assistant", 3, 2)).await,
        ApiAdmission::Capacity
    ));
    // 끝난 작업만 자리를 비운다
    db.sql(&format!(
        "update bot_runs set state = 'succeeded', wait_kind = null where id = '{}'",
        one.run.0
    ))
    .await;
    let three = accepted(admit(&db, request("k3", "build", "assistant", 3, 2)).await);
    assert_ne!(three.run, one.run);
    // 한도는 입구마다 따로 센다(다른 입구의 작업은 이 입구를 막지 않는다)
    let mut elsewhere = request("k4", "build", "assistant", 4, 1);
    elsewhere.endpoint = "other".into();
    elsewhere.accept.event.principal = PrincipalId("http:other/build".into());
    assert!(matches!(
        admit(&db, elsewhere).await,
        ApiAdmission::Accepted(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_identical_requests_make_exactly_one_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(8));
    let mut tasks = Vec::new();
    for n in 0..8 {
        let store = db.peer(&format!("w{n}")).await;
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            ApiRuns::accept(&store, request("same", "build", "assistant", 7, 4)).await
        }));
    }
    let mut accepted = 0;
    let mut replayed = 0;
    let mut runs = std::collections::BTreeSet::new();
    for task in tasks {
        match task.await.unwrap().unwrap() {
            ApiAdmission::Accepted(run) => {
                accepted += 1;
                runs.insert(run.run.0);
            }
            ApiAdmission::Replayed(run) => {
                replayed += 1;
                runs.insert(run.run.0);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((accepted, replayed, runs.len()), (1, 7, 1));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_distinct_keys_never_exceed_the_limit() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(12));
    let mut tasks = Vec::new();
    for n in 0..12u8 {
        let store = db.peer(&format!("w{n}")).await;
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            ApiRuns::accept(
                &store,
                request(&format!("k{n}"), "build", "assistant", n, 3),
            )
            .await
        }));
    }
    let (mut accepted, mut full) = (0, 0);
    for task in tasks {
        match task.await.unwrap().unwrap() {
            ApiAdmission::Accepted(_) => accepted += 1,
            ApiAdmission::Capacity => full += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((accepted, full), (3, 9));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (3, 3));
}

#[tokio::test]
async fn a_lookup_sees_only_its_own_profile_endpoint_principal_and_key() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 4)).await);
    assert_eq!(lookup(&db, "k1", "build").await.unwrap().run, run.run);
    // 다른 주체·다른 키·다른 입구·다른 프로필은 «없음»이다(타인의 키 존재 여부를 알려 주지 않는다)
    assert!(lookup(&db, "k1", "other").await.is_none());
    assert!(lookup(&db, "nope", "build").await.is_none());
    let mut elsewhere = lookup_key("k1", "build");
    elsewhere.endpoint = "other".into();
    assert!(
        ApiRuns::lookup(&db.store, elsewhere)
            .await
            .unwrap()
            .is_none()
    );
    let mut other_profile = lookup_key("k1", "build");
    other_profile.profile = ProfileId("work".into());
    assert!(
        ApiRuns::lookup(&db.store, other_profile)
            .await
            .unwrap()
            .is_none()
    );
    // 같은 주체·같은 키를 가진 레거시(webhook) 접수 행은 이 조회로 보이지 않는다(최초 시각·계량을 지어내지 않는다)
    let mut legacy = accept_as(
        "personal",
        "legacy",
        &principal("build").0,
        ConversationKind::Http,
        "c-legacy",
        "hi",
    );
    legacy.event.binding = BindingId("hook".into());
    runs(&db).accept(legacy).await.unwrap();
    assert!(lookup(&db, "legacy", "build").await.is_none());
}

#[tokio::test]
async fn the_fixed_values_and_the_activation_fence_survive_a_later_revision_and_set_change() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 8)).await);
    let pinned = db.store.for_activation(Revision("set-1".into()));
    assert!(
        ApiRuns::lookup(&pinned, lookup_key("k1", "build"))
            .await
            .unwrap()
            .is_some()
    );
    // 새 판본이 활성이 되어도 접수 때 고정한 값은 그대로다(재지정된 모델의 결과로 바뀌어 보이지 않는다)
    let set = std::collections::BTreeMap::from([
        (ProfileId("personal".into()), Revision("r2".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    bot_core::profile::ActiveRevisions::replace(
        &db.store,
        Some(Revision("set-1".into())),
        Revision("set-2".into()),
        set,
        Vec::new(),
        Default::default(),
    )
    .await
    .unwrap();
    let seen = lookup(&db, "k1", "build").await.unwrap();
    assert_eq!(
        (
            seen.run,
            seen.binding.0.as_str(),
            seen.agent.0.as_str(),
            seen.revision.0.as_str(),
            seen.model.as_str()
        ),
        (run.run, "b-assistant", "main", "r1", "assistant")
    );
    // 옛 집합에 고정한 핸들은 읽지도 접수하지도 못한다(조회는 낡음, 접수는 아무것도 남기지 않는 `Stale`)
    let got = ApiRuns::lookup(&pinned, lookup_key("k1", "build")).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert!(matches!(
        ApiRuns::accept(&pinned, request("k2", "build", "assistant", 2, 8))
            .await
            .unwrap(),
        ApiAdmission::Stale
    ));
    // 새 요청은 현재 활성 판본(r2)과 맞아야 한다
    assert!(matches!(
        admit(&db, request("k3", "build", "assistant", 3, 8)).await,
        ApiAdmission::Stale
    ));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
}

#[tokio::test]
async fn a_failure_before_the_commit_leaves_neither_a_request_row_nor_a_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 감사 기록이 막히면(커밋 전 실패) 접수 행·run도 남지 않고 «커밋 전»이라는 뜻의 `Unavailable`이다
    db.sql(
        "create function bot_fail_audit() returns trigger as $$ begin raise exception 'audit blocked'; end $$ language plpgsql",
    )
    .await;
    db.sql("create trigger bot_fail_audit before insert on bot_audit for each row execute function bot_fail_audit()")
        .await;
    let got = ApiRuns::accept(&db.store, request("k1", "build", "assistant", 1, 4)).await;
    assert!(matches!(got, Err(Error::Unavailable)), "{got:?}");
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (0, 0));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 0);
    db.sql("drop trigger bot_fail_audit on bot_audit").await;
    // 같은 키를 그대로 다시 보내면 새로 접수된다(키가 남지 않았다)
    assert!(matches!(
        admit(&db, request("k1", "build", "assistant", 1, 4)).await,
        ApiAdmission::Accepted(_)
    ));
}

#[tokio::test]
async fn a_commit_that_cannot_be_confirmed_is_unknown_and_never_half_written() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 커밋 순간 백엔드가 끊긴다(응답을 잃은 것과 같다): 커밋했는지 호출자는 모른다
    db.sql(
        "create function bot_kill_at_commit() returns trigger as $$ begin perform pg_terminate_backend(pg_backend_pid()); return null; end $$ language plpgsql",
    )
    .await;
    db.sql("create constraint trigger bot_kill_at_commit after insert on bot_requests deferrable initially deferred for each row execute function bot_kill_at_commit()")
        .await;
    let got = admit(&db, request("k1", "build", "assistant", 1, 4)).await;
    assert!(matches!(got, ApiAdmission::Unknown), "{got:?}");
    db.sql("drop trigger bot_kill_at_commit on bot_requests")
        .await;
    // 같은 키로 조회해 확인한다: 여기서는 커밋되지 않았으므로 접수 행·run이 모두 없다(한쪽만 남지 않는다)
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(lookup(&db, "k1", "build").await.is_none());
    assert_eq!(
        (
            requests(&db).await,
            db.scalar::<i64>("select count(*) from bot_runs").await
        ),
        (0, 0)
    );
    // 그 키로 다시 접수하면 정상이다
    assert!(matches!(
        admit(&db, request("k1", "build", "assistant", 1, 4)).await,
        ApiAdmission::Accepted(_)
    ));
}

/// 접수 요청을 입구에 맞지 않게 만드는 변경.
type Mutation = Box<dyn Fn(&mut ApiAccept)>;

#[tokio::test]
async fn requests_that_do_not_fit_this_entrance_are_invalid_and_record_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cases: Vec<(&str, Mutation)> = vec![
        ("limit 0", Box::new(|r| r.max_active = 0)),
        ("limit 257", Box::new(|r| r.max_active = 257)),
        (
            "principal is not http",
            Box::new(|r| r.accept.event.principal = PrincipalId("local:owner".into())),
        ),
        (
            "endpoint differs from the principal's",
            Box::new(|r| r.endpoint = "other".into()),
        ),
        (
            "key longer than 128 bytes",
            Box::new(|r| r.accept.event.request = RequestId("k".repeat(129))),
        ),
        (
            "empty key",
            Box::new(|r| r.accept.event.request = RequestId(String::new())),
        ),
        ("empty model", Box::new(|r| r.model = String::new())),
        (
            "a direct tool target",
            Box::new(|r| {
                r.accept.target = bot_core::command::Target::Tool {
                    agent: AgentId("main".into()),
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                }
            }),
        ),
        (
            "not an http conversation",
            Box::new(|r| {
                r.accept.event.conversation.kind = ConversationKind::Cli;
                r.accept.event.http = None;
            }),
        ),
    ];
    for (name, change) in cases {
        let mut req = request("k1", "build", "assistant", 1, 4);
        change(&mut req);
        let got = ApiRuns::accept(&db.store, req).await;
        assert!(matches!(got, Err(Error::Invalid)), "{name}: {got:?}");
    }
    assert_eq!(
        (
            requests(&db).await,
            db.scalar::<i64>("select count(*) from bot_runs").await
        ),
        (0, 0)
    );
}

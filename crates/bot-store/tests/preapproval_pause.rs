//! 사전 승인의 정지·재개: 새 허가 차단·결과 기록 허용·세대 비교·CAS 명령·경합·관리 조회. 실제 PG 위에서 확인한다.

mod support;

use bot_core::{
    admin::{AdminAct, AdminRead},
    command::*,
    grant::{GrantCas, reason},
    id::*,
    port::Error,
    state::*,
};
use support::{preapproval::*, *};

const PAIRS: &[(&str, &str)] = &[("post", "channel:reports"), ("notify", "user:ops")];

fn confirmed(effect: &EffectId, receipt: &str) -> EffectStatus {
    EffectStatus {
        effect: effect.clone(),
        state: EffectState::Confirmed,
        resolution: None,
        receipt: Some(receipt.into()),
    }
}

#[tokio::test]
async fn pause_stops_new_permits_not_results_and_a_resume_cannot_revive_an_old_call() {
    let Some(r) = rig(10, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let first = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(first.send);
    // 정지 커밋 뒤에는 새 허가가 없다. 먼저 허가한 쓰기의 결과 기록과 같은 단계의 재요청은 받는다
    let paused = pause(&r, &r.grant.0, 0, true).await.unwrap();
    assert!(paused.row.paused && paused.row.epoch == 1 && !paused.repeated);
    let blocked = r.effect(&ctx, &call, "s2", "post", "channel:reports").await;
    assert!(matches!(blocked, Err(Error::Denied)));
    assert_eq!(
        r.verdicts("effect.refuse").await,
        vec![reason::PAUSED.to_owned()]
    );
    let replay = r
        .effect(&ctx, &call, "s1", "post", "channel:reports")
        .await
        .unwrap();
    assert!(!replay.send, "정지 중에도 재요청은 새 쓰기가 아니다");
    effects(&r.db)
        .record(
            ctx.attempt.clone(),
            call.clone(),
            confirmed(&first.status.effect, "msg-1"),
        )
        .await
        .unwrap();
    assert_eq!(r.used().await, 1);
    // 새 접수와 호출 시작도 막힌다: 정지 중 발생은 건너뛰고(커서만 앞으로) 재개 뒤 새 발생부터 받는다
    let cursor_before =
        r.db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await;
    assert!(matches!(r.occur(1).await, Err(Error::Stale)));
    let cursor_after =
        r.db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await;
    assert!(
        cursor_after > cursor_before,
        "건너뛴 발생은 다시 오지 않는다"
    );
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_runs").await,
        1,
        "run을 만들지 않았다"
    );
    assert_eq!(
        r.verdicts("schedule.occurrence")
            .await
            .last()
            .map(String::as_str),
        Some("skip:trigger_paused")
    );
    let held_back = r.next_run_unchecked().await;
    assert!(held_back.is_none(), "정지 중에는 접수가 없다");
    // 재개해도 정지 전에 시작한 호출의 새 허가는 되살아나지 않는다(세대가 다르다)
    let resumed = pause(&r, &r.grant.0, 1, false).await.unwrap();
    assert!(!resumed.row.paused && resumed.row.epoch == 2);
    let stale = r.effect(&ctx, &call, "s3", "post", "channel:reports").await;
    assert!(
        matches!(stale, Err(Error::Stale)),
        "정지 전 호출의 새 허가는 낡았다(denied가 아니다)"
    );
    assert!(
        r.verdicts("effect.refuse")
            .await
            .contains(&reason::STALE.to_owned())
    );
    assert_eq!(r.used().await, 1);
    // 재개 뒤 새 발생은 새 세대에 묶여 정상으로 쓴다
    let (fresh, fresh_call) = r.started().await;
    let bound: i64 =
        r.db.scalar(&format!(
            "select epoch from bot_run_grants where run = '{}'",
            fresh.attempt.run.0
        ))
        .await;
    assert_eq!(bound, 2);
    assert!(
        r.effect(&fresh, &fresh_call, "s4", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    assert_eq!(r.used().await, 2, "정지·재개는 사용량을 지우지 않는다");
}

#[tokio::test]
async fn pause_commands_are_compare_and_set_and_listing_shows_usage() {
    let Some(r) = rig(5, PAIRS).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    assert!(
        r.effect(&ctx, &call, "s1", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    let rows = AdminRead::preapprovals(&r.db.store, &operator())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        (
            row.profile.as_str(),
            row.agent.as_str(),
            row.trigger.as_str(),
            row.grant_id.as_str()
        ),
        ("personal", "main", "notify", r.grant.0.as_str())
    );
    assert_eq!(
        (row.epoch, row.paused, row.used_today, row.max_per_day),
        (0, false, 1, 5)
    );
    let outsider = bot_core::admin::Viewer {
        principal: PrincipalId("local:guest".into()),
        operator: false,
    };
    assert!(matches!(
        AdminRead::preapprovals(&r.db.store, &outsider).await,
        Err(Error::Denied)
    ));
    // 정지 → 같은 명령 재전달(응답 유실)은 같은 행·감사 1건
    let g = r.grant.0.clone();
    let one = pause(&r, &g, 0, true).await.unwrap();
    assert!(!one.repeated && one.row.epoch == 1 && one.row.paused);
    let replay = pause(&r, &g, 0, true).await.unwrap();
    assert!(replay.repeated && replay.row == one.row);
    let same_epoch = pause(&r, &g, 1, true).await.unwrap();
    assert!(same_epoch.repeated, "이미 정지 상태");
    assert_eq!(r.audits("preapproval.pause").await, 1);
    // 재개 → 다시 정지: 첫 정지 명령의 재전달은 상태가 같아도 낡았다(ABA)
    assert!(!pause(&r, &g, 1, false).await.unwrap().repeated);
    assert!(!pause(&r, &g, 2, true).await.unwrap().repeated);
    assert!(matches!(pause(&r, &g, 0, true).await, Err(Error::Stale)));
    assert!(matches!(pause(&r, &g, 1, false).await, Err(Error::Stale)));
    assert!(
        matches!(pause(&r, "g-other", 3, false).await, Err(Error::Stale)),
        "다른 승인 ID"
    );
    assert!(matches!(
        pause(&r, &g, -1, false).await,
        Err(Error::Stale) | Err(Error::Invalid)
    ));
    assert!(matches!(
        pause(&r, &g, i64::MAX, false).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        (
            r.audits("preapproval.pause").await,
            r.audits("preapproval.resume").await
        ),
        (2, 1)
    );
    // 없는 트리거는 Missing, 승인이 교체되면 옛 ID는 낡았다(정지 상태는 이어진다)
    let missing = AdminAct::set_pause(
        &r.db.store,
        PrincipalId(OWNER.into()),
        bot_core::grant::TriggerKey {
            trigger: "ghost".into(),
            ..key()
        },
        GrantCas {
            grant_id: g.clone(),
            epoch: 0,
        },
        true,
    )
    .await;
    assert!(matches!(missing, Err(Error::Missing)));
    let changed = world(6, PAIRS);
    let issued = publish(&r.db, &changed, vec![spec_of(&changed, IMAGE)]).await;
    let fresh = issued[0].0.0.clone();
    assert!(
        matches!(pause(&r, &g, 3, false).await, Err(Error::Stale)),
        "교체된 승인 ID는 낡았다"
    );
    let rows = AdminRead::preapprovals(&r.db.store, &operator())
        .await
        .unwrap();
    assert_eq!(
        (
            rows.len(),
            rows[0].grant_id.as_str(),
            rows[0].paused,
            rows[0].epoch
        ),
        (1, fresh.as_str(), true, 3),
        "정지는 승인이 바뀌어도 이어진다"
    );
}

#[tokio::test]
async fn a_pause_racing_new_permits_leaves_no_permit_after_its_commit() {
    let Some(r) = rig(50, &[("post", "channel:reports")]).await else {
        return;
    };
    let (ctx, call) = r.started().await;
    let peer = r.db.peer("pauser").await;
    let g = r.grant.0.clone();
    let writers = async {
        let mut sent = 0;
        for i in 0..20 {
            let step = format!("w{i}");
            let args = format!(
                r#"{{"operation":"post","target":"channel:reports","params":{{"i":{i}}}}}"#
            );
            match bot_core::port::EffectJournal::request(
                &r.db.store,
                effect_req(&ctx, &call, &step, &args),
            )
            .await
            {
                Ok(permit) if permit.send => sent += 1,
                _ => {}
            }
        }
        sent
    };
    let stopper = async {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        AdminAct::set_pause(
            &peer,
            PrincipalId(OWNER.into()),
            key(),
            GrantCas {
                grant_id: g.clone(),
                epoch: 0,
            },
            true,
        )
        .await
        .unwrap()
    };
    let (sent, paused) = tokio::join!(writers, stopper);
    assert!(paused.row.paused);
    // 사용량·효과 행·감사 시작 줄이 정확히 일치하고, 정지 커밋 시각 뒤에 만든 단계는 없다
    assert_eq!(r.used().await, sent);
    assert_eq!(r.audits("effect.start").await, sent);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_effects").await,
        sent
    );
    let after = r
        .effect(&ctx, &call, "late", "post", "channel:reports")
        .await;
    assert!(matches!(after, Err(Error::Denied)));
}

#[tokio::test]
async fn occurrences_scheduled_in_or_before_a_pause_stay_skipped_after_the_resume() {
    let Some(r) = rig(10, PAIRS).await else {
        return;
    };
    // 스케줄러가 밀려 정지 전에 예정된 발생을 아직 접수하지 못했다
    let before = r.db.now_ms().await - 1;
    r.pause(&r.grant.0, 0, true).await.unwrap();
    // 정지 커밋 뒤에 예정된 발생(정지 구간)
    let during = r.db.now_ms().await + 2;
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    r.pause(&r.grant.0, 1, false).await.unwrap();
    // 재개 뒤 따라잡기가 두 발생을 접수하려 해도 건너뛴다(정지 커밋 뒤 새 접수 없음, 정지 중 예정된 발생은 받지 않음)
    for at in [before, during] {
        assert!(matches!(r.occur_at(at).await, Err(Error::Stale)), "{at}");
    }
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_runs").await,
        0,
        "run 0"
    );
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_run_grants")
            .await,
        0
    );
    let skipped = r.verdicts("schedule.occurrence").await;
    assert_eq!(
        skipped,
        vec!["skip:trigger_paused".to_owned(); 2],
        "건너뛴 발생마다 감사 한 줄"
    );
    let cursor =
        r.db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await;
    assert_eq!(
        cursor, during,
        "커서는 건너뛴 발생까지 나아간다(다시 오지 않는다)"
    );
    // 재개 뒤 예정된 새 발생은 새 세대에 묶여 접수된다
    let fresh = r.db.now_ms().await;
    r.occur_at(fresh).await.unwrap();
    let bound: i64 = r.db.scalar("select epoch from bot_run_grants").await;
    assert_eq!(bound, 2);
    assert_eq!(r.verdicts("schedule.occurrence").await.len(), 3);
}

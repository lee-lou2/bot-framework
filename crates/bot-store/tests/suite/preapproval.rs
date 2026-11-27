//! 사전 승인: 활성화가 발급·유지·철회하고, 접수가 run에 묶고, 브로커가 카드 없이 시작하며, 아무도 그 승인을 물려받지 못한다.
//! 실제 PG 저장소와 실제 브로커(가짜 실행기) 위에서 확인한다.

use crate::support::{preapproval::*, *};
use bot_core::{
    command::*,
    grant::{Authority, Granted, reason},
    id::*,
    port::{Broker, DelegationStore, Error},
};

const PAIRS: &[(&str, &str)] = &[("post", "channel:reports"), ("post", "channel:alerts")];

async fn active_grants(db: &TestDb) -> Vec<String> {
    sqlx::query_scalar("select id from bot_grants where state = 'active' order by id")
        .fetch_all(db.store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn activation_issues_keeps_and_revokes_and_a_revoked_id_never_returns() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let first = r.grant.0.clone();
    assert_eq!(active_grants(&r.db).await, vec![first.clone()]);
    // 같은 선언을 다시 게시하면(합류·재시작처럼) ID를 그대로 읽는다
    assert!(
        publish(&r.db, &r.snap, vec![r.spec.clone()])
            .await
            .is_empty()
    );
    assert_eq!(active_grants(&r.db).await, vec![first.clone()]);
    // 상한이 바뀌면 옛 ID는 즉시 효력을 잃고 새 활성화가 새 ID를 발급한다
    let changed = world(4, PAIRS);
    let spec2 = spec_of(&changed, IMAGE);
    let issued = publish(&r.db, &changed, vec![spec2]).await;
    let second = issued[0].0.0.clone();
    assert_ne!(second, first);
    assert_eq!(active_grants(&r.db).await, vec![second.clone()]);
    let old: String =
        r.db.scalar(&format!(
            "select state from bot_grants where id = '{first}'"
        ))
        .await;
    assert_eq!(old, "revoked");
    // 설정을 과거로 되돌려도 철회된 ID는 되살아나지 않는다(또 새 ID)
    let back = publish(&r.db, &r.snap, vec![r.spec.clone()]).await;
    let third = back[0].0.0.clone();
    assert!(third != first && third != second, "되돌려도 새 ID다");
    assert_eq!(active_grants(&r.db).await, vec![third.clone()]);
    // 선언을 지우면 철회한다(승인 없는 게시는 모두 철회다)
    assert!(publish(&r.db, &r.snap, vec![]).await.is_empty());
    assert!(active_grants(&r.db).await.is_empty());
    assert_eq!(r.audits("preapproval.issue").await, 3);
    assert_eq!(r.audits("preapproval.revoke").await, 3);
    // 정지 상태는 트리거에 속한다: 발급이 거듭돼도 한 행이다
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_trigger_pause")
            .await,
        1
    );
}

#[tokio::test]
async fn a_preapproved_run_goes_through_the_broker_without_a_card_and_leaves_the_grant_on_every_receipt()
 {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    let ctx = r.next_run().await;
    let call = CallId(format!("{}:tool", ctx.attempt.run.0));
    let bound: String = r.db.scalar("select grant_id from bot_run_grants").await;
    assert_eq!(bound, r.grant.0, "접수 때 승인에 묶인다");
    assert!(matches!(
        r.broker.call(tool_call(&ctx, &call)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 1, "도구는 한 번 실행됐다");
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_approvals")
            .await,
        0,
        "승인 카드 0"
    );
    let recorded: String = r.db.scalar("select grant_id from bot_calls").await;
    assert_eq!(recorded, r.grant.0, "호출 영수증에 승인 ID");
    assert_eq!(
        r.verdicts("call.start").await,
        vec![format!("started:preapproved;grant={}", r.grant.0)]
    );
    // 같은 호출의 재전달은 저장된 결과를 돌려주고 다시 실행하지 않는다
    assert!(matches!(
        r.broker.call(tool_call(&ctx, &call)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn model_delegation_other_triggers_and_fake_bodies_do_not_inherit_the_grant() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    // 본문이 만든 직접 호출 요청(같은 도구·같은 인자): 사람이 요청한 일반 접수는 기존 카드 규칙이다
    let mut forged = tool_accept("personal", "forged", TOOL, ARGS);
    forged.scopes = scopes(&["run", "reports.post"]);
    forged.approver = Some(PrincipalId(OWNER.into()));
    runs(&r.db).accept(forged).await.unwrap();
    let forged = runs(&r.db).claim().await.unwrap().unwrap();
    let forged_call = CallId(format!("{}:tool", forged.attempt.run.0));
    let result = r
        .broker
        .call(tool_call(&forged, &forged_call))
        .await
        .unwrap();
    assert!(matches!(result, CallResult::Awaiting(_)), "카드를 거친다");
    assert!(r.runner.jobs().is_empty());
    // 위임한 자식 run은 부모의 승인을 물려받지 못한다
    let parent = r.next_run().await;
    let child =
        r.db.store
            .delegate(Delegate {
                checked_revision: Revision("r1".into()),
                parent: parent.attempt.clone(),
                step: "review".into(),
                child: AgentId("main/research".into()),
                instruction: "go".into(),
                scopes: scopes(&["reports.post"]),
            })
            .await
            .unwrap();
    let next = runs(&r.db).claim().await.unwrap().unwrap();
    assert_eq!(next.attempt.run, child);
    let denied = r.broker.call(tool_call(&next, &CallId("c1".into()))).await;
    assert!(matches!(denied, Err(Error::Denied)), "승인자가 없어 거절");
    assert!(r.runner.jobs().is_empty(), "어느 쪽도 실행 0");
    // 시작 커밋도 같은 판정이다: 접수에 묶이지 않은 run·위임 자식·직접 호출이 아닌 호출·모양이 다른 지문
    let loose = tool_accept("personal", "loose", TOOL, ARGS);
    runs(&r.db).accept(loose).await.unwrap();
    let loose = runs(&r.db).claim().await.unwrap().unwrap();
    let call = r.begin(&loose, ARGS).await;
    assert_eq!(
        r.start(&loose, &call).await.unwrap(),
        Granted::Refused(reason::STALE),
        "접수에 묶인 승인이 없다"
    );
    let ctx = r.next_run().await;
    let call = r.begin(&ctx, ARGS).await;
    let wrong = Authority::Grant {
        fingerprint: [9; 32],
    };
    assert_eq!(
        calls(&r.db)
            .start_with(start_of(&ctx, &call), wrong)
            .await
            .unwrap(),
        Granted::Refused(reason::STALE),
        "지문이 다르면 시작하지 않는다"
    );
    let mut other_revision = start_of(&ctx, &call);
    other_revision.tool_revision = Revision("t1@other-image".into());
    let authority = Authority::Grant {
        fingerprint: r.spec.fingerprint,
    };
    assert_eq!(
        calls(&r.db)
            .start_with(other_revision, authority)
            .await
            .unwrap(),
        Granted::Refused(reason::STALE),
        "설치 판본이 다르면 시작하지 않는다"
    );
    // 권한 없이 시작하려는 호출은 그 자리에서 거절로 닫힌다(건너뛰는 길이 없다)
    let plain = calls(&r.db).start(start_of(&ctx, &call)).await;
    assert!(matches!(plain, Err(Error::Denied)));
    let state: String =
        r.db.scalar(&format!(
            "select state from bot_calls where run = '{}'",
            ctx.attempt.run.0
        ))
        .await;
    assert_eq!(state, "denied");
    assert_eq!(r.runner.jobs().len(), 0);
    assert!(
        r.verdicts("call.deny")
            .await
            .contains(&reason::STALE.to_owned())
    );
}

#[tokio::test]
async fn a_changed_install_or_republished_scope_leaves_old_runs_refused() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    // 설치 판본이 달라지면(재활성화 없이) 브로커가 다시 계산한 지문이 달라 시작하지 않는다
    let ctx = r.next_run().await;
    let call = CallId(format!("{}:tool", ctx.attempt.run.0));
    r.runner.set_image("reports", Some("reports-img-2"));
    let swapped = r.broker.call(tool_call(&ctx, &call)).await;
    assert!(matches!(swapped, Err(Error::Denied)));
    assert!(r.runner.jobs().is_empty());
    assert!(
        r.verdicts("call.deny")
            .await
            .contains(&reason::STALE.to_owned())
    );
    // 새로 접수한 run의 결과도 같은 이유로 거절된다. 복원하면 같은 승인 ID가 다시 맞는다(ID는 철회된 적이 없다)
    r.runner.set_image("reports", Some(IMAGE));
    let again = r.next_run().await;
    let again_call = CallId(format!("{}:tool", again.attempt.run.0));
    assert!(matches!(
        r.broker.call(tool_call(&again, &again_call)).await.unwrap(),
        CallResult::Done(_)
    ));
    // 선언이 바뀐 새 활성화는 옛 승인에 묶여 접수된 run을 쓸 수 없게 한다
    let waiting = r.next_run().await;
    let waiting_call = CallId(format!("{}:tool", waiting.attempt.run.0));
    let changed = world(9, PAIRS);
    publish(&r.db, &changed, vec![spec_of(&changed, IMAGE)]).await;
    r.catalog.publish(changed);
    let late = r.broker.call(tool_call(&waiting, &waiting_call)).await;
    assert!(
        matches!(late, Err(Error::Denied)),
        "옛 승인에 묶인 run은 거절"
    );
    let grants = r.verdicts("call.deny").await;
    assert!(grants.iter().filter(|v| *v == reason::STALE).count() >= 2);
}

#[tokio::test]
async fn a_pause_or_republish_between_acceptance_and_start_keeps_the_old_run_from_starting() {
    let Some(r) = rig(3, PAIRS).await else {
        return;
    };
    // 접수한 뒤 정지가 커밋되면 호출을 시작하지 못한다(trigger_paused). 재개해도 세대가 달라 낡았다(preapproval_stale)
    let ctx = r.next_run().await;
    let call = r.begin(&ctx, ARGS).await;
    r.pause(&r.grant.0, 0, true).await.unwrap();
    assert_eq!(
        r.start(&ctx, &call).await.unwrap(),
        Granted::Refused(reason::PAUSED)
    );
    r.pause(&r.grant.0, 1, false).await.unwrap();
    assert_eq!(
        r.start(&ctx, &call).await.unwrap(),
        Granted::Refused(reason::STALE)
    );
    let state: String = r.db.scalar("select state from bot_calls").await;
    assert_eq!(
        state, "open",
        "거절한 시작은 아무것도 쓰지 않는다(호출을 닫는 것은 브로커 몫)"
    );
    assert_eq!(r.audits("call.start").await, 0);
    // 직접 호출(`{run}:tool`)만 승인을 쓴다: 같은 도구·인자라도 다른 호출 ID·위임 자식은 시작하지 못한다
    let fresh = r.next_run().await;
    let other = CallId(format!("{}:other", fresh.attempt.run.0));
    calls(&r.db)
        .begin(fresh.attempt.clone(), other.clone(), tool(TOOL), sha(ARGS))
        .await
        .unwrap();
    assert_eq!(
        r.start(&fresh, &other).await.unwrap(),
        Granted::Refused(reason::STALE),
        "다른 호출 ID"
    );
    // 시작한 호출이 쓰기 허가를 받기 전에 승인이 철회되면(교체) 새 허가는 낡았다(stale) — 사용량 0
    let direct = r.begin(&fresh, ARGS).await;
    assert_eq!(r.start(&fresh, &direct).await.unwrap(), Granted::Started);
    let changed = world(9, PAIRS);
    publish(&r.db, &changed, vec![spec_of(&changed, IMAGE)]).await;
    let late = r
        .effect(&fresh, &direct, "s1", "post", "channel:reports")
        .await;
    assert!(matches!(late, Err(Error::Stale)), "철회된 승인의 새 허가");
    assert_eq!(r.used().await, 0);
    assert_eq!(
        r.db.scalar::<i64>("select count(*) from bot_effects").await,
        0
    );
    // 사용량은 승인이 새 ID로 바뀌어도 (트리거, 일)로 이어진다
    let next = r.next_run().await;
    let next_call = r.begin(&next, ARGS).await;
    let authority = Authority::Grant {
        fingerprint: spec_of(&changed, IMAGE).fingerprint,
    };
    assert_eq!(
        calls(&r.db)
            .start_with(start_of(&next, &next_call), authority)
            .await
            .unwrap(),
        Granted::Started
    );
    assert!(
        r.effect(&next, &next_call, "s2", "post", "channel:reports")
            .await
            .unwrap()
            .send
    );
    assert_eq!(r.used().await, 1);
    let changed_again = world(10, PAIRS);
    publish(&r.db, &changed_again, vec![spec_of(&changed_again, IMAGE)]).await;
    assert_eq!(r.used().await, 1, "재활성화로 사용량은 초기화되지 않는다");
}

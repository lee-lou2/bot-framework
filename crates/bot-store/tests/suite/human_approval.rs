//! 지정한 사람의 승인: 지정한 사람의 메시지·반응이 승인 한 단계를 대신한다. 실제 PG 저장소와 실제 브로커(가짜 실행기) 위에서
//! 인증된 이벤트 증거의 접수·소비·재전달·우회 시도를 확인한다. 독립적인 요건(민감 회상 확인·소유자 전용 등)은 그대로다.

use crate::support::{human::*, *};
use bot_core::{
    command::*,
    grant::{Authority, Granted, reason},
    id::*,
    ledger::StartCall,
    port::{Broker, DelegationStore, Error},
    state::RunContext,
};
use std::sync::Arc;

const MEMBERS: &[&str] = &[APPROVER, OTHER, "service:morning"];

async fn rig_for(approvers: &[&str], members: &[&str]) -> Option<Rig> {
    rig(world(approvers, members)).await
}

#[tokio::test]
async fn the_designated_person_approves_once_with_no_card_and_the_evidence_is_recorded_with_the_start()
 {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    let evidence: (String, String, String, String) =
        sqlx::query_as("select kind, team, event_id, actor from bot_run_evidence")
            .fetch_one(r.db.store.pool())
            .await
            .unwrap();
    assert_eq!(
        evidence,
        (
            "reaction".into(),
            "T1".into(),
            "Ev1".into(),
            APPROVER.into()
        )
    );
    let result = r.broker.call(Rig::call_of(&ctx)).await.unwrap();
    assert!(matches!(result, CallResult::Done(_)));
    assert_eq!(r.runner.jobs().len(), 1, "도구는 한 번");
    assert_eq!(
        r.count("select count(*) from bot_approvals").await,
        0,
        "카드 0"
    );
    // 증거 ID·원본 이벤트·주체·도구·인자 지문이 승인 소비·시작과 같은 커밋에 남는다
    let used: (String, String, String, String, String, Vec<u8>) = sqlx::query_as(
        "select team, event_id, actor, tool, call, args_sha256 from bot_evidence_uses",
    )
    .fetch_one(r.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        (
            used.0.as_str(),
            used.1.as_str(),
            used.2.as_str(),
            used.3.as_str()
        ),
        ("T1", "Ev1", APPROVER, "reports.notify")
    );
    assert_eq!(used.4, format!("{}:tool", ctx.attempt.run.0));
    assert_eq!(used.5.len(), 32);
    assert_eq!(
        r.verdicts("call.start").await,
        vec!["started:human;evidence=T1/Ev1".to_owned()]
    );
    // 같은 호출의 재전달은 저장된 결과를 돌려준다(두 번 승인·실행 0)
    assert!(matches!(
        r.broker.call(Rig::call_of(&ctx)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 1);
}

#[tokio::test]
async fn a_redelivered_event_and_a_reused_evidence_cannot_approve_twice() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let first = r.react("req-1", APPROVER, "Ev1").await;
    // 같은 접수 키의 재전달은 같은 run으로 수렴한다(증거도 하나)
    let again = runs(&r.db)
        .accept_with(
            event_accept("req-1", APPROVER),
            AcceptExtras {
                source: Some(source("Ev1", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await
        .unwrap();
    assert_eq!(again, first.attempt.run);
    assert_eq!(r.count("select count(*) from bot_run_evidence").await, 1);
    assert!(matches!(
        r.broker.call(Rig::call_of(&first)).await.unwrap(),
        CallResult::Done(_)
    ));
    // 같은 Slack 이벤트를 다른 접수 키로 다시 받으면 이벤트 식별이 첫 run으로 수렴한다: 두 번째 run 자체가 생기지 않는다
    let converged = runs(&r.db)
        .accept_with(
            event_accept("req-2", APPROVER),
            AcceptExtras {
                source: Some(source("Ev1", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await
        .unwrap();
    assert_eq!(converged, first.attempt.run);
    // 그래도 두 번째 run이 생기는 길(이벤트 식별 열이 없던 옛 접수 행)에서 같은 이벤트(팀·이벤트 ID)를 다른 접수 키로 받아도
    // 그 run은 이미 쓴 증거로 승인되지 않는다 → 카드 경로가 아니라 거절
    r.db.sql("update bot_requests set slack_key = null").await;
    let second = r.react("req-2", APPROVER, "Ev1").await;
    let denied = r.broker.call(Rig::call_of(&second)).await;
    assert!(matches!(denied, Err(Error::Denied)), "이미 쓴 증거");
    assert_eq!(r.runner.jobs().len(), 1, "효과는 한 번뿐");
    assert!(
        r.verdicts("call.deny")
            .await
            .contains(&reason::HUMAN_MISMATCH.to_owned())
    );
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 1);
}

#[tokio::test]
async fn other_people_other_candidates_models_and_revoked_members_get_no_bypass() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    // 지정하지 않은 멤버의 반응: 증거는 쓰이지 않고 기존 카드 규칙(요청자 본인 승인)을 따른다
    let other = r.react("req-o", OTHER, "Ev-o").await;
    let waiting = r.broker.call(Rig::call_of(&other)).await.unwrap();
    assert!(matches!(waiting, CallResult::Awaiting(_)));
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
    assert!(
        r.verdicts("call.note")
            .await
            .contains(&reason::HUMAN_MISMATCH.to_owned())
    );
    // 증거는 첫 후보의 직접 호출(`{run}:tool`)에만 묶인다: 다른 호출 ID는 카드 규칙이다
    let mine = r.react("req-m", APPROVER, "Ev-m").await;
    let mut other_call = Rig::call_of(&mine);
    other_call.call = CallId(format!("{}:tool:1", mine.attempt.run.0));
    assert!(matches!(
        r.broker.call(other_call).await.unwrap(),
        CallResult::Awaiting(_)
    ));
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
    // 위임한 자식 run은 증거를 쓰지 못한다
    let child =
        r.db.store
            .delegate(Delegate {
                checked_revision: Revision("r1".into()),
                parent: mine.attempt.clone(),
                step: "review".into(),
                child: AgentId("main".into()),
                instruction: "go".into(),
                scopes: scopes(&["reports.post"]),
            })
            .await;
    if let Ok(child) = child {
        let next = runs(&r.db).claim().await.unwrap().unwrap();
        assert_eq!(next.attempt.run, child);
        let result = r
            .broker
            .call(ToolCall {
                call: CallId("c1".into()),
                ..Rig::call_of(&next)
            })
            .await;
        assert!(
            !matches!(result, Ok(CallResult::Done(_))),
            "자식은 승인 없이 실행하지 못한다"
        );
    }
    assert!(r.runner.jobs().is_empty());
    // 저장소도 같은 판정이다: 증거가 없는 run·다른 행위자·다른 호출은 시작하지 않고 증거를 소비하지 않는다
    let start = |ctx: &RunContext, call: &str| StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Irreversible,
        tool_revision: Revision("t1@reports-img".into()),
        attempt: ctx.attempt.clone(),
        call: CallId(call.into()),
        approval: None,
        private_recall: false,
        owner: format!("{}.{}", ctx.attempt.run.0, call),
    };
    let wrong_actor = Authority::Human {
        actor: PrincipalId(OTHER.into()),
    };
    // 카드를 연 run은 대기로 넘어갔으니 증거를 막 묶은 새 run으로 본다
    let fresh = r.react("req-s", APPROVER, "Ev-s").await;
    let call_id = format!("{}:tool", fresh.attempt.run.0);
    calls(&r.db)
        .begin(
            fresh.attempt.clone(),
            CallId(call_id.clone()),
            ToolId(TOOL.into()),
            sha_args(),
        )
        .await
        .unwrap();
    assert_eq!(
        calls(&r.db)
            .start_with(start(&fresh, &call_id), wrong_actor)
            .await
            .unwrap(),
        Granted::Refused(reason::HUMAN_MISMATCH),
        "증거의 행위자와 다르다"
    );
    assert_eq!(
        r.count("select count(*) from bot_evidence_uses").await,
        0,
        "거절은 증거를 소비하지 않는다"
    );
}

fn sha_args() -> [u8; 32] {
    let value = bot_core::json::parse(ARGS).unwrap();
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&value).unwrap())
}

#[tokio::test]
async fn a_member_revoked_after_the_reaction_cannot_approve() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    // 새 판본이 멤버를 뺐다(활성 판본 교체 전에 카탈로그가 먼저 바뀐 경우도 같다): 승인자 지정도 현재 판본에서 다시 본다
    let revoked = world(&[APPROVER], &[OTHER, "service:morning"]);
    r.catalog.publish(revoked);
    let result = r.broker.call(Rig::call_of(&ctx)).await;
    assert!(
        !matches!(result, Ok(CallResult::Done(_))),
        "멤버가 아니면 어느 길로도 실행하지 못한다"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
    // 선언에서 승인자를 뺀 판본도 같다(앞 호출은 거절로 닫혔으니 새 반응·새 run으로 본다)
    let undesignated = world(&[OTHER], MEMBERS);
    r.catalog.publish(undesignated);
    let ctx = r.react("req-2", APPROVER, "Ev2").await;
    let result = r.broker.call(Rig::call_of(&ctx)).await.unwrap();
    assert!(
        matches!(result, CallResult::Awaiting(_)),
        "지정이 사라지면 카드 규칙이다"
    );
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn evidence_is_only_made_from_an_authenticated_human_direct_tool_event_with_its_candidate() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    let evidence_rows = || async { r.count("select count(*) from bot_run_evidence").await };
    // 원천이나 후보 사슬이 없으면(일반 접수) 증거도 없다
    r.accept("req-a", APPROVER, AcceptExtras::default()).await;
    r.accept(
        "req-b",
        APPROVER,
        AcceptExtras {
            source: Some(source("Ev-b", "1.1")),
            chain: None,
        },
    )
    .await;
    // 서비스 주체는 접수는 되지만 증거를 만들지 않는다
    let service = r
        .accept(
            "req-c",
            "service:morning",
            AcceptExtras {
                source: Some(source("Ev-c", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await;
    assert!(service.1.is_some());
    // 모델 대상·후보 대상이 다른 접수는 넘김 사슬 검증(첫 후보는 접수의 대상과 같은 직접 도구여야 한다)에서 접수째 거절된다: 증거도 run도 남지 않는다
    let mut model = event_accept("req-d", APPROVER);
    model.target = Target::Agent {
        agent: AgentId("main".into()),
        instruction: "react".into(),
    };
    let refused = runs(&r.db)
        .accept_with(
            model,
            AcceptExtras {
                source: Some(source("Ev-d", "1.1")),
                chain: Some(chain("confirm")),
            },
        )
        .await;
    assert!(
        matches!(refused, Err(Error::Invalid)),
        "모델 대상 접수는 직접 도구 사슬을 가질 수 없다: {refused:?}"
    );
    let mut mismatched = chain("confirm");
    mismatched.candidates[0].target = Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId("other.tool".into()),
        args: JsonPayload("{}".into()),
    };
    let refused = runs(&r.db)
        .accept_with(
            event_accept("req-e", APPROVER),
            AcceptExtras {
                source: Some(source("Ev-e", "1.1")),
                chain: Some(mismatched),
            },
        )
        .await;
    assert!(
        matches!(refused, Err(Error::Invalid)),
        "첫 후보가 접수의 대상과 다르면 거절한다: {refused:?}"
    );
    assert_eq!(evidence_rows().await, 0);
    assert_eq!(
        r.count("select count(*) from bot_requests where request in ('req-d', 'req-e')")
            .await,
        0,
        "거절된 접수는 요청 행도 남기지 않는다"
    );
    // 인증된 사람의 직접 도구 이벤트는 한 줄이다
    r.accept(
        "req-f",
        APPROVER,
        AcceptExtras {
            source: Some(source("Ev-f", "1.1")),
            chain: Some(chain("confirm")),
        },
    )
    .await;
    assert_eq!(evidence_rows().await, 1);
}

#[tokio::test]
async fn sensitive_recall_confirmation_is_not_replaced_by_a_human_reaction() {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    // 도구가 외부로 보내고 이 실행이 private 기억을 읽었으면 «민감 회상 뒤 확인»이 따로 필요하다
    let mut external = world(&[APPROVER], MEMBERS);
    {
        let main = Arc::make_mut(&mut external)
            .agents
            .get_mut(&AgentId("main".into()))
            .unwrap();
        for skill in main.skills.values_mut() {
            for tool in skill.tools.values_mut() {
                tool.external = true;
            }
        }
    }
    r.catalog.publish(external);
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    r.db.sql("update bot_runs set private_memory_read = true")
        .await;
    let result = r.broker.call(Rig::call_of(&ctx)).await.unwrap();
    assert!(matches!(result, CallResult::Awaiting(_)), "카드가 남는다");
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
}

#[tokio::test]
async fn the_evidence_only_starts_the_first_candidates_direct_call_and_both_snapshots_must_designate()
 {
    let Some(r) = rig_for(&[APPROVER], MEMBERS).await else {
        return;
    };
    // 저장소 판정: 증거는 `{run}:tool`에만 쓴다(같은 도구·인자라도 후보 위치가 다른 호출 ID는 아니다)
    let ctx = r.react("req-1", APPROVER, "Ev1").await;
    let other = CallId(format!("{}:tool:1", ctx.attempt.run.0));
    calls(&r.db)
        .begin(
            ctx.attempt.clone(),
            other.clone(),
            ToolId(TOOL.into()),
            sha_args(),
        )
        .await
        .unwrap();
    let start = StartCall {
        checked_revision: Revision("r1".into()),
        risk: bot_core::state::Risk::Irreversible,
        tool_revision: Revision("t1@reports-img".into()),
        attempt: ctx.attempt.clone(),
        call: other,
        approval: None,
        private_recall: false,
        owner: "o".into(),
    };
    let human = Authority::Human {
        actor: PrincipalId(APPROVER.into()),
    };
    assert_eq!(
        calls(&r.db).start_with(start, human).await.unwrap(),
        Granted::Refused(reason::HUMAN_MISMATCH)
    );
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
    // 고정 판본이 승인자를 지정하지 않았으면(지금 판본만 지정) 실행하지 못한다: 접수한 판본(r1)과 지금 판본(r2)이 다르다
    let mut now = world(&[APPROVER], MEMBERS);
    Arc::make_mut(&mut now).revision = Revision("r2".into());
    r.catalog.publish(world(&[OTHER], MEMBERS));
    r.catalog.publish(now);
    // 저장소의 활성 판본도 r2로 올린다(그래야 시작 커밋의 판본 대조를 지나 고정·현재 판본의 승인자 지정만 갈린다)
    let current = bot_core::profile::ActiveRevisions::current(&r.db.store)
        .await
        .unwrap();
    bot_core::profile::ActiveRevisions::replace(
        &r.db.store,
        current,
        Revision("set-2".into()),
        std::collections::BTreeMap::from([
            (ProfileId("personal".into()), Revision("r2".into())),
            (ProfileId("work".into()), Revision("r1".into())),
        ]),
        vec![],
        Default::default(),
    )
    .await
    .unwrap();
    let result = r.broker.call(Rig::call_of(&ctx)).await;
    assert!(
        !matches!(result, Ok(CallResult::Done(_))),
        "고정 판본이 지정하지 않은 사람의 증거"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.count("select count(*) from bot_evidence_uses").await, 0);
}

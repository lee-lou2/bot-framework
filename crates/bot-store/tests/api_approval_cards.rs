//! 키로 이어지는 긴 요청 입구(OpenAI 호환 HTTP)로 접수된 실행의 일반 도구 승인 카드(PG). 카드는 지정된 사람의 Slack DM으로만 가고(전달 바인딩·지정된 사람은 브로커가 승인 경로에서
//! 정해 `OpenApproval.delivery`·승인자로 싣는다), 접수 원장이 보증한 루트만 이 경로를 받는다. 게시 영수증이 확정되기 전의 카드는 열리지 않았다.
//! 작업은 원래 요청의 바인딩·요청자·고정 판본·전달 설치(team)를 이어받고, 올리기 전에 승인 경로가 어긋났음을 알면(`block`) 낱말 그대로 저장돼 조회가 읽는다.

mod support;

use bot_core::{
    api::{ApiState, ApprovalBlock},
    cards::{CardOrigin, CardOutbox, CardOutcome},
    command::Delegate,
    id::*,
    ledger::{ApprovalState, CallLedger, Decision},
    port::{DelegationStore, Error},
    state::*,
};
use support::{api::*, *};

const APPROVER: &str = "slack:home/U1";

/// 접수한 API 실행이 승인 대기에 들어갔다: 요청자는 HTTP 주체, 승인자는 지정된 사람, 카드는 전달 바인딩 `team`의 Slack DM으로 가야 한다.
async fn waiting(
    db: &TestDb,
    key: &str,
    nonce: &str,
    team: Option<&str>,
) -> (RunContext, Approval) {
    let mut req = request(key, "build", "assistant", 1, 8);
    req.team = team.map(str::to_owned);
    accepted(admit(db, req).await);
    let ctx = claimed(db).await;
    let approval = open_for(db, &ctx, nonce, Some("team")).await;
    (ctx, approval)
}

async fn open_for(db: &TestDb, ctx: &RunContext, nonce: &str, delivery: Option<&str>) -> Approval {
    let call_id = format!("c-{nonce}");
    begin_call(db, &ctx.attempt, &call_id).await;
    let approval = Approval {
        approver: PrincipalId(APPROVER.into()),
        ..api_approval(&ctx.attempt, &call_id, nonce, "build")
    };
    let mut open = open_approval(approval.clone());
    open.delivery = delivery.map(|d| BindingId(d.into()));
    CallLedger::open_approval(&pinned(db), open).await.unwrap();
    approval
}

fn card(db: &TestDb) -> &dyn CardOutbox {
    &db.store
}

async fn decide_as(db: &TestDb, nonce: &str, actor: &str) -> Result<ApprovalState, Error> {
    calls(db)
        .decide(Decision {
            nonce: ApprovalId(nonce.into()),
            actor: PrincipalId(actor.into()),
            allow: true,
        })
        .await
}

async fn outbox_rows(db: &TestDb) -> i64 {
    db.scalar("select count(*) from bot_outbox").await
}

#[tokio::test]
async fn an_api_origin_card_is_staged_closed_keeps_the_original_evidence_and_posts_no_public_line()
{
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (ctx, approval) = waiting(&db, "k1", "n1", Some("T1")).await;
    // 카드는 닫힌 채 전달 작업이 쌓인다: 전달 바인딩·지정된 사람, 원래 요청의 증거를 이어받는다. HTTP 출처에는 원래 채널이 없어 공개 줄이 없다
    assert_eq!(
        db.scalar::<String>(
            "select binding || '|' || owner || '|' || origin_binding || '|' || requester || '|' || revision || '|' || origin_team || '|' || state from bot_card_jobs"
        )
        .await,
        format!("team|{APPROVER}|b-assistant|http:openai/build|r1|T1|staged")
    );
    assert_eq!(
        card(&db).origin(approval.nonce.clone()).await.unwrap(),
        Some(CardOrigin {
            binding: BindingId("b-assistant".into()),
            requester: principal("build"),
            revision: Revision("r1".into()),
            team: Some("T1".into()),
        })
    );
    assert_eq!(outbox_rows(&db).await, 0);
    // 게시 영수증 전에는 지정된 사람의 클릭도 승인을 소비하지 않고, 사람이 할 일(승인 대기)도 아니다
    assert!(matches!(
        decide_as(&db, "n1", APPROVER).await,
        Err(Error::Conflict)
    ));
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "waiting");
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    // 올라가고 영수증이 확정되면 열린다: 이제 승인 대기이고, 결정은 지정된 사람만(HTTP 주체는 승인할 수 없다)
    let job = card(&db).claim(8).await.unwrap().remove(0);
    assert_eq!(
        (job.binding.0.as_str(), job.owner.0.as_str()),
        ("team", APPROVER)
    );
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db)
        .settle(
            job.key.clone(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outbox_rows(&db).await,
        0,
        "HTTP 출처에는 공개 안내 줄이 없다"
    );
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    let receipt = calls(&db)
        .approval(approval.nonce.clone())
        .await
        .unwrap()
        .delivery;
    let receipt = receipt.expect("영수증");
    assert_eq!(
        (
            receipt.team.as_str(),
            receipt.owner.0.as_str(),
            receipt.binding.0.as_str()
        ),
        ("T1", APPROVER, "team")
    );
    assert!(matches!(
        decide_as(&db, "n1", "http:openai/build").await,
        Err(Error::Denied)
    ));
    assert_eq!(
        decide_as(&db, "n1", APPROVER).await.unwrap(),
        ApprovalState::Approved
    );
}

#[tokio::test]
async fn a_delivery_binding_without_an_authenticated_api_root_is_refused_and_records_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 레거시(webhook·CLI) 실행은 접수 원장이 보증하지 않으므로 전달 바인딩을 받지 못한다
    let ctx = leased(&db, "legacy").await;
    begin_call(&db, &ctx.attempt, "c1").await;
    let mut open = open_approval(Approval {
        requester: principal("build"),
        approver: PrincipalId(APPROVER.into()),
        ..binding(&ctx.attempt, "c1", "n1")
    });
    open.delivery = Some(BindingId("team".into()));
    let got = calls(&db).open_approval(open).await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
    // 전달 바인딩이 없는 API 실행 승인은 로컬 경로다: 작업 없이 곧바로 결정할 수 있다
    let (local, _) = {
        let req = request("k2", "build", "assistant", 2, 8);
        accepted(admit(&db, req).await);
        let local = claimed(&db).await;
        let approval = open_for(&db, &local, "n2", None).await;
        (local, approval)
    };
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(
        lookup(&db, "k2", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    assert_eq!(state_of_run(&db, &local.attempt.run).await, "waiting");
}

#[tokio::test]
async fn a_route_verdict_closes_the_card_before_posting_and_the_lookup_reads_the_stored_word() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (n, why) in [
        ApprovalBlock::NoApprover,
        ApprovalBlock::ApproverRevoked,
        ApprovalBlock::RouteChanged,
    ]
    .into_iter()
    .enumerate()
    {
        let (key, nonce) = (format!("k{n}"), format!("n{n}"));
        let (ctx, _) = waiting(&db, &key, &nonce, Some("T1")).await;
        let job = card(&db).claim(8).await.unwrap().remove(0);
        assert_eq!(job.key.nonce.0, nonce);
        // 올리기 전에 승인 경로가 어긋났다: 카드를 올리지 않고 작업·승인을 닫는다
        card(&db).block(job.key.clone(), why).await.unwrap();
        assert_eq!(
            db.scalar::<String>(&format!(
                "select state || '|' || closed_reason from bot_card_jobs where nonce = '{nonce}'"
            ))
            .await,
            format!("closed|{}", why.code())
        );
        assert_eq!(
            db.scalar::<String>(&format!(
                "select state || '|' || decided_by from bot_approvals where nonce = '{nonce}'"
            ))
            .await,
            "denied|system"
        );
        assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "ready");
        assert_eq!(
            lookup(&db, &key, "build").await.unwrap().state,
            ApiState::ApprovalBlocked(why),
            "{}",
            why.code()
        );
        // 같은 판정의 재전달은 같은 결과(감사 한 번), 닫힌 작업을 게시로 열 수는 없다
        card(&db).block(job.key.clone(), why).await.unwrap();
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select count(*) from bot_audit where action = 'approval.blocked' and target = '{nonce}'"
            ))
            .await,
            1
        );
        let posted = card(&db)
            .settle(
                job.key.clone(),
                CardOutcome::Posted {
                    team: "T1".into(),
                    dm: "D1".into(),
                    ts: "2.5".into(),
                },
            )
            .await;
        assert!(matches!(posted, Err(Error::Conflict)), "{posted:?}");
        // 이 요청은 끝났으니 다음 요청이 같은 입구 슬롯을 쓴다
        db.sql(&format!(
            "update bot_runs set state = 'succeeded' where id = '{}'",
            ctx.attempt.run.0
        ))
        .await;
    }
    assert_eq!(outbox_rows(&db).await, 0);
}

#[tokio::test]
async fn an_opened_card_cannot_be_blocked_later_and_a_dm_failure_is_the_dm_verdict() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    waiting(&db, "k1", "n1", Some("T1")).await;
    let job = card(&db).claim(8).await.unwrap().remove(0);
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db)
        .settle(
            job.key.clone(),
            CardOutcome::Posted {
                team: "T1".into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
    // 열린 카드를 늦은 판정으로 닫지 않는다(지정된 사람의 클릭만 결정한다)
    let late = card(&db).block(job.key, ApprovalBlock::RouteChanged).await;
    assert!(matches!(late, Err(Error::Conflict)), "{late:?}");
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // 올리지 못한 카드(전달 실패)는 DM 전달 실패 판정이다
    let (_, _) = waiting(&db, "k2", "n2", Some("T1")).await;
    let job = card(&db).claim(8).await.unwrap().remove(0);
    assert_eq!(job.key.nonce.0, "n2");
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db).settle(job.key, CardOutcome::Held).await.unwrap();
    assert_eq!(
        lookup(&db, "k2", "build").await.unwrap().state,
        ApiState::ApprovalBlocked(ApprovalBlock::ApprovalDmUnavailable)
    );
    assert_eq!(outbox_rows(&db).await, 0);
}

#[tokio::test]
async fn a_delegated_child_keeps_the_roots_origin_binding_and_delivery_team() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut req = request("k1", "build", "assistant", 1, 8);
    req.team = Some("T9".into());
    let root = accepted(admit(&db, req).await);
    let parent = claimed(&db).await;
    db.store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            checked_revision: Revision("r1".into()),
            step: "research".into(),
            child: AgentId("main/research".into()),
            instruction: "조사해".into(),
            scopes: scopes(&["run"]),
        })
        .await
        .unwrap();
    let child = claimed(&db).await;
    assert_ne!(child.attempt.run, root.run);
    // 자식도 루트의 정확한 원래 바인딩과 접수 출처를 이어받는다(루트→자식→재개)
    assert_eq!(child.binding.0, "b-assistant");
    assert_eq!(child.root, root.run);
    assert_eq!(
        child
            .api
            .as_ref()
            .map(|a| (a.endpoint.as_str(), a.model.as_str())),
        Some(("openai", "assistant"))
    );
    open_for(&db, &child, "n1", Some("team")).await;
    assert_eq!(
        card(&db).origin(ApprovalId("n1".into())).await.unwrap(),
        Some(CardOrigin {
            binding: BindingId("b-assistant".into()),
            requester: principal("build"),
            revision: Revision("r1".into()),
            team: Some("T9".into()),
        })
    );
    // 자식 승인 대기 중에는 아직 카드가 올라가지 않았으니 루트는 진행 중이다. 올라가면 승인 대기다
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    let job = card(&db).claim(8).await.unwrap().remove(0);
    card(&db)
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    card(&db)
        .settle(
            job.key,
            CardOutcome::Posted {
                team: "T9".into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // 이 입구의 접수는 그대로 하나다
    assert_eq!(requests(&db).await, 1);
}

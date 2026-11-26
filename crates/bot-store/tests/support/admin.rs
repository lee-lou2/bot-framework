//! 관리 조회 시험 도구(admin_views·admin_listing이 함께 쓴다).
#![allow(dead_code)]

use super::{OWNER, TestDb};
use bot_core::{
    admin::{AdminRead, HoldKind, HoldRef, Viewer},
    id::{CallId, EffectId, PrincipalId, RunId},
};

pub fn operator() -> Viewer {
    Viewer {
        principal: PrincipalId("local:admin".into()),
        operator: true,
    }
}

pub fn owner() -> Viewer {
    Viewer {
        principal: PrincipalId(OWNER.into()),
        operator: false,
    }
}

/// 보류 목록에서 대상의 열린 발생 ID를 읽어 판정 대상을 만든다. 호출 단위는 `effect`가 None, 다단계는 단계 ID다.
pub async fn hold_ref(
    db: &TestDb,
    kind: HoldKind,
    run: &RunId,
    call: &CallId,
    effect: Option<&EffectId>,
) -> HoldRef {
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    let hold = holds
        .iter()
        .find(|h| h.kind == kind && h.run == *run && h.call == *call)
        .expect("보류 목록에 대상이 있다");
    let hold_id = match effect {
        None => hold.hold_id.clone(),
        Some(e) => hold
            .steps
            .iter()
            .find(|s| s.effect == *e)
            .and_then(|s| s.hold_id.clone()),
    }
    .expect("판정할 수 있는 발생 ID가 있다");
    HoldRef {
        hold_id,
        profile: hold.profile.clone(),
        run: run.clone(),
        call: call.clone(),
        effect: effect.cloned(),
    }
}

/// 0011(보류 발생 ID)을 걷어 이전 판(0010)의 DB로 되돌린다. 구판 모양의 행을 만든 뒤 `store.migrate()`로 다시 올려 본다.
pub async fn rewind_holds(db: &TestDb) {
    sqlx::raw_sql(
        "drop trigger bot_effects_call_hold on bot_effects;
         drop trigger bot_calls_hold on bot_calls;
         drop trigger bot_effects_hold on bot_effects;
         drop trigger bot_outbox_hold on bot_outbox;
         drop trigger bot_runs_hold on bot_runs;
         drop function bot_calls_hold(), bot_effects_hold(), bot_outbox_hold(), bot_runs_hold(),
                       bot_hold_open(text, text, text, text, text), bot_hold_close(text, text, text, text, text);
         drop table bot_holds;
         drop sequence bot_hold_seq;
         delete from _sqlx_migrations where version in (11,25);",
    )
    .execute(db.store.pool())
    .await
    .unwrap();
}

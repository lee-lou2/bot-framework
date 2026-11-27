//! Shared admin-view test helpers (used by `admin_views` and `admin_listing`).
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

/// Looks up the open `HoldId` from `AdminRead::holds` (`effect: None` for call-level, `Some` for step-level).
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
        .expect("target must be in holds list");
    let hold_id = match effect {
        None => hold.hold_id.clone(),
        Some(e) => hold
            .steps
            .iter()
            .find(|s| s.effect == *e)
            .and_then(|s| s.hold_id.clone()),
    }
    .expect("resolvable hold_id must exist");
    HoldRef {
        hold_id,
        profile: hold.profile.clone(),
        run: run.clone(),
        call: call.clone(),
        effect: effect.cloned(),
    }
}

/// Strips migrations 0011 and 0025 to simulate upgrading a pre-`bot_holds` database via `store.migrate()`.
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

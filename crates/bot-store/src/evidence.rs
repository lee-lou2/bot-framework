//! Human-origin approval evidence bound to root runs at intake and consumed once at call start (`authority::human`).
//! Never stores message bodies; evidence identity is `(profile, team, event_id)`.

use crate::{
    calls::fence,
    codec::*,
    db::{R, Store, Tx},
};
use bot_core::{
    command::{Accept, AcceptExtras, Target},
    grant::{Evidence, EvidenceKind},
    id::*,
    slack::SourceKind,
};
use sqlx::Row;

/// Records adapter-verified event evidence on a direct-tool root run when human-triggered with a matching first trigger candidate.
pub(crate) async fn bind_in(
    tx: &mut Tx<'_>,
    a: &Accept,
    run: &str,
    extras: &AcceptExtras,
) -> R<()> {
    let (Some(source), Some(chain)) = (&extras.source, &extras.chain) else {
        return Ok(());
    };
    let e = &a.event;
    let Some(first) = chain.candidates.first() else {
        return Ok(());
    };
    if !matches!(a.target, Target::Tool { .. })
        || !e.principal.is_human()
        || first.target != a.target
    {
        return Ok(());
    }
    let kind = match source.kind {
        SourceKind::Message => EvidenceKind::Message,
        SourceKind::ThreadReply => EvidenceKind::ThreadReply,
        SourceKind::Reaction => EvidenceKind::Reaction,
    };
    if source.thread_ts.as_deref().is_some_and(|t| !id_ok(t)) {
        return invalid();
    }
    ids_ok(&[
        &source.team,
        &source.event_id,
        &source.channel,
        &source.ts,
        &first.trigger,
    ])?;
    sqlx::query(
        "insert into bot_run_evidence (profile, run, kind, team, event_id, binding, channel, item_ts, thread_ts, actor, trigger)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(&e.profile.0)
    .bind(run)
    .bind(kind.as_str())
    .bind(&source.team)
    .bind(&source.event_id)
    .bind(&e.binding.0)
    .bind(&source.channel)
    .bind(&source.ts)
    .bind(&source.thread_ts)
    .bind(&e.principal.0)
    .bind(&first.trigger)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// Returns the run's intake-bound evidence after checking the active set and current claim.
    pub(crate) async fn call_evidence(&self, at: &Attempt) -> R<Option<Evidence>> {
        let mut tx = self.pool.begin().await?;
        self.check_pinned(&mut tx).await?;
        fence(&mut tx, at).await?;
        let row = sqlx::query(
            "select kind, team, event_id, binding, channel, item_ts, thread_ts, actor, trigger
             from bot_run_evidence where profile = $1 and run = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        Ok(row.and_then(|r| {
            Some(Evidence {
                kind: EvidenceKind::from_recorded(&r.get::<String, _>("kind"))?,
                team: r.get("team"),
                event_id: r.get("event_id"),
                binding: BindingId(r.get("binding")),
                channel: r.get("channel"),
                item_ts: r.get("item_ts"),
                thread_ts: r.get("thread_ts"),
                actor: PrincipalId(r.get("actor")),
                trigger: r.get("trigger"),
            })
        }))
    }
}

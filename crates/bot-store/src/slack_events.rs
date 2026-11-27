//! Slack event deduplication across binding/profile reassignments and legacy pre-`slack_key` row adoption.

use crate::{
    codec::*,
    db::{R, Tx},
};
use bot_core::{
    id::RunId,
    slack::{SlackSource, SourceKind},
};
use sqlx::Row;

pub(crate) fn key_of(source: &SlackSource) -> R<String> {
    ids_ok(&[&source.team, &source.channel, &source.ts, &source.event_id])?;
    Ok(source.event_key())
}

/// Temporary key assigned by migration 0022 to pre-`slack_key` rows lacking `team`.
fn legacy_key(source: &SlackSource) -> String {
    format!("l\u{1f}{}.{}", source.channel, source.ts)
}

/// Upgrades a matching legacy message row's `slack_key` in place and returns its run (reactions had no legacy rows).
pub(crate) async fn adopt_legacy(
    tx: &mut Tx<'_>,
    source: &SlackSource,
    key: &str,
) -> R<Option<RunId>> {
    if source.kind == SourceKind::Reaction {
        return Ok(None);
    }
    let row =
        sqlx::query("update bot_requests set slack_key = $1 where slack_key = $2 returning run")
            .bind(key)
            .bind(legacy_key(source))
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.map(|row| RunId(row.get("run"))))
}

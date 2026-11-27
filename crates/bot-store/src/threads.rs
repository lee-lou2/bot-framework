//! Records verified bot-participating Slack threads (`bot_slack_threads`) scoped to `(profile, binding, team, bot, channel, root_ts)`.

use crate::{
    codec::*,
    db::{R, Store},
};
use bot_core::slack::BotThread;

/// Validates Slack `<secs>.<micros>` timestamp format.
fn ts_ok(ts: &str) -> bool {
    ts.len() <= 32
        && ts.split_once('.').is_some_and(|(a, b)| {
            !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
        })
}

fn checked(t: &BotThread) -> R<()> {
    ids_ok(&[&t.profile.0, &t.binding.0, &t.team, &t.bot, &t.channel])?;
    if ts_ok(&t.root_ts) { Ok(()) } else { invalid() }
}

impl Store {
    pub(crate) async fn thread_note(&self, t: &BotThread) -> R<()> {
        checked(t)?;
        sqlx::query(
            "insert into bot_slack_threads (profile, binding, team, bot, channel, root_ts)
             values ($1, $2, $3, $4, $5, $6) on conflict do nothing",
        )
        .bind(&t.profile.0)
        .bind(&t.binding.0)
        .bind(&t.team)
        .bind(&t.bot)
        .bind(&t.channel)
        .bind(&t.root_ts)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn thread_known(&self, t: &BotThread) -> R<bool> {
        if checked(t).is_err() {
            return Ok(false);
        }
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from bot_slack_threads
                            where profile = $1 and binding = $2 and team = $3 and bot = $4 and channel = $5 and root_ts = $6)",
        )
        .bind(&t.profile.0)
        .bind(&t.binding.0)
        .bind(&t.team)
        .bind(&t.bot)
        .bind(&t.channel)
        .bind(&t.root_ts)
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }
}

//! F2: 봇이 참여했다고 확인된 Slack 스레드의 증거(`bot_slack_threads`). 확인된 게시 영수증이나 인증된 자기 봇 메시지만 기록한다.
//! 키는 (프로필, 바인딩, 팀, 봇, 채널, 루트 ts)라 설치(워크스페이스·봇)나 바인딩이 바뀌면 옛 증거는 다른 소유가 된다.

use crate::{
    codec::*,
    db::{R, Store},
};
use bot_core::slack::BotThread;

/// Slack ts(`초.마이크로초`)인가: 숫자 둘을 점으로 이은 짧은 글
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
    /// 이 스레드에 봇이 참여했다고 기록한다(이미 있으면 그대로).
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

    /// 기록돼 있는가. 모양이 틀린 키는 기록될 수 없으니 «없다»(`false`)다.
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

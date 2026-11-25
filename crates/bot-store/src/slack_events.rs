//! F2: Slack 이벤트 한 건은 접수 하나다. 접수 행(`bot_requests.slack_key`)이 어댑터가 인증한 이벤트의 식별(`SlackSource::event_key`)을 따로 지켜,
//! 바인딩·프로필이 바뀐 뒤의 재전달도 첫 접수의 run으로 수렴한다.
//!
//! 이 열이 생기기 전(구판)에 받은 접수 행은 마이그레이션이 팀을 모르는 임시 키(`l␟채널.글ts`)를 달아 두었다. 그 글이 다시 오면 그 행을 이 이벤트의 정식 키로 바꾸고(채택)
//! 그 run으로 수렴한다 — 구판 행이 있는 글을 새 접수로 다시 받지 않는다.

use crate::{
    codec::*,
    db::{R, Tx},
};
use bot_core::{
    id::RunId,
    slack::{SlackSource, SourceKind},
};
use sqlx::Row;

/// 이 이벤트의 접수 식별. 식별자 문자열이 모양을 지켜야 한다(제어 문자 없음).
pub(crate) fn key_of(source: &SlackSource) -> R<String> {
    ids_ok(&[&source.team, &source.channel, &source.ts, &source.event_id])?;
    Ok(source.event_key())
}

/// 구판 행의 임시 키: 팀을 모르므로 (채널, 글 ts)만 담는다(구판의 요청 ID `채널.글ts`와 같은 글이다). 마이그레이션 0022가 같은 모양으로 단다.
fn legacy_key(source: &SlackSource) -> String {
    format!("l\u{1f}{}.{}", source.channel, source.ts)
}

/// 같은 글의 구판 접수 행이 있으면 그 행의 키를 이 이벤트의 정식 키로 바꾸고 그 run을 돌려준다. 글(멘션·답글·DM)만 해당한다(반응은 구판에 없다).
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

//! 호출 시작과 새 쓰기 허가에서 사전 승인(F1)과 지정한 사람의 증거(F4)를 같은 잠금 안에서 확인한다.
//! 확인에 실패하면 저장소는 아무것도 쓰지 않고 고정 사유를 돌려준다(호출을 닫는 것은 브로커의 몫이다).
//! 잠금 순서: 활성 집합(호출자가 이미 쥠) → run → 호출 → 승인 행(공유) → 정지 행(공유) → 사용 행 → 증거 소비 → 감사 head.

use crate::{
    codec::*,
    db::{R, Tx},
    finish::RunRow,
};
use bot_core::{
    command::JsonPayload,
    grant::{Authority, reason},
    ledger::StartCall,
    port::Error,
};
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};

/// 시작 때 확인한 권한의 흔적(호출 행·감사에 남는다).
#[derive(Default)]
pub(crate) struct Mark {
    /// (승인 ID, 접수 때 묶은 정지 세대)
    pub grant: Option<(String, i64)>,
    /// `팀/이벤트` — 소비한 증거의 원본 이벤트 참조
    pub evidence: Option<String>,
}

impl Mark {
    /// 감사 판정에 덧붙이는 꼬리표. 증거 ID·승인 ID만 담고 본문은 담지 않는다.
    pub(crate) fn verdict(&self, base: &str) -> String {
        match (&self.grant, &self.evidence) {
            (Some((id, _)), _) => format!("{base}:preapproved;grant={id}"),
            (_, Some(event)) => format!("{base}:human;evidence={event}"),
            _ => base.to_owned(),
        }
    }
}

/// 직접 호출인가: 루트 run의 `{run}:tool` 호출만이다(위임한 자식·모델이 고른 호출·다른 ID는 아니다).
fn direct(run: &RunRow, call: &str) -> bool {
    run.parent.is_none() && call == format!("{}:tool", run.id)
}

/// 호출을 시작하기 전의 권한 확인. `Err`는 저장소 오류, `Ok(Err(사유))`는 시작할 수 없다는 판정(상태 변경 0)이다.
pub(crate) async fn check_start(
    tx: &mut Tx<'_>,
    run: &RunRow,
    call: &PgRow,
    start: &StartCall,
    pointer: bool,
    authority: Option<&Authority>,
) -> R<Result<Mark, &'static str>> {
    let is_direct = direct(run, &start.call.0);
    let bound: Option<(String, i64)> =
        sqlx::query("select grant_id, epoch from bot_run_grants where profile = $1 and run = $2")
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_optional(&mut **tx)
            .await?
            .map(|row| (row.get("grant_id"), row.get("epoch")));
    match authority {
        // 사전 승인이 묶인 run의 직접 호출은 권한을 내지 않고는 시작하지 못한다(건너뛰는 길이 없다): 호출을 거절로 닫는다
        None if bound.is_some() && is_direct => Ok(Err(reason::STALE)),
        None => Ok(Ok(Mark::default())),
        Some(_) if start.approval.is_some() => Err(Error::Conflict.into()),
        Some(Authority::Grant { fingerprint }) => {
            let Some((grant, epoch)) = bound.filter(|_| is_direct) else {
                return Ok(Err(reason::STALE));
            };
            Ok(granted(tx, call, start, fingerprint, &grant, epoch).await?)
        }
        Some(Authority::Human { actor }) => {
            if !is_direct || pointer || bound.is_some() {
                return Ok(Err(reason::HUMAN_MISMATCH));
            }
            Ok(human(tx, run, call, start, &actor.0).await?)
        }
    }
}

async fn granted(
    tx: &mut Tx<'_>,
    call: &PgRow,
    start: &StartCall,
    fingerprint: &[u8; 32],
    grant: &str,
    epoch: i64,
) -> R<Result<Mark, &'static str>> {
    let row = sqlx::query(
        "select profile, agent, trigger, state, fingerprint, tool, tool_revision, args_sha256
         from bot_grants where id = $1 for share",
    )
    .bind(grant)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(Err(reason::STALE));
    };
    let same = row.get::<String, _>("state") == "active"
        && row.get::<Vec<u8>, _>("fingerprint") == fingerprint.as_slice()
        && row.get::<String, _>("tool") == call.get::<String, _>("tool")
        && row.get::<String, _>("tool_revision") == start.tool_revision.0
        && row.get::<Vec<u8>, _>("args_sha256") == call.get::<Vec<u8>, _>("args_sha256");
    if !same {
        return Ok(Err(reason::STALE));
    }
    match pause_state(tx, &row).await? {
        (true, _) => Ok(Err(reason::PAUSED)),
        (false, current) if current != epoch => Ok(Err(reason::STALE)),
        (false, _) => Ok(Ok(Mark {
            grant: Some((grant.to_owned(), epoch)),
            evidence: None,
        })),
    }
}

/// 승인 행이 가리키는 트리거의 (정지 여부, 세대). 정지 행은 공유 잠금으로 읽어 정지 커밋과 직렬화한다.
async fn pause_state(tx: &mut Tx<'_>, grant: &PgRow) -> R<(bool, i64)> {
    let row = sqlx::query(
        "select epoch, paused from bot_trigger_pause where profile = $1 and agent = $2 and trigger = $3 for share",
    )
    .bind(grant.get::<String, _>("profile"))
    .bind(grant.get::<String, _>("agent"))
    .bind(grant.get::<String, _>("trigger"))
    .fetch_optional(&mut **tx)
    .await?;
    // 정지 행이 없으면 확인할 수 없다: 멈춘 것으로 본다
    Ok(row.map_or((true, 0), |r| (r.get("paused"), r.get("epoch"))))
}

async fn human(
    tx: &mut Tx<'_>,
    run: &RunRow,
    call: &PgRow,
    start: &StartCall,
    actor: &str,
) -> R<Result<Mark, &'static str>> {
    let evidence = sqlx::query(
        "select team, event_id, actor from bot_run_evidence where profile = $1 and run = $2",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(evidence) = evidence.filter(|e| e.get::<String, _>("actor") == actor) else {
        return Ok(Err(reason::HUMAN_MISMATCH));
    };
    let (team, event): (String, String) = (evidence.get("team"), evidence.get("event_id"));
    // (프로필, 팀, 이벤트)당 한 번: 같은 증거가 다른 후보·재시작의 두 번째 승인이 되지 못한다
    let used = sqlx::query(
        "insert into bot_evidence_uses (profile, team, event_id, run, call, tool, tool_revision, args_sha256, actor)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict do nothing",
    )
    .bind(&run.profile)
    .bind(&team)
    .bind(&event)
    .bind(&run.id)
    .bind(&start.call.0)
    .bind(call.get::<String, _>("tool"))
    .bind(&start.tool_revision.0)
    .bind(call.get::<Vec<u8>, _>("args_sha256"))
    .bind(actor)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if used != 1 {
        return Ok(Err(reason::HUMAN_MISMATCH));
    }
    Ok(Ok(Mark {
        grant: None,
        evidence: Some(format!("{team}/{event}")),
    }))
}

/// 권한 없이 시작하려던 호출을 같은 트랜잭션에서 거절로 닫는다(상태 `denied`, 감사 한 줄). 호출자가 커밋한다.
pub(crate) async fn deny_in(tx: &mut Tx<'_>, run: &RunRow, call: &PgRow, why: &str) -> R<()> {
    sqlx::query(
        "update bot_calls set state = 'denied', updated_ms = bot_now_ms(), finished_ms = bot_now_ms()
         where profile = $1 and run = $2 and call = $3 and state = 'open'",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .bind(call.get::<String, _>("call"))
    .execute(&mut **tx)
    .await?;
    let principal: String =
        sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_one(&mut **tx)
            .await?;
    let id: String = call.get("call");
    let mut entry = crate::audit::Entry::new(&principal, "call.deny", &id, why);
    entry.args = Some(to32(call.get("args_sha256"))?);
    crate::audit::append(tx, &run.profile, &entry).await?;
    Ok(())
}

/// 새 쓰기 허가를 거절한 이유. 감사에는 고정 사유(`reason`)가, 호출자(효과 채널)에는 wire 분류(`error`)가 간다:
/// 대상 밖·한도·정지 = denied, 철회·교체·옛 정지 세대 = stale, 형식 오류 = invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refused {
    Stale,
    Paused,
    Limit,
    /// 정확한 (operation, target) 쌍이 허용 목록에 없다.
    Target,
    /// args가 `{operation, target, params}` 모양이 아니다.
    Shape,
}

impl Refused {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Stale => reason::STALE,
            Self::Paused => reason::PAUSED,
            Self::Limit => reason::LIMIT,
            Self::Target | Self::Shape => reason::TARGET_DENIED,
        }
    }

    pub(crate) fn error(self) -> Error {
        match self {
            Self::Stale => Error::Stale,
            Self::Shape => Error::Invalid,
            Self::Paused | Self::Limit | Self::Target => Error::Denied,
        }
    }
}

/// 효과 요청 args의 모양: 정확히 `{operation, target, params}`(앞 둘은 문자열, params는 객체)이다.
fn shape(args: &JsonPayload) -> Option<(String, String)> {
    let Ok((Value::Object(map), _)) = canonical(args) else {
        return None;
    };
    let (operation, target) = (
        map.get("operation")?.as_str()?,
        map.get("target")?.as_str()?,
    );
    (map.len() == 3 && map.get("params")?.is_object())
        .then(|| (operation.to_owned(), target.to_owned()))
}

fn listed(pairs: &Value, operation: &str, target: &str) -> bool {
    pairs.as_array().is_some_and(|list| {
        list.iter()
            .any(|p| p[0].as_str() == Some(operation) && p[1].as_str() == Some(target))
    })
}

/// 승인이 철회·교체됐거나 호출이 시작된 뒤 정지 세대가 바뀌었는가(낡았다). 정지 중이어도 같다: 정지는 세대를 올리므로 정지 전에 시작한 호출은 정지 중에도 옛 세대다.
/// 정지 여부·허용 쌍·한도는 새 허가에서만 본다.
/// 기록된 단계의 인자가 달라 충돌로 끝내기 전에 이것을 먼저 봐, 같은 유효한 승인 안에서만 `conflict`이고 철회·교체·옛 세대는 `stale`로 알린다.
pub(crate) async fn stale(tx: &mut Tx<'_>, grant: &str, epoch: i64) -> R<bool> {
    let row = sqlx::query(
        "select profile, agent, trigger, state from bot_grants where id = $1 for share",
    )
    .bind(grant)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row.filter(|r| r.get::<String, _>("state") == "active") else {
        return Ok(true);
    };
    Ok(pause_state(tx, &row).await?.1 != epoch)
}

/// 사전 승인 호출의 **새** 외부 쓰기 허가(새 단계나 운영자 미실행 판정 뒤 다시 보내기)를 확인하고 오늘 사용량을 1 올린다.
/// 호출자가 활성 집합(공유)·run·호출·효과 행을 이미 잠갔고 같은 커밋에서 효과 행과 감사를 쓴다.
/// 어느 거절에서도 사용량은 그대로다.
pub(crate) async fn permit_write(
    tx: &mut Tx<'_>,
    grant: &str,
    epoch: i64,
    args: &JsonPayload,
) -> R<Result<(), Refused>> {
    let row = sqlx::query(
        "select profile, agent, trigger, state, pairs, max_per_day from bot_grants where id = $1 for share",
    )
    .bind(grant)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row.filter(|r| r.get::<String, _>("state") == "active") else {
        return Ok(Err(Refused::Stale));
    };
    match pause_state(tx, &row).await? {
        (true, _) => return Ok(Err(Refused::Paused)),
        (false, current) if current != epoch => return Ok(Err(Refused::Stale)),
        (false, _) => {}
    }
    let Some((operation, target)) = shape(args) else {
        return Ok(Err(Refused::Shape));
    };
    if !listed(&row.get::<Value, _>("pairs"), &operation, &target) {
        return Ok(Err(Refused::Target));
    }
    let (profile, agent, trigger): (String, String, String) =
        (row.get("profile"), row.get("agent"), row.get("trigger"));
    // UTC 일 번호는 이 트랜잭션에서 한 번만 읽는다(자정 경계에서 두 문장이 다른 날을 보지 않게)
    let day: i64 = sqlx::query_scalar("select bot_now_ms() / 86400000")
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query(
        "insert into bot_grant_usage (profile, agent, trigger, day, used) values ($1, $2, $3, $4, 0)
         on conflict do nothing",
    )
    .bind(&profile)
    .bind(&agent)
    .bind(&trigger)
    .bind(day)
    .execute(&mut **tx)
    .await?;
    let used: i32 = sqlx::query_scalar(
        "select used from bot_grant_usage where profile = $1 and agent = $2 and trigger = $3 and day = $4 for update",
    )
    .bind(&profile)
    .bind(&agent)
    .bind(&trigger)
    .bind(day)
    .fetch_one(&mut **tx)
    .await?;
    if used >= row.get::<i32, _>("max_per_day") {
        return Ok(Err(Refused::Limit));
    }
    sqlx::query(
        "update bot_grant_usage set used = used + 1 where profile = $1 and agent = $2 and trigger = $3 and day = $4",
    )
    .bind(&profile)
    .bind(&agent)
    .bind(&trigger)
    .bind(day)
    .execute(&mut **tx)
    .await?;
    Ok(Ok(()))
}

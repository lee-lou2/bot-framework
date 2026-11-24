//! 관리 조회의 승인 부분: 목록(필터·정렬·커서)과 상세, 카드 줄 렌더. 기억 공유 카드(`본문`)와 예약 회신 카드(`전송 초안`)는 소유자에게만 원문을 준다.

use crate::db::{R, Store, ms_to_time};
use bot_core::{admin::*, id::*, port::Error};
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};

/// 단순 `<시각>:<ID>` 커서(승인 대기 목록과 다른 목록들이 같이 쓴다).
fn cursor(after: &Option<String>) -> (Option<i64>, Option<String>) {
    match after.as_deref().and_then(|a| a.split_once(':')) {
        Some((ms, id)) => (ms.parse().ok(), Some(id.to_owned())),
        None => (None, None),
    }
}

/// 처리 시각 순 목록의 커서 `<처리 시각 또는 -1>:<대기 여부 0|1>:<대기면 만료 시각, 아니면 0>:<nonce>`.
fn processed_cursor(after: &Option<String>) -> Option<(i64, i32, i64, String)> {
    let mut parts = after.as_deref()?.splitn(4, ':');
    let decided = parts.next()?.parse().ok()?;
    let pending = parts.next()?.parse().ok()?;
    let expires = parts.next()?.parse().ok()?;
    Some((decided, pending, expires, parts.next()?.to_owned()))
}

/// 카드가 줄 상한 안에서 이어 쓰는 긴 원문.
#[derive(Clone, Copy, PartialEq)]
enum Body {
    /// 이어 쓰는 원문이 없는 카드
    None,
    /// 기억 공유: 본문을 JSON 문자열로 이스케이프해 이어 쓴다
    Share,
    /// 예약 회신: 승인 문서(JSON) 전체를 그대로 이어 쓴다
    Draft,
}

/// 브로커가 렌더링한 카드의 줄(`라벨: 값`)을 (라벨, 값) 쌍으로 바꾼다. 제목이 맨 앞이다.
/// 긴 원문 카드는 줄 상한 안에서 이어 쓴다(첫 줄 `본문: `·`전송 초안: `, 이어지는 줄은 공백 두 칸으로 시작).
/// 그 조각을 이어 붙여 풀어 **한 행(원문 전체)**으로 준다: 소비자가 `memory_share_hash(기억 ID, SHA-256(본문))`나 초안의 SHA-256을 승인의
/// 인자 해시와 대조해 "보이는 것 = 승인한 것"을 확인할 수 있다. 풀 수 없으면 그 행을 주지 않는다(대조가 실패로 끝난다).
fn render_of(card: &str, body: Body) -> Vec<(String, String)> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(card) else {
        return vec![];
    };
    let Some(Value::Object(card)) = map.get("approval") else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(title) = card.get("title").and_then(Value::as_str) {
        out.push(("제목".to_owned(), title.to_owned()));
    }
    // 본문 줄 묶음마다 하나씩(정상 카드는 정확히 하나). 풀어 보기 전에 개수부터 센다: 하나가 아니면(둘 이상이면 하나만 풀려도) 본문 행을 주지 않는다
    let first_label = match body {
        Body::None => None,
        Body::Share => Some("본문: ".to_owned()),
        Body::Draft => Some(format!("{}: ", bot_core::schedule::DELIVERY_CARD_LABEL)),
    };
    let mut bodies: Vec<String> = Vec::new();
    for line in card
        .get("lines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(label) = &first_label {
            if let Some(first) = line.strip_prefix(label.as_str()) {
                bodies.push(first.to_owned());
                continue;
            }
            if let (Some(chunk), Some(quoted)) = (line.strip_prefix("  "), bodies.last_mut()) {
                quoted.push_str(chunk);
                continue;
            }
        }
        out.push(match line.split_once(": ") {
            Some((label, value)) => (label.to_owned(), value.to_owned()),
            None => (String::new(), line.to_owned()),
        });
    }
    if let [joined] = bodies.as_slice() {
        // 풀 수 없는 본문도 행을 주지 않는다(소비자는 이 행이 정확히 하나여야 승인하고 그 지문이 승인 해시와 같아야 한다)
        match body {
            Body::Share => {
                if let Ok(text) = serde_json::from_str::<String>(joined) {
                    out.push(("본문".to_owned(), text));
                }
            }
            Body::Draft => out.push((
                bot_core::schedule::DELIVERY_CARD_LABEL.to_owned(),
                joined.clone(),
            )),
            Body::None => {}
        }
    }
    out
}

/// 기억 공유 카드는 기억 본문 전체를, 예약 회신 카드는 전송 초안 전체를 싣는다: 요청자이자 승인자인 소유자 본인이 아닌 조회자(운영자 포함)에게는
/// 원문을 주지 않고 `render`를 비운다(안내 문구는 화면이 맡는다). 메타데이터(상태·위험·판본·해시)와 감사는 그대로다.
fn render_for(
    viewer: &Viewer,
    approval: &bot_core::state::Approval,
    card: &str,
) -> Vec<(String, String)> {
    let owner = approval.requester == viewer.principal && approval.approver == viewer.principal;
    let body = match approval.tool.0.as_str() {
        crate::share::SHARE_TOOL => Body::Share,
        bot_core::schedule::DELIVER_TOOL => Body::Draft,
        _ => Body::None,
    };
    if body != Body::None && !owner {
        return vec![];
    }
    render_of(card, body)
}

pub(crate) fn approval_view(row: &PgRow, viewer: &Viewer) -> R<ApprovalView> {
    let approval = crate::approvals::approval_of(row)?;
    let render = render_for(viewer, &approval, &row.get::<String, _>("card"));
    Ok(ApprovalView {
        nonce: approval.nonce,
        profile: approval.attempt.profile,
        run: approval.attempt.run,
        generation: approval.attempt.generation,
        call: approval.call,
        requester: approval.requester,
        approver: approval.approver,
        risk: approval.risk,
        tool: approval.tool,
        tool_revision: approval.tool_revision,
        policy_revision: approval.policy_revision,
        args_sha256: approval.args_sha256,
        expires_at: approval.expires_at,
        state: crate::approvals::state_of(&row.get::<String, _>("state"))?,
        render,
        decided_by: row.get::<Option<String>, _>("decided_by").map(PrincipalId),
        decided_at: row.get::<Option<i64>, _>("decided_ms").map(ms_to_time),
    })
}

impl Store {
    /// 승인 목록. 필터를 LIMIT 전에 적용하고 커서는 정렬 키와 같다. 소유자 DM으로 전달 중인(영수증 확정 전) 카드는 대기 행에서 뺀다: 열리지 않은 카드다.
    /// - Pending: 만료가 가까운 순(`expires_at` ASC, `nonce` ASC).
    /// - Decided: 대기를 벗어난 모든 상태(자동 만료·대체 포함), 처리 시각 최신순(시각 없는 행은 뒤, 같은 시각은 `nonce` ASC).
    /// - All: 처리 행이 먼저(Decided와 같은 순서), 날짜 없는 대기 행은 마지막이며 그 묶음은 Pending과 같이 만료가 가까운 순이다.
    pub(crate) async fn admin_approvals(
        &self,
        viewer: &Viewer,
        q: ApprovalQuery,
    ) -> R<Listing<ApprovalView>> {
        let limit = i64::from(q.page.limit.clamp(1, 200));
        let profile = q.profile.as_ref().map(|p| p.0.as_str());
        let rows = if q.filter == ApprovalFilter::Pending {
            let (after_ms, after_id) = cursor(&q.page.after);
            sqlx::query(&format!(
                "select {} from bot_approvals a
                 where ($1::text is null or a.profile = $1) and a.state = 'pending'
                   and not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted')
                   and ($2 or a.requester = $3 or a.approver = $3)
                   and ($4::bigint is null or a.expires_ms > $4
                        or (a.expires_ms = $4 and a.nonce collate \"C\" > $5 collate \"C\"))
                 order by a.expires_ms asc, a.nonce collate \"C\" asc limit $6",
                crate::cols::APPROVALS
            ))
            .bind(profile)
            .bind(viewer.operator)
            .bind(&viewer.principal.0)
            .bind(after_ms)
            .bind(after_id)
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        } else {
            let after = processed_cursor(&q.page.after);
            sqlx::query(
                &format!("select {} from bot_approvals a
                 where ($1::text is null or a.profile = $1)
                   and ($2::text <> 'decided' or a.state <> 'pending')
                   and (a.state <> 'pending'
                        or not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted'))
                   and ($3 or a.requester = $4 or a.approver = $4)
                   and ($5::bigint is null
                        or coalesce(a.decided_ms, -1) < $5
                        or (coalesce(a.decided_ms, -1) = $5 and ((a.state = 'pending')::int > $6
                            or ((a.state = 'pending')::int = $6 and (case when a.state = 'pending' then a.expires_ms else 0 end > $7
                                or (case when a.state = 'pending' then a.expires_ms else 0 end = $7
                                    and a.nonce collate \"C\" > $8 collate \"C\"))))))
                 order by coalesce(a.decided_ms, -1) desc, (a.state = 'pending')::int asc,
                          case when a.state = 'pending' then a.expires_ms else 0 end asc, a.nonce collate \"C\" asc
                 limit $9", crate::cols::prefixed(crate::cols::APPROVALS, "a")),
            )
            .bind(profile)
            .bind(if q.filter == ApprovalFilter::Decided { "decided" } else { "all" })
            .bind(viewer.operator)
            .bind(&viewer.principal.0)
            .bind(after.as_ref().map(|c| c.0))
            .bind(after.as_ref().map_or(0, |c| c.1))
            .bind(after.as_ref().map_or(0, |c| c.2))
            .bind(after.as_ref().map(|c| c.3.clone()).unwrap_or_default())
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        };
        let more = rows.len() as i64 > limit;
        let kept: Vec<&PgRow> = rows.iter().take(limit as usize).collect();
        let items = kept
            .iter()
            .map(|r| approval_view(r, viewer))
            .collect::<R<Vec<_>>>()?;
        let next = more
            .then(|| {
                kept.last().map(|r| {
                    if q.filter == ApprovalFilter::Pending {
                        format!(
                            "{}:{}",
                            r.get::<i64, _>("expires_ms"),
                            r.get::<String, _>("nonce")
                        )
                    } else {
                        let decided = r.get::<Option<i64>, _>("decided_ms").unwrap_or(-1);
                        let pending = r.get::<String, _>("state") == "pending";
                        let expires = if pending {
                            r.get::<i64, _>("expires_ms")
                        } else {
                            0
                        };
                        format!(
                            "{decided}:{}:{expires}:{}",
                            i32::from(pending),
                            r.get::<String, _>("nonce")
                        )
                    }
                })
            })
            .flatten();
        Ok(Listing { items, next })
    }

    pub(crate) async fn admin_approval(
        &self,
        viewer: &Viewer,
        nonce: &ApprovalId,
    ) -> R<ApprovalView> {
        let row = sqlx::query(&format!("select {} from bot_approvals a where a.nonce = $1 and ($2 or a.requester = $3 or a.approver = $3)", crate::cols::APPROVALS))
            .bind(&nonce.0)
            .bind(viewer.operator)
            .bind(&viewer.principal.0)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::Missing)?;
        approval_view(&row, viewer)
    }
}

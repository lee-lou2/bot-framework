//! 지정한 사람의 승인 증거: 접수 때 루트 run에 묶고, 호출 시작 커밋에서만 한 번 소비한다(`authority::human`).
//! 본문·반응 대상 메시지 전문은 저장하지 않는다. 증거 ID는 (프로필, 팀, 이벤트)다.

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

/// 어댑터가 인증한 이번 이벤트의 원천(`extras.source`)과 접수 명령에서 승인 증거를 만들어 접수 커밋 안에서 루트 run에 묶는다.
/// 행위자는 접수의 요청자(사람), 바인딩은 접수의 바인딩이며 대상은 직접 도구여야 한다. 첫 후보 트리거 ID(`extras.chain`)가 없으면 만들지 않는다
/// (증거가 없으면 지정한 사람의 승인은 쓰이지 않고 기존 카드 규칙을 따른다).
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
    /// 이 run(루트)이 접수 때 묶은 증거. 없으면 None. 현재 claim을 확인하는 읽기 전용이다.
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

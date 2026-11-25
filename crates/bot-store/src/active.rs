//! 활성 판본 집합. 호스트가 프로필 활성 집합의 판본을 한 번에 게시한다. 예상 판본이 다르면 전체 교체를 거절한다(CAS).
//! 발송 직전 검사(`Outbox::begin`)가 프로필별 활성 판본을 읽는다.

use crate::memory::{check_seed, seed_audit, seed_in};
use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store},
    schedule::{sync_in, validate_set},
};
use bot_core::{
    command::{MemorySeed, ScheduleSet, TriggerSpec},
    grant::{GrantId, GrantSpec},
    id::*,
    port::Error,
};

use crate::db::Tx;
use std::collections::{BTreeMap, BTreeSet};

/// 게시와 함께 받는 선택 입력: 준비한 목록 대조(없으면 대조하지 않는다)와 사전 승인 범위.
#[derive(Clone, Copy)]
pub(crate) struct Extra<'a> {
    pub prepared: Option<&'a BTreeSet<(ProfileId, Revision)>>,
    pub grants: &'a [GrantSpec],
}

impl Store {
    /// 마지막에 게시된 전체 집합의 판본. 첫 게시 전(행이 없거나 빈 글)에는 None이다.
    pub(crate) async fn current_active(&self) -> R<Option<Revision>> {
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active where singleton")
                .fetch_optional(&self.pool)
                .await?;
        Ok(current.filter(|r| !r.is_empty()).map(Revision))
    }

    /// 예상 판본을 확인한 뒤 활성 집합·소속 프로필의 기억 시드·정적 예약 구간·감사를 **한 트랜잭션**으로 게시한다.
    /// 시드·예약 정의는 새 집합에 든 프로필의 것만 받고(아니면 `Invalid`), 하나라도 규칙을 어기거나 CAS에서 지면 아무것도 남지 않는다.
    /// 같은 (profile, agent, 경로, 내용 해시) 시드는 다시 만들지 않는다.
    /// 예약: `schedules`에 없는 프로필은 정적 예약이 0개다. 모든 구간의 경계는 이 트랜잭션의 같은 DB 시각이고, 집합에서 빠진 프로필의
    /// 열린 구간도 그 시각에 닫는다(앞선 구간의 지난 발생 따라잡기는 그대로다). 늦게 온 옛 게시는 CAS에서 져서 정의를 되살리지 못한다.
    pub(crate) async fn replace_active(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
        extra: Extra<'_>,
    ) -> R<Vec<(GrantId, GrantSpec)>> {
        let Extra { prepared, grants } = extra;
        ids_ok(&[&next.0])?;
        if profiles.iter().any(|(p, r)| !id_ok(&p.0) || !id_ok(&r.0)) {
            return invalid();
        }
        for seed in &seeds {
            if check_seed(seed).is_err() || !profiles.contains_key(&seed.profile) {
                return invalid();
            }
        }
        if schedules.keys().any(|p| !profiles.contains_key(p)) {
            return invalid();
        }
        let sets: Vec<ScheduleSet> = profiles
            .iter()
            .map(|(profile, revision)| ScheduleSet {
                profile: profile.clone(),
                revision: revision.clone(),
                triggers: schedules.get(profile).cloned().unwrap_or_default(),
            })
            .collect();
        for set in &sets {
            validate_set(set)?;
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into bot_active (singleton, revision) values (true, '') on conflict do nothing",
        )
        .execute(&mut *tx)
        .await?;
        let current: String =
            sqlx::query_scalar("select revision from bot_active where singleton for update")
                .fetch_one(&mut *tx)
                .await?;
        // 빈 글은 아직 게시한 적이 없다는 뜻이다
        let current = (!current.is_empty()).then_some(current);
        if current != expected.map(|r| r.0) {
            return Err(Error::Conflict.into());
        }
        // 준비한 목록과 대조한다: 활성 행을 잠근 지금(새 접수는 이 잠금 뒤에서 기다린다) 이 집합의 ID들이 고정한 판본이 모두 준비한 목록에 있어야 한다.
        // 준비 뒤 게시 전에 옛 프로세스가 새 고정 참조를 커밋했다면 아무것도 쓰지 않고 `Stale`이다 — 호출한 쪽이 목록을 다시 준비한다.
        if let Some(prepared) = prepared {
            let required = crate::profile_bundles::pinned_in(&mut *tx, &profiles).await?;
            if !required.is_subset(prepared) {
                return Err(Error::Stale.into());
            }
        }
        let removed: Vec<String> = sqlx::query_scalar("select profile from bot_active_profiles")
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query("delete from bot_active_profiles")
            .execute(&mut *tx)
            .await?;
        for (profile, revision) in &profiles {
            sqlx::query("insert into bot_active_profiles (profile, revision) values ($1, $2)")
                .bind(&profile.0)
                .bind(&revision.0)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "update bot_active set revision = $1, updated_ms = bot_now_ms() where singleton",
        )
        .bind(&next.0)
        .execute(&mut *tx)
        .await?;
        // 정적 예약: 새 집합의 프로필은 새 정의로, 빠진 프로필은 정의 없음으로 같은 시각에 맞춘다(감사 head 잠금보다 먼저)
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        for set in &sets {
            sync_in(&mut tx, set, now).await?;
        }
        let keep: Vec<String> = profiles.keys().map(|p| p.0.clone()).collect();
        let dropped: Vec<String> = sqlx::query_scalar(
            "select distinct profile from bot_schedule_segments where until_ms is null and not (profile = any($1))",
        )
        .bind(&keep)
        .fetch_all(&mut *tx)
        .await?;
        for profile in dropped {
            let empty = ScheduleSet {
                profile: ProfileId(profile),
                revision: Revision(String::new()),
                triggers: Vec::new(),
            };
            sync_in(&mut tx, &empty, now).await?;
        }
        // 사전 승인: 지문이 같으면 ID를 이어 가고, 바뀌었거나 사라졌으면 철회하며(되살아나지 않는다), 처음이거나 바뀌었으면 새 ID를 발급한다(감사 head 잠금보다 먼저)
        let (issued, grant_notes) = crate::grants::sync_in(&mut tx, grants, &profiles).await?;
        // 시드는 감사 head 잠금보다 먼저 넣는다(잠금 순서). 새로 들어간 것만 감사에 남긴다
        let mut fresh = Vec::new();
        for seed in &seeds {
            if seed_in(&mut tx, seed).await? {
                fresh.push(seed);
            }
        }
        for (profile, revision) in &profiles {
            audit::append(
                &mut tx,
                &profile.0,
                &Entry::new("system", "active.replace", &next.0, &revision.0),
            )
            .await?;
        }
        for seed in fresh {
            audit::append(&mut tx, &seed.profile.0, &seed_audit(seed)).await?;
        }
        for note in &grant_notes {
            note.write(&mut tx).await?;
        }
        for gone in removed
            .iter()
            .filter(|p| !profiles.contains_key(&ProfileId((*p).clone())))
        {
            audit::append(
                &mut tx,
                gone,
                &Entry::new("system", "active.replace", &next.0, "removed"),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(issued)
    }
}

/// 권한 검사와 시작·발송 커밋을 활성 판본 교체와 직렬화하는 공통 가드.
/// 교체(`replace_active`)가 `bot_active`를 FOR UPDATE로 잡으므로 여기서는 먼저 FOR SHARE를 잡고(교체가 끝났거나 우리가 커밋할 때까지 서로 기다린다),
/// **별도 문장**으로 프로필의 활성 판본을 읽어 호출자가 검사에 쓴 판본과 대조한다. 없거나 다르면 `Stale`이다.
/// 잠금 순서: bot_active → (루트 run) → run → 호출·승인 → 감사 head.
pub(crate) async fn guard_active(tx: &mut Tx<'_>, profile: &str, checked: &Revision) -> R<()> {
    // 한 번도 게시한 적이 없으면(행이 없으면) 보호할 잠금이 없으므로 바로 거절한다
    sqlx::query("select 1 from bot_active where singleton for share")
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Stale)?;
    let active: Option<String> =
        sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
            .bind(profile)
            .fetch_optional(&mut **tx)
            .await?;
    if active.as_deref() == Some(checked.0.as_str()) {
        Ok(())
    } else {
        Err(Error::Stale.into())
    }
}

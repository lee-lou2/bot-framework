//! 프로필 판본의 검증한 바이트 묶음(재배포 뒤 옛 판본 복원의 근거)과 작업이 고정한 판본 목록.
//! 묶음은 `(프로필, 판본)`으로 한 번만 쓰고 바꾸거나 지우지 않는다. 판본은 묶음의 SHA-256이라 DB가 키와 바이트가 맞는지 강제한다.

use crate::{
    codec::*,
    db::{Fail, R, Store},
};
use bot_core::{
    id::{ProfileId, Revision},
    port::{Error, Reply},
    profile::ProfileBundles,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};

/// 묶음의 판본 = 바이트의 SHA-256 소문자 16진수.
fn revision_of(bundle: &[u8]) -> String {
    Sha256::digest(bundle)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 주어진 프로필 ID들이 고정한 (프로필, 판본)을 **한 문장**(한 스냅숏)에서 읽는다: 예약 구간이 새 실행으로 넘어가는 순간에도 둘 중 하나는 반드시 보인다.
/// - 끝나지 않은 실행(대기 포함)과 운영자가 재개할 수 있는 dead letter(비공개 접수는 재개할 수 없다).
/// - 대기·승인 미소비 카드가 묶인 실행.
/// - 정적 cron의 열린 구간과 커서가 구간 끝에 이르지 않은 닫힌 구간.
/// - 활성 사용자 예약이 참조하는 생성 실행.
///
/// 집합에 없는 프로필의 참조는 읽지 않고, 어떤 작업도 참조하지 않는 옛 판본은 DB의 활성 판본이라는 이유만으로 넣지 않는다.
const PINNED: &str = "
    select profile, revision from bot_runs
     where profile = any($1) and (state in ('ready', 'leased', 'waiting', 'retry') or (state = 'dead' and not private))
    union
    select a.profile, r.revision from bot_approvals a
      join bot_runs r on r.profile = a.profile and r.id = a.run
     where a.profile = any($1) and a.state in ('pending', 'approved')
    union
    select s.profile, s.revision from bot_schedule_segments s
      join bot_schedule_cursors c on c.profile = s.profile and c.agent = s.agent and c.trigger = s.trigger
     where s.profile = any($1) and (s.until_ms is null or s.until_ms > c.cursor_ms)
    union
    select u.profile, r.revision from bot_user_schedules u
      join bot_runs r on r.profile = u.profile and r.id = u.origin_run
     where u.profile = any($1) and u.state = 'active'";

/// `target`의 ID들이 고정한 판본 ∪ `target`의 현재 쌍. 풀이든 트랜잭션이든 같은 정의로 읽는다(게시 확정 때는 활성 행 잠금 아래 트랜잭션에서 다시 읽는다).
pub(crate) async fn pinned_in<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    target: &BTreeMap<ProfileId, Revision>,
) -> R<BTreeSet<(ProfileId, Revision)>> {
    let ids: Vec<&str> = target.keys().map(|p| p.0.as_str()).collect();
    let rows = sqlx::query(PINNED).bind(&ids).fetch_all(executor).await?;
    let mut pinned: BTreeSet<(ProfileId, Revision)> = rows
        .iter()
        .map(|row| (ProfileId(row.get("profile")), Revision(row.get("revision"))))
        .collect();
    pinned.extend(target.iter().map(|(p, r)| (p.clone(), r.clone())));
    Ok(pinned)
}

impl Store {
    pub(crate) async fn bundle_put(
        &self,
        profile: &ProfileId,
        revision: &Revision,
        bundle: &[u8],
    ) -> R<()> {
        ids_ok(&[&profile.0, &revision.0])?;
        if revision.0 != revision_of(bundle) {
            return invalid();
        }
        sqlx::query(
            "insert into bot_profile_bundles (profile, revision, bundle) values ($1, $2, $3)
             on conflict (profile, revision) do nothing",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .bind(bundle)
        .execute(&self.pool)
        .await?;
        // 이미 있던 행이면 바이트가 같은지 DB에서 대조한다(같은 키에 다른 바이트는 손상이다). 큰 묶음을 다시 내려받지 않는다
        let same: bool = sqlx::query_scalar(
            "select bundle = $3 from bot_profile_bundles where profile = $1 and revision = $2",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .bind(bundle)
        .fetch_one(&self.pool)
        .await?;
        if !same {
            return Err(Fail::Core(Error::Conflict));
        }
        Ok(())
    }

    pub(crate) async fn bundle_get(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> R<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar(
            "select bundle from bot_profile_bundles where profile = $1 and revision = $2",
        )
        .bind(&profile.0)
        .bind(&revision.0)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub(crate) async fn pinned(
        &self,
        target: &BTreeMap<ProfileId, Revision>,
    ) -> R<BTreeSet<(ProfileId, Revision)>> {
        pinned_in(&self.pool, target).await
    }
}

impl ProfileBundles for Store {
    fn put_bundle(&self, profile: ProfileId, revision: Revision, bundle: Vec<u8>) -> Reply<'_, ()> {
        Box::pin(async move {
            self.bundle_put(&profile, &revision, &bundle)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn bundle(&self, profile: &ProfileId, revision: &Revision) -> Reply<'_, Option<Vec<u8>>> {
        let (profile, revision) = (profile.clone(), revision.clone());
        Box::pin(async move {
            self.bundle_get(&profile, &revision)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn pinned_revisions<'a>(
        &'a self,
        target: &'a BTreeMap<ProfileId, Revision>,
    ) -> Reply<'a, BTreeSet<(ProfileId, Revision)>> {
        Box::pin(async move { self.pinned(target).await.map_err(Fail::into_core) })
    }
}

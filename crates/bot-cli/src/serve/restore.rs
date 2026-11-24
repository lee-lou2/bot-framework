//! 재배포 복원: 활성 집합을 게시하기 전에 ① 새 집합의 검증한 묶음을 저장소에 보관하고 ② 진행 중 작업이 고정한 옛 판본을 묶음에서 복원해 등록한다.
//! 첫 기동·같은 집합에 합류하는 기동·`--activate` 교체 모두 역할을 열기 전에, 판본을 게시하기 전에 끝낸다 — 실패하면 활성 집합·실행·대기 카드·시도 횟수는 그대로다.
//! 복원 대상은 이번에 게시·합류할 집합의 프로필 ID가 고정한 판본이다(`--activate`는 교체 뒤의 후보 집합, 재기동·합류는 확인한 현 집합). 집합에서 통째로 빠진 프로필의 작업은
//! 되살리지 않고, 그 프로필의 번들이 없거나 손상돼도 다른 프로필의 기동을 막지 않는다. 준비한 뒤 게시 전에 옛 프로세스가 새 고정 참조를 커밋할 수 있으므로,
//! 게시 확정은 활성 행 잠금 아래에서 목록을 다시 대조하고(`ActiveRevisions::replace_prepared`) 어긋나면 목록을 다시 준비한다(`refresh`).

use bot_core::{
    id::{ProfileId, Revision},
    port::Error,
    profile::ProfileBundles,
};
use bot_profile::{Limits, ProblemKind, ProfileSet, Registry, Restored};
use std::collections::{BTreeMap, BTreeSet};

/// 준비한 결과: 복원한 옛 판본의 강한 참조(등록은 약한 참조라 역할이 도는 내내 쥐고 있어야 한다)와, 검증·보관·복원을 마친 (프로필, 판본) 목록.
#[derive(Debug, Default)]
pub struct Prepared {
    held: Vec<Restored>,
    covered: BTreeSet<(ProfileId, Revision)>,
}

impl Prepared {
    /// 게시 확정 때 활성 행 잠금 아래에서 다시 읽은 필수 판본이 모두 이 안에 있어야 한다.
    pub fn covered(&self) -> &BTreeSet<(ProfileId, Revision)> {
        &self.covered
    }

    /// 복원한 옛 판본 수.
    pub fn restored(&self) -> usize {
        self.held.iter().map(Restored::len).sum()
    }
}

/// 새 집합의 묶음을 먼저 보관한다. 새 활성 집합이 보일 때는 그 집합의 모든 묶음이 이미 저장돼 있어야 한다.
/// 같은 키로 같은 바이트는 다시 보관해도 되고(합류·재시작), 이전 버전이 보관하지 않은 현재 판본은 이때 채워진다.
pub async fn store_current(bundles: &dyn ProfileBundles, set: &ProfileSet) -> Result<(), String> {
    for (profile, snapshot) in set.active().iter() {
        let Some(bundle) = set.bundle(profile) else {
            return Err(format!(
                "프로필 {}의 검증한 묶음이 없어요 — 활성 판본은 바뀌지 않았어요",
                profile.0
            ));
        };
        bundles
            .put_bundle(profile.clone(), snapshot.revision.clone(), bundle.to_vec())
            .await
            .map_err(|error| match error {
                Error::Conflict => format!(
                    "프로필 {} 판본 {}: 저장소에 같은 판본 번호로 다른 묶음이 이미 있어요 — 저장소가 손상됐을 수 있어요. 활성 판본은 바뀌지 않았어요",
                    profile.0, snapshot.revision.0
                ),
                Error::Unavailable => format!(
                    "프로필 {} 판본 {}의 묶음을 저장소에 보관하지 못했어요(저장소에 닿지 못했어요) — 활성 판본은 바뀌지 않았어요",
                    profile.0, snapshot.revision.0
                ),
                _ => format!(
                    "프로필 {} 판본 {}의 묶음을 저장소가 받지 않았어요 — 활성 판본은 바뀌지 않았어요",
                    profile.0, snapshot.revision.0
                ),
            })?;
    }
    Ok(())
}

/// 복원할 판본: 고정 판본 목록 중 이번 집합에 든 프로필의 것(저장소가 이미 ID로 거르지만 여기서도 한 번 더 막는다).
pub fn wanted(
    pinned: BTreeSet<(ProfileId, Revision)>,
    set: &ProfileSet,
) -> BTreeSet<(ProfileId, Revision)> {
    pinned
        .into_iter()
        .filter(|(profile, _)| set.active().contains_key(profile))
        .collect()
}

/// 새 집합의 묶음을 보관하고, 진행 중 작업이 고정한 옛 판본을 복원해 `registry`에 등록한다.
/// 돌려받은 `Prepared`는 역할이 도는 내내(프로세스가 끝날 때까지) 쥐고 있어야 한다.
pub async fn prepare(
    bundles: &dyn ProfileBundles,
    registry: &Registry,
    set: &ProfileSet,
    target: &BTreeMap<ProfileId, Revision>,
    limits: &Limits,
) -> Result<Prepared, String> {
    store_current(bundles, set).await?;
    let mut prepared = Prepared::default();
    refresh(bundles, registry, set, target, limits, &mut prepared).await?;
    Ok(prepared)
}

/// 고정 판본 목록을 다시 읽어 새로 필요해진 옛 판본을 복원하고 준비한 목록을 넓힌다(준비 뒤 게시 전에 새 고정 참조가 생겼을 때).
/// 이미 복원했거나 서빙하는 판본은 다시 읽지 않는다.
pub async fn refresh(
    bundles: &dyn ProfileBundles,
    registry: &Registry,
    set: &ProfileSet,
    target: &BTreeMap<ProfileId, Revision>,
    limits: &Limits,
    prepared: &mut Prepared,
) -> Result<(), String> {
    let pinned = bundles.pinned_revisions(target).await.map_err(|error| {
        format!(
            "진행 중 작업이 고정한 판본을 읽지 못했어요({}) — 활성 판본은 바뀌지 않았어요",
            match error {
                Error::Unavailable => "저장소에 닿지 못했어요",
                _ => "저장소가 거절했어요",
            }
        )
    })?;
    let wanted = wanted(pinned, set);
    let restored = registry
        .restore(bundles, &wanted, limits)
        .await
        .map_err(|failure| {
            let damaged = failure
                .problems
                .iter()
                .any(|p| matches!(p.kind, ProblemKind::Unreadable(_)));
            let missing = failure
                .problems
                .iter()
                .any(|p| matches!(p.kind, ProblemKind::Missing));
            format!(
                "진행 중인 작업이 쓰는 옛 판본을 복원하지 못해 시작하지 않아요 — 활성 판본·실행·대기 카드·시도 횟수는 바뀌지 않았어요:\n{failure}\n\
                 이 판본을 쓰는 실행·예약 구간·승인 카드가 있어요.{}{}",
                if missing {
                    " 그 판본의 원본 프로필 폴더가 있으면 `bot profile-bundle put <원본 프로필 폴더>`로 보관한 뒤 다시 시작하세요(프로필 ID와 SHA-256이 같은 원본만 그 판본을 채워요)."
                } else {
                    ""
                },
                if damaged {
                    " 손상된 묶음은 `bot profile-bundle put`이 덮어쓰지 않아요(보관한 묶음은 바뀌지 않아요) — 저장소를 백업에서 복구하세요."
                } else {
                    ""
                }
            )
        })?;
    prepared.covered.extend(wanted);
    prepared.held.push(restored);
    Ok(())
}

/// 지금의 고정 판본 목록이 준비한 목록 안에 있는가(합류가 역할을 열기 전에 집합·참조가 맞는지 보는 읽기 전용 대조).
pub async fn covers(
    bundles: &dyn ProfileBundles,
    set: &ProfileSet,
    target: &BTreeMap<ProfileId, Revision>,
    prepared: &Prepared,
) -> Result<bool, String> {
    let pinned = bundles.pinned_revisions(target).await.map_err(|_| {
        "진행 중 작업이 고정한 판본을 다시 읽지 못했어요(저장소에 닿지 못했어요)".to_owned()
    })?;
    Ok(wanted(pinned, set).is_subset(prepared.covered()))
}

#[cfg(test)]
mod tests;

//! 활성 판본 게시 결정과 확정. 1단계(`plan_activation`, 읽기만)는 저장소의 현재 집합 판본과 이 프로필 폴더·입구 매핑의 판본을 견주어 합류·첫 게시·교체·거절을 정하고,
//! 2단계(`commit_with_refresh`, 쓰기)는 역할 점검·리스너 확보를 모두 끝낸 뒤에만 게시한다. 게시는 활성 행 잠금 아래에서 «이 집합의 프로필 ID가 고정한 판본이 모두 준비한 목록 안에 있는지»
//! 다시 대조하고, 준비 뒤 게시 전에 옛 프로세스가 새 고정 참조를 커밋했다면 목록을 다시 준비해 몇 번 더 시도한다. 합류(이미 게시된 같은 집합, 그리고 쓰려다 다른 프로세스가 같은 집합을 먼저 게시한 경우)도
//! 역할을 열기 전에 같은 대조를 하고, 빠진 번들은 복원하며, 못 하면 시작하지 않는다.

use super::{Publication, explain, restore};
use bot_core::{id::Revision, port::Error, profile::ActiveRevisions};
use bot_profile::{ProfileSet, Registry};
use bot_store::Store;

/// 활성 판본 게시 결정(순수). 저장소의 현재 집합 판본과 이 프로필 폴더·입구 매핑의 판본을 견준다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Activation {
    /// 같은 집합이 이미 게시돼 있다 — 쓰기 없이 합류한다(재시작·여러 역할).
    Join,
    /// 아직 게시된 적이 없다 — 처음 게시한다(CAS 예상값 없음).
    Publish,
    /// 명시 활성화: 현재 판본을 예상값으로 교체한다.
    Replace(Revision),
    Refuse(String),
}

fn decide_activation(current: Option<&Revision>, local: &Revision, explicit: bool) -> Activation {
    match current {
        Some(current) if current == local => Activation::Join,
        None => Activation::Publish,
        Some(current) if explicit => Activation::Replace(current.clone()),
        Some(current) => Activation::Refuse(format!(
            "저장소의 활성 판본({})이 이 프로필 폴더·호스트 입구 설정({})과 달라요 — 구판이라면 폴더·호스트 설정을 맞추고, 새 판본이면 --activate로 명시 활성화하세요",
            short(&current.0),
            short(&local.0)
        )),
    }
}

fn short(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

fn read_failed(error: Error) -> String {
    format!("활성 판본을 읽지 못했어요: {}", explain(error))
}

/// 1단계(읽기만): 저장소의 현재 판본과 이 폴더·입구 설정의 판본을 견주어 할 일을 정한다. 거절이면 아무것도 열기 전에 Err.
pub(super) async fn plan_activation(
    store: &Store,
    publication: &Publication,
    explicit: bool,
) -> Result<Activation, String> {
    let current = store.current().await.map_err(read_failed)?;
    match decide_activation(current.as_ref(), &publication.revision, explicit) {
        Activation::Refuse(message) => Err(message),
        plan => Ok(plan),
    }
}

/// 이미 게시된 같은 집합에 합류하기 전의 확인(쓰기 없음): 지금의 고정 판본이 모두 준비한 목록 안에 있어야 역할을 열 수 있다. 아니면 `Stale`이다 —
/// 호출한 쪽이 목록을 다시 준비하고(빠진 번들을 복원하고, 못 하면 시작하지 않는다) 다시 시도한다.
/// 게시가 보인 뒤에는 옛 판본으로 새 실행을 접수하는 쪽이 `Stale`로 거절되므로(접수가 활성 행을 공유 잠금으로 확인한다) 필수 목록은 더 늘지 않는다 — 이 읽기 한 번으로 충분하다.
async fn join(
    store: &Store,
    set: &ProfileSet,
    publication: &Publication,
    prepared: &restore::Prepared,
) -> Result<bool, Commit> {
    match restore::covers(store, set, &publication.profiles, prepared).await? {
        true => Ok(false),
        false => Err(Commit::Stale),
    }
}

/// 게시 확정이 끝나지 못한 이유.
pub(super) enum Commit {
    /// 준비한 목록 밖의 새 고정 참조가 생겼다(준비 뒤 게시 전에 옛 프로세스가 커밋했다): 목록을 다시 준비하고 다시 시도한다. 아무것도 쓰지 않았다.
    Stale,
    Failed(String),
}

impl From<String> for Commit {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// 2단계(쓰기): 활성 집합의 판본을 저장소에 합류·게시·교체한다(발송·접수가 현재 판본을 확인하는 기준).
/// 교체는 명시 활성화(`--activate`)만 하고 예상값(현재 판본) CAS로 한다. 쓰기가 일어났으면 true(예약·시드 동기화 신호).
/// 점검·리스너 확보를 모두 끝낸 뒤에만 부른다 — 시작이 실패해도 운영 판본을 바꾸지 않기 위해서다.
/// 게시는 활성 행 잠금 아래에서 «이 집합의 프로필 ID가 고정한 판본이 모두 준비한 목록 안에 있는지» 다시 대조한다. 합류(처음부터 같은 집합이거나, 쓰려다 다른 프로세스가 같은 집합을 먼저 게시한 경우)는 쓰기 없이 같은 대조만 읽기로 한다.
async fn commit_activation(
    store: &Store,
    publication: &Publication,
    plan: Activation,
    set: &ProfileSet,
    prepared: &restore::Prepared,
) -> Result<bool, Commit> {
    let local = publication.revision.clone();
    let profiles = publication.profiles.clone();
    let seeds = publication.seeds.clone();
    let schedules = publication.schedules.clone();
    let read = read_failed;
    let (expected, wrote) = match plan {
        // 쓰기는 없지만 역할을 열기 전에 참조가 맞아야 한다
        Activation::Join => return join(store, set, publication, prepared).await,
        Activation::Refuse(message) => return Err(message.into()),
        Activation::Publish => (None, true),
        Activation::Replace(expected) => (Some(expected), true),
    };
    match store
        .replace_prepared(
            expected,
            local.clone(),
            profiles,
            seeds,
            schedules,
            prepared.covered().clone(),
        )
        .await
    {
        Ok(()) => Ok(wrote),
        Err(Error::Stale) => Err(Commit::Stale),
        // 그 사이 다른 쪽이 게시했다 — 다시 읽어 같은 집합이면 합류하고 아니면 멈춘다.
        // 합류도 준비한 목록과 지금의 필수 판본을 대조한다: 이 프로세스의 목록은 그 게시 전에 읽은 것이라, 그 뒤에 옛 프로세스가 접수한 옛 판본의 실행은 빠져 있을 수 있다
        // (게시한 쪽은 `Stale`을 겪고 다시 준비해 복원했지만 이쪽은 아니다). 빠졌으면 `Stale`로 돌려 목록을 다시 준비한 뒤에만 연다.
        Err(Error::Conflict) => match store.current().await.map_err(read)? {
            Some(now) if now == local => join(store, set, publication, prepared).await,
            _ => Err(String::from("그 사이 저장소의 활성 판본이 바뀌었어요 — 다시 시도하세요").into()),
        },
        Err(Error::Invalid) => Err(String::from(
            "저장소가 판본·기억 시드·예약 정의를 받지 못했어요 — 활성 판본과 기억·예약은 바뀌지 않았어요",
        )
        .into()),
        Err(other) => Err(format!("활성 판본을 게시하지 못했어요: {}", explain(other)).into()),
    }
}

/// 게시 확정을 시도한다. 준비 뒤 게시 전에 옛 프로세스가 새 고정 참조를 커밋해 목록이 어긋나면(쓰기 0) 목록을 다시 준비해 몇 번 더 시도한다.
pub(super) async fn commit_with_refresh(
    store: &Store,
    registry: &Registry,
    publication: &Publication,
    plan: &Activation,
    set: &ProfileSet,
    prepared: &mut restore::Prepared,
) -> Result<bool, String> {
    for attempt in 1..=COMMIT_ATTEMPTS {
        let committed = commit_activation(store, publication, plan.clone(), set, prepared).await;
        match after_commit(committed, attempt) {
            Next::Done(result) => return result,
            Next::Refresh => {
                restore::refresh(
                    store,
                    registry,
                    set,
                    &publication.profiles,
                    &bot_profile::Limits::default(),
                    prepared,
                )
                .await?;
            }
        }
    }
    unreachable!("마지막 시도는 Stale이어도 Done으로 끝난다")
}

/// 확정 한 번의 결과로 다음에 할 일.
#[derive(Debug, Eq, PartialEq)]
enum Next {
    Done(Result<bool, String>),
    /// 준비한 목록 밖의 새 고정 참조(쓰기 0): 목록을 다시 준비하고 다시 시도한다.
    Refresh,
}

/// 확정이 `Stale`이고 시도가 남았으면 다시 준비하고, 한도에 이르면 활성 판본이 바뀌지 않았다고 알리며 멈춘다. 다른 결과는 그대로 끝낸다.
fn after_commit(committed: Result<bool, Commit>, attempt: usize) -> Next {
    match committed {
        Ok(wrote) => Next::Done(Ok(wrote)),
        Err(Commit::Failed(message)) => Next::Done(Err(message)),
        Err(Commit::Stale) if attempt < COMMIT_ATTEMPTS => Next::Refresh,
        Err(Commit::Stale) => Next::Done(Err("준비하는 사이에도 옛 판본을 쓰는 작업이 계속 생겨 게시하지 못했어요 — 활성 판본은 바뀌지 않았어요. 잠시 뒤 다시 시도하세요".to_owned())),
    }
}

/// 준비 뒤 게시 전에 새 고정 참조가 생겨 목록을 다시 준비하는 최대 횟수.
const COMMIT_ATTEMPTS: usize = 4;

#[cfg(test)]
mod race;

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn rev(text: &str) -> Revision {
        Revision(text.into())
    }

    fn activation_revision(set: &Revision, mapping: &[u8; 32]) -> Revision {
        super::super::activation_revision(set, mapping)
    }

    #[test]
    fn the_activation_revision_differs_from_the_one_an_older_publication_format_made() {
        // v1(예약 정의를 판본과 함께 올리지 않던 때)의 같은 입력과 달라야, 업그레이드 뒤 첫 `--activate`가 한 번 교체해 새 내용을 올린다
        let (set, mapping) = (rev("set-1"), [7u8; 32]);
        let mut legacy = Sha256::new();
        legacy.update(b"bot-active-v1\0");
        legacy.update((set.0.len() as u64).to_be_bytes());
        legacy.update(set.0.as_bytes());
        legacy.update(mapping);
        let legacy = Revision(format!("{:x}", legacy.finalize()));
        let now = activation_revision(&set, &mapping);
        assert_ne!(now, legacy);
        // 옛 판본이 게시돼 있으면 `--activate`는 교체하고, 없이 시작하면 거절한다(합류가 아니다)
        assert_eq!(
            decide_activation(Some(&legacy), &now, true),
            Activation::Replace(legacy.clone())
        );
        assert!(matches!(
            decide_activation(Some(&legacy), &now, false),
            Activation::Refuse(_)
        ));
    }

    #[test]
    fn a_stale_commit_refreshes_the_prepared_list_and_tries_again_a_few_times_at_most() {
        // Stale이면 시도가 남은 동안 다시 준비한다
        for attempt in 1..COMMIT_ATTEMPTS {
            assert_eq!(after_commit(Err(Commit::Stale), attempt), Next::Refresh);
        }
        // 한도에 이르면 멈추고 활성 판본이 바뀌지 않았다고 알린다
        match after_commit(Err(Commit::Stale), COMMIT_ATTEMPTS) {
            Next::Done(Err(message)) => assert!(message.contains("활성 판본은 바뀌지 않았어요")),
            other => panic!("{other:?}"),
        }
        // 성공은 쓰기 여부 그대로, 다른 실패는 다시 준비하지 않고 그대로 돌려준다
        assert_eq!(after_commit(Ok(true), 1), Next::Done(Ok(true)));
        assert_eq!(after_commit(Ok(false), 2), Next::Done(Ok(false)));
        assert_eq!(
            after_commit(Err(Commit::Failed("저장소가 거절했어요".into())), 1),
            Next::Done(Err("저장소가 거절했어요".to_owned()))
        );
    }

    #[test]
    fn same_set_joins_without_writing_and_first_start_publishes() {
        assert_eq!(
            decide_activation(Some(&rev("a")), &rev("a"), false),
            Activation::Join
        );
        assert_eq!(
            decide_activation(Some(&rev("a")), &rev("a"), true),
            Activation::Join
        );
        assert_eq!(
            decide_activation(None, &rev("a"), false),
            Activation::Publish
        );
        assert_eq!(
            decide_activation(None, &rev("a"), true),
            Activation::Publish
        );
    }

    #[test]
    fn a_different_published_set_is_replaced_only_by_explicit_activation() {
        let refused = decide_activation(
            Some(&rev("old-set-aaaaaaaa")),
            &rev("new-set-bbbbbbbb"),
            false,
        );
        match refused {
            Activation::Refuse(message) => {
                assert!(message.contains("--activate") && message.contains("old-set-aaaa"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decide_activation(Some(&rev("old")), &rev("new"), true),
            Activation::Replace(rev("old")),
            "예상값은 지금 게시된 판본이다"
        );
    }
}

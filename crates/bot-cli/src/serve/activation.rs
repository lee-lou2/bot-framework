//! Two-phase active revision planning and CAS commit with pinned-bundle refresh.
use super::{Publication, explain, restore};
use bot_core::{id::Revision, port::Error, profile::ActiveRevisions};
use bot_profile::{ProfileSet, Registry};
use bot_store::Store;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Activation {
    Join,
    Publish,
    Replace(Revision),
    Refuse(String),
}

fn decide_activation(current: Option<&Revision>, local: &Revision, explicit: bool) -> Activation {
    match current {
        Some(current) if current == local => Activation::Join,
        None => Activation::Publish,
        Some(current) if explicit => Activation::Replace(current.clone()),
        Some(current) => Activation::Refuse(format!(
            "active revision in store ({}) differs from this profile directory and host config ({}) — align the files if stale, or pass --activate to replace it",
            short(&current.0),
            short(&local.0)
        )),
    }
}

fn short(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

fn read_failed(error: Error) -> String {
    format!("failed to read active revision: {}", explain(error))
}

/// Phase 1 (read-only): compare the store's active revision with local config before opening roles.
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

/// Verifies before joining that all currently pinned revisions are covered by `prepared`; returns `Stale` to trigger a refresh.
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

pub(super) enum Commit {
    /// A new pinned reference committed after prepare; caller refreshes and retries without side effects.
    Stale,
    Failed(String),
}

impl From<String> for Commit {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// Phase 2 (write): join, publish, or CAS-replace the active revision after checks and listeners succeed.
async fn commit_activation(
    store: &Store,
    publication: &Publication,
    plan: Activation,
    set: &ProfileSet,
    prepared: &restore::Prepared,
) -> Result<bool, Commit> {
    let local = publication.revision.clone();
    let grants = &publication.grants;
    let read = read_failed;
    let (mut expected, mut wrote) = match plan {
        // Explicit `--activate` on an already-active set still reconciles unissued pre-approval grants.
        Activation::Join if !grants.is_empty() => (Some(local.clone()), false),
        Activation::Join => return join(store, set, publication, prepared).await,
        Activation::Refuse(message) => return Err(message.into()),
        Activation::Publish => (None, true),
        Activation::Replace(expected) => (Some(expected), true),
    };
    loop {
        match store
            .replace_granted(
                expected.clone(),
                local.clone(),
                publication.profiles.clone(),
                publication.seeds.clone(),
                publication.schedules.clone(),
                bot_core::grant::GrantPlan {
                    prepared: prepared.covered().clone(),
                    grants: grants.clone(),
                },
            )
            .await
        {
            Ok(issued) => {
                for (id, spec) in issued {
                    eprintln!("Issued pre-approval {} — {}", id.0, spec.scope_line());
                }
                return Ok(wrote);
            }
            Err(Error::Stale) => return Err(Commit::Stale),
            // Lost CAS race: re-read to join if a peer published the exact same set, else fail.
            Err(Error::Conflict) => match store.current().await.map_err(read)? {
                Some(now) if now == local && grants.is_empty() => {
                    return join(store, set, publication, prepared).await;
                }
                // Lost race to a peer that published the same set without `--activate`; reconcile grants once.
                Some(now) if now == local && expected.as_ref() != Some(&local) => {
                    expected = Some(local.clone());
                    wrote = false;
                }
                _ => {
                    return Err(String::from(
                        "active revision in store changed concurrently — retry",
                    )
                    .into());
                }
            },
            Err(Error::Invalid) => {
                return Err(String::from(
                    "store rejected revision, memory seeds, or schedules — active revision was not changed",
                )
                .into());
            }
            Err(other) => {
                return Err(
                    format!("failed to publish active revision: {}", explain(other)).into(),
                );
            }
        }
    }
}

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
    unreachable!("final attempt returns Done even on Stale")
}

#[derive(Debug, Eq, PartialEq)]
enum Next {
    Done(Result<bool, String>),
    Refresh,
}

fn after_commit(committed: Result<bool, Commit>, attempt: usize) -> Next {
    match committed {
        Ok(wrote) => Next::Done(Ok(wrote)),
        Err(Commit::Failed(message)) => Next::Done(Err(message)),
        Err(Commit::Stale) if attempt < COMMIT_ATTEMPTS => Next::Refresh,
        Err(Commit::Stale) => Next::Done(Err(
            "runs using older revisions kept arriving during preparation — active revision was not changed; retry shortly"
                .to_owned(),
        )),
    }
}

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
        let (set, mapping) = (rev("set-1"), [7u8; 32]);
        let mut legacy = Sha256::new();
        legacy.update(b"bot-active-v1\0");
        legacy.update((set.0.len() as u64).to_be_bytes());
        legacy.update(set.0.as_bytes());
        legacy.update(mapping);
        let legacy = Revision(format!("{:x}", legacy.finalize()));
        let now = activation_revision(&set, &mapping);
        assert_ne!(now, legacy);
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
        for attempt in 1..COMMIT_ATTEMPTS {
            assert_eq!(after_commit(Err(Commit::Stale), attempt), Next::Refresh);
        }
        match after_commit(Err(Commit::Stale), COMMIT_ATTEMPTS) {
            Next::Done(Err(message)) => {
                assert!(message.contains("active revision was not changed"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(after_commit(Ok(true), 1), Next::Done(Ok(true)));
        assert_eq!(after_commit(Ok(false), 2), Next::Done(Ok(false)));
        assert_eq!(
            after_commit(Err(Commit::Failed("store rejected".into())), 1),
            Next::Done(Err("store rejected".to_owned()))
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
            "expected value is the currently published revision"
        );
    }
}

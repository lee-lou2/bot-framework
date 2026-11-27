//! Restores historical profile revisions from stored bundles into `Registry` for in-flight run resumption.
use crate::{
    build,
    issue::{Issue, Issues},
    limits::Limits,
    registry::{Registry, Report},
    scan,
    snapshot::{BundleFormat, from_bundle},
};
use bot_core::{
    id::{ProfileId, Revision},
    port::Error,
    profile::{ProfileBundles, ProfileCatalog, Snapshot},
};
use std::{collections::BTreeSet, fmt, path::Path, sync::Arc};

/// Builds a validated bundle `(profile_id, revision, bytes)` from a directory without checking the
/// host catalog, used to backfill missing historical bundles.
pub fn read_bundle(dir: &Path, limits: &Limits) -> Result<(ProfileId, Revision, Vec<u8>), Report> {
    read_bundle_with(dir, None, limits)
}

/// Builds a `BOTPROFILE1` bundle from a directory to reproduce legacy SHA-256 revisions (`bot profile-bundle put`).
pub fn read_legacy_bundle(
    dir: &Path,
    limits: &Limits,
) -> Result<(ProfileId, Revision, Vec<u8>), Report> {
    read_bundle_with(dir, Some(BundleFormat::V1), limits)
}

fn read_bundle_with(
    dir: &Path,
    format: Option<BundleFormat>,
    limits: &Limits,
) -> Result<(ProfileId, Revision, Vec<u8>), Report> {
    let mut issues = Issues::new(limits.max_issues);
    let name = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    if name.is_none() {
        issues.error(
            "dir-missing",
            &dir.display().to_string(),
            "cannot open profile directory",
        );
        return Err(Report::from_issues(issues));
    }
    let tree = scan::read_tree(dir, "", limits, &mut issues);
    let built = if issues.has_errors() {
        None
    } else {
        match format {
            Some(fmt) => build::build_for_bundle(fmt, &tree, name.as_deref(), limits, &mut issues),
            None => build::build(&tree, name.as_deref(), limits, &mut issues),
        }
    };
    match built {
        Some(out) if !issues.has_errors() => {
            Ok((out.snapshot.id, out.snapshot.revision, out.bundle))
        }
        _ => Err(Report::from_issues(issues)),
    }
}

#[derive(Debug)]
pub enum ProblemKind {
    Missing,
    Unreadable(Vec<Issue>),
    Store(Error),
}

#[derive(Debug)]
pub struct Problem {
    pub profile: ProfileId,
    pub revision: Revision,
    pub kind: ProblemKind,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the full SHA-256 so operators know which exact revision bundle to supply.
        write!(
            f,
            "profile {} revision {}: ",
            self.profile.0, self.revision.0
        )?;
        match &self.kind {
            ProblemKind::Missing => write!(f, "stored bundle is missing"),
            ProblemKind::Unreadable(issues) => {
                let first = issues.iter().find(|i| i.is_error());
                match first {
                    Some(issue) => write!(
                        f,
                        "stored bundle is unreadable [{}] {}",
                        issue.code, issue.message
                    ),
                    None => write!(f, "stored bundle is unreadable"),
                }
            }
            ProblemKind::Store(Error::Unavailable) => write!(f, "store is unavailable"),
            ProblemKind::Store(_) => write!(f, "store failed to return bundle"),
        }
    }
}

#[derive(Debug)]
pub struct RestoreFailure {
    pub problems: Vec<Problem>,
}

impl fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, problem) in self.problems.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{problem}")?;
        }
        Ok(())
    }
}

/// Strong guard keeping restored snapshots alive in `Registry` (which only holds weak refs).
#[derive(Debug, Default)]
pub struct Restored {
    held: Vec<Arc<Snapshot>>,
}

impl Restored {
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn revisions(&self) -> impl Iterator<Item = (&ProfileId, &Revision)> {
        self.held.iter().map(|s| (&s.id, &s.revision))
    }
}

impl Registry {
    /// Restores missing `(profile, revision)` pairs from `bundles` and adopts them atomically for `lookup`.
    /// Fails without adopting anything if any requested revision cannot be restored.
    pub async fn restore(
        &self,
        bundles: &dyn ProfileBundles,
        wanted: &BTreeSet<(ProfileId, Revision)>,
        limits: &Limits,
    ) -> Result<Restored, RestoreFailure> {
        let mut held = Vec::new();
        let mut problems = Vec::new();
        for (profile, revision) in wanted {
            if self.lookup(profile, revision).is_some() {
                continue;
            }
            let found = bundles.bundle(profile, revision).await;
            let kind = match found {
                Ok(Some(bytes)) => match from_bundle(&bytes, profile, revision, limits) {
                    Ok(snapshot) => {
                        held.push(Arc::new(snapshot));
                        continue;
                    }
                    Err(issues) => ProblemKind::Unreadable(issues),
                },
                Ok(None) => ProblemKind::Missing,
                Err(error) => ProblemKind::Store(error),
            };
            problems.push(Problem {
                profile: profile.clone(),
                revision: revision.clone(),
                kind,
            });
        }
        if !problems.is_empty() {
            return Err(RestoreFailure { problems });
        }
        for snapshot in &held {
            self.adopt(snapshot);
        }
        Ok(Restored { held })
    }
}

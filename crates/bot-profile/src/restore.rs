//! 재배포 뒤 옛 판본 복원. 저장소에 보관한 검증한 묶음에서, 진행 중 작업이 고정한 판본을 다시 만들어 `Registry`에 등록한다.
//! 구조·프로필 ID·해시를 `from_bundle`로 다시 검증한다. 호스트 등록부는 보지 않는다 — 옛 판본의 재개이며 호출·승인·게시 직전의 현재 권한 검사를 대신하지 않는다.
//! 하나라도 복원하지 못하면 아무것도 등록하지 않고 프로필 ID·판본별 이유를 모두 돌려준다(호출한 쪽이 시작·교체를 멈춘다).

use crate::{
    build,
    issue::{Issue, Issues},
    limits::Limits,
    registry::{Registry, Report},
    scan,
    snapshot::from_bundle,
};
use bot_core::{
    id::{ProfileId, Revision},
    port::Error,
    profile::{ProfileBundles, ProfileCatalog, Snapshot},
};
use std::{collections::BTreeSet, fmt, path::Path, sync::Arc};

/// 프로필 폴더 하나를 읽어 검증한 묶음 `(프로필 ID, 판본, 바이트)`를 만든다 — 이전 버전이 보관하지 않은 옛 판본을 원본 폴더로 보충할 때 쓴다.
/// 호스트 등록부(서버·모델·입구 별칭)는 보지 않는다: 옛 판본은 지금의 호스트와 맞지 않을 수 있고, 옛 판본의 재개는 호출·승인·게시 직전의 현재 권한 검사가 지킨다.
/// 판본은 바이트의 해시이고 프로필 ID는 폴더 이름과 `profile.yaml`의 id가 같아야 하므로, 다른 판본·프로필의 폴더는 같은 키를 만들 수 없다.
pub fn read_bundle(dir: &Path, limits: &Limits) -> Result<(ProfileId, Revision, Vec<u8>), Report> {
    let mut issues = Issues::new(limits.max_issues);
    let name = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    if name.is_none() {
        issues.error(
            "dir-missing",
            &dir.display().to_string(),
            "프로필 폴더를 열 수 없어요",
        );
        return Err(Report::from_issues(issues));
    }
    let tree = scan::read_tree(dir, "", limits, &mut issues);
    let built = if issues.has_errors() {
        None
    } else {
        build::build(&tree, name.as_deref(), limits, &mut issues)
    };
    match built {
        Some(out) if !issues.has_errors() => {
            Ok((out.snapshot.id, out.snapshot.revision, out.bundle))
        }
        _ => Err(Report::from_issues(issues)),
    }
}

/// 복원하지 못한 이유.
#[derive(Debug)]
pub enum ProblemKind {
    /// 저장소에 그 판본의 묶음이 없다(구버전이 보관하지 않았다).
    Missing,
    /// 묶음이 구조·프로필 ID·해시 검증을 통과하지 못했다(손상).
    Unreadable(Vec<Issue>),
    /// 저장소가 묶음을 돌려주지 못했다.
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
        // 판본은 SHA-256 전체를 보인다: 어느 원본을 채워야 하는지 정확히 가리킨다
        write!(f, "프로필 {} 판본 {}: ", self.profile.0, self.revision.0)?;
        match &self.kind {
            ProblemKind::Missing => write!(f, "보관한 묶음이 없어요"),
            ProblemKind::Unreadable(issues) => {
                let first = issues.iter().find(|i| i.is_error());
                match first {
                    Some(issue) => write!(
                        f,
                        "보관한 묶음을 읽지 못했어요 [{}] {}",
                        issue.code, issue.message
                    ),
                    None => write!(f, "보관한 묶음을 읽지 못했어요"),
                }
            }
            ProblemKind::Store(Error::Unavailable) => write!(f, "저장소에 닿지 못했어요"),
            ProblemKind::Store(_) => write!(f, "저장소가 묶음을 돌려주지 않았어요"),
        }
    }
}

/// 복원하지 못한 판본 전체. 비어 있지 않다.
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

/// 복원한 옛 판본의 강한 참조. `Registry`는 약한 참조로만 등록하므로, 이것을 쥔 동안만 `lookup`에 보인다 —
/// 호출한 쪽이 역할이 도는 내내(프로세스가 끝날 때까지) 쥐고 있어야 한다.
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

    /// 복원한 (프로필, 판본)
    pub fn revisions(&self) -> impl Iterator<Item = (&ProfileId, &Revision)> {
        self.held.iter().map(|s| (&s.id, &s.revision))
    }
}

impl Registry {
    /// `wanted` 가운데 지금 조회되지 않는 판본(현재 집합도 이미 복원한 것도 아닌 옛 판본)을 `bundles`의 묶음에서 복원해 등록한다.
    /// 복원한 판본은 `lookup`으로만 보이고 `active`·`current`는 게시된 집합 그대로다.
    /// 실패하면 아무것도 등록하지 않고 모든 문제를 한 번에 돌려준다.
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

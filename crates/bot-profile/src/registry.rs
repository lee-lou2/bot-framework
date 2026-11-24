//! 활성 프로필 집합과 판본 교체. 읽기·검증이 모두 끝난 새 집합을 한 번에 바꿔 끼우고,
//! 하나라도 실패하면 이전 집합을 그대로 쓴다. 읽는 쪽은 옛 집합이나 새 집합 하나만 본다.
use crate::{
    activate, build,
    hash::sha256_hex,
    hint,
    host::HostCatalog,
    issue::{Issue, Issues, Severity},
    limits::Limits,
    model::entrance,
    scan,
};
use bot_core::{
    id::{ProfileId, Revision},
    profile::{ProfileCatalog, Snapshot},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak},
};

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub issues: Vec<Issue>,
    /// 항목이 너무 많아 일부만 담았는가
    pub truncated: bool,
    /// 보관하지 못한 것까지 센 오류 전체 개수(보고 한도와 무관)
    pub error_total: usize,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.error_total == 0
    }

    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.is_error())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Issue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
    }

    /// 오류 코드(나온 순서)
    pub fn codes(&self) -> Vec<&'static str> {
        self.errors().map(|i| i.code).collect()
    }

    pub fn has(&self, code: &str) -> bool {
        self.issues.iter().any(|i| i.code == code)
    }

    pub(crate) fn from_issues(issues: Issues) -> Self {
        let (truncated, error_total) = (issues.truncated(), issues.error_count());
        Self {
            issues: issues.into_vec(),
            truncated,
            error_total,
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for issue in &self.issues {
            writeln!(f, "{issue}")?;
        }
        if self.truncated {
            writeln!(f, "(항목이 많아 일부만 보여요)")?;
        }
        write!(
            f,
            "오류 {}건, 경고 {}건",
            self.error_total,
            self.warnings().count()
        )
    }
}

type Active = Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;

/// 한 번의 활성화로 만들어진 불변 집합
#[derive(Debug)]
pub struct ProfileSet {
    generation: u64,
    revision: Revision,
    active: Active,
    bundles: BTreeMap<ProfileId, Arc<[u8]>>,
    warnings: Vec<Issue>,
}

impl ProfileSet {
    fn new(
        profiles: BTreeMap<ProfileId, Arc<Snapshot>>,
        bundles: BTreeMap<ProfileId, Arc<[u8]>>,
        warnings: Vec<Issue>,
    ) -> Self {
        let mut text = String::from("botprofiles1\n");
        for (id, snap) in &profiles {
            text.push_str(&format!("{} {}\n", id.0, snap.revision.0));
        }
        Self {
            generation: 0,
            revision: Revision(sha256_hex(text.as_bytes())),
            active: Arc::new(profiles),
            bundles,
            warnings,
        }
    }

    pub fn empty() -> Self {
        Self::new(BTreeMap::new(), BTreeMap::new(), Vec::new())
    }

    /// 교체할 때마다 1씩 늘어난다(처음 `Registry`는 0).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 프로필 판본들을 묶은 집합 판본(`ProfileSource::validate`가 돌려주는 값).
    /// 실행에 기록하는 판본은 프로필 하나의 `Snapshot::revision`이다.
    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    /// 같은 활성화 시점의 전체 맵
    pub fn active(&self) -> &Active {
        &self.active
    }

    pub fn profile(&self, id: &ProfileId) -> Option<&Arc<Snapshot>> {
        self.active.get(id)
    }

    /// 프로필의 검증한 바이트 묶음. PG에 저장해 두면 `from_bundle`로 옛 판본을 다시 만든다.
    pub fn bundle(&self, id: &ProfileId) -> Option<&[u8]> {
        self.bundles.get(id).map(|b| &**b)
    }

    pub fn warnings(&self) -> &[Issue] {
        &self.warnings
    }

    /// `bot validate`가 그대로 출력할 글: 프로필 요약과 경고
    pub fn render(&self) -> String {
        let mut lines = vec![format!("검증 통과: 프로필 {}개", self.active.len())];
        lines.push(self.summary());
        lines.extend(self.warnings.iter().map(Issue::to_string));
        lines.join("\n")
    }

    /// 프로필마다 한 줄
    pub fn summary(&self) -> String {
        let lines: Vec<String> = self.active.values().map(|s| summary(s)).collect();
        lines.join("\n")
    }
}

fn summary(s: &Snapshot) -> String {
    // 상속으로 여러 에이전트에 보이는 도구는 한 번만 센다
    let tools: BTreeSet<(&str, &str)> = s
        .agents
        .values()
        .flat_map(|a| a.skills.values())
        .flat_map(|k| {
            k.tools
                .keys()
                .map(|t| (k.instructions.relative_path.as_str(), t.0.as_str()))
        })
        .collect();
    let triggers: usize = s.agents.values().map(|a| a.triggers.len()).sum();
    format!(
        "{} 판본 {} 에이전트 {} 도구 {} 트리거 {} 바인딩 {}",
        s.id.0,
        &s.revision.0[..12],
        s.agents.len(),
        tools.len(),
        triggers,
        s.bindings.len()
    )
}

/// 폴더를 읽어 검증한 집합을 만든다(교체하지 않는다). 실패하면 모든 오류를 담은 보고서를 돌려준다.
pub fn load(root: &Path, host: &HostCatalog, limits: &Limits) -> Result<ProfileSet, Report> {
    let mut all = Issues::new(limits.max_issues);
    let found = scan::list_profiles(root, limits, &mut all);
    let mut snapshots: BTreeMap<ProfileId, Arc<Snapshot>> = BTreeMap::new();
    let mut bundles: BTreeMap<ProfileId, Arc<[u8]>> = BTreeMap::new();
    for (name, dir) in &found {
        let mut local = Issues::new(limits.max_issues);
        let tree = scan::read_tree(dir, "", limits, &mut local);
        if !local.has_errors()
            && let Some(out) = build::build(&tree, Some(name), limits, &mut local)
        {
            activate::check_host(&out.snapshot, host, &mut local);
            if !local.has_errors() {
                let id = out.snapshot.id.clone();
                snapshots.insert(id.clone(), Arc::new(out.snapshot));
                bundles.insert(id, Arc::from(out.bundle));
            }
        }
        all.absorb(local, name);
    }
    if found.is_empty() && !all.has_errors() {
        all.error(
            "no-profiles",
            &root.display().to_string(),
            "프로필 폴더가 하나도 없어요",
        );
    }
    cross_profile(&snapshots, &mut all);
    if all.has_errors() {
        return Err(Report::from_issues(all));
    }
    Ok(ProfileSet::new(snapshots, bundles, all.into_vec()))
}

/// 활성 프로필 전체에서 같은 입구를 두 번 쓸 수 없다.
fn cross_profile(snapshots: &BTreeMap<ProfileId, Arc<Snapshot>>, issues: &mut Issues) {
    let mut seen: HashMap<String, (&str, &str)> = HashMap::new();
    for (id, snap) in snapshots {
        for (binding_id, binding) in &snap.bindings {
            let key = entrance(&binding.source);
            if let Some((other, other_binding)) = seen.insert(key.clone(), (&id.0, &binding_id.0)) {
                issues.error_at(
                    "duplicate-entrance",
                    &format!("{}/profile.yaml", id.0),
                    format!("바인딩 {}", binding_id.0),
                    format!(
                        "입구 `{key}`를 프로필 {other}의 바인딩 {other_binding}도 써요. {}",
                        hint::entrance_fix(&key, true)
                    ),
                );
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 현재 활성 집합과, 교체된 뒤에도 누군가 쥐고 있는 옛 판본. `ProfileCatalog`를 구현한다.
pub struct Registry {
    current: RwLock<Arc<ProfileSet>>,
    retired: Mutex<HashMap<(ProfileId, Revision), Weak<Snapshot>>>,
    reloads: Mutex<()>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            current: RwLock::new(Arc::new(ProfileSet::empty())),
            retired: Mutex::new(HashMap::new()),
            reloads: Mutex::new(()),
        }
    }

    /// 지금 활성인 집합(번들·경고·세대 포함)
    pub fn set(&self) -> Arc<ProfileSet> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 새 집합을 만들어 통째로 바꿔 끼운다. 실패하면 아무것도 바뀌지 않는다.
    /// 바이트가 그대로면(집합 판본이 같으면) 현재 집합을 돌려준다.
    pub fn reload(
        &self,
        root: &Path,
        host: &HostCatalog,
        limits: &Limits,
    ) -> Result<Arc<ProfileSet>, Report> {
        let _serial = lock(&self.reloads);
        let mut next = load(root, host, limits)?;
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        if next.revision == current.revision {
            return Ok(current.clone());
        }
        let mut retired = lock(&self.retired);
        retired.retain(|_, weak| weak.strong_count() > 0);
        for old in current.active.values() {
            if next.active.get(&old.id).map(|n| &n.revision) != Some(&old.revision) {
                retired.insert((old.id.clone(), old.revision.clone()), Arc::downgrade(old));
            }
        }
        next.generation = current.generation + 1;
        let next = Arc::new(next);
        *current = next.clone();
        Ok(next)
    }

    /// 저장해 둔 묶음에서 다시 만든 옛 판본을 등록한다(재시작 뒤 진행 중 실행의 재개).
    /// 등록은 약한 참조라, 이 `Arc`를 쥔 동안만 `lookup`에 보인다.
    pub fn adopt(&self, snapshot: &Arc<Snapshot>) {
        lock(&self.retired).insert(
            (snapshot.id.clone(), snapshot.revision.clone()),
            Arc::downgrade(snapshot),
        );
    }
}

impl ProfileCatalog for Registry {
    fn active(&self) -> Active {
        self.set().active.clone()
    }

    /// 현재 집합이거나, 교체된 뒤에도 아직 쓰이는 옛 판본.
    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>> {
        if let Some(found) = self
            .set()
            .active
            .get(profile)
            .filter(|s| s.revision == *revision)
        {
            return Some(found.clone());
        }
        lock(&self.retired)
            .get(&(profile.clone(), revision.clone()))
            .and_then(Weak::upgrade)
    }
}

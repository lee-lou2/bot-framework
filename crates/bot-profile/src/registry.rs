//! Active profile set, atomic reload/apply swap, and weak-referenced retired snapshot catalog.
use crate::{
    activate, build,
    hash::sha256_hex,
    hint,
    host::HostCatalog,
    issue::{Issue, Issues, Severity},
    limits::Limits,
    model::owner_key,
    scan,
};
use bot_core::{
    id::{ProfileId, Revision},
    profile::{Adapter, ProfileCatalog, Snapshot},
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
    pub truncated: bool,
    /// Total error count including errors dropped by the issue cap.
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
            writeln!(f, "(output truncated due to issue limit)")?;
        }
        write!(
            f,
            "{} error(s), {} warning(s)",
            self.error_total,
            self.warnings().count()
        )
    }
}

type Active = Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;

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

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Set-level revision across all active profiles (runs record per-profile `Snapshot::revision`).
    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    pub fn active(&self) -> &Active {
        &self.active
    }

    pub fn profile(&self, id: &ProfileId) -> Option<&Arc<Snapshot>> {
        self.active.get(id)
    }

    /// Validated canonical bundle bytes suitable for storing and restoring via `from_bundle`.
    pub fn bundle(&self, id: &ProfileId) -> Option<&[u8]> {
        self.bundles.get(id).map(|b| &**b)
    }

    pub fn warnings(&self) -> &[Issue] {
        &self.warnings
    }

    pub fn render(&self) -> String {
        let mut lines = vec![format!(
            "Validation passed: {} profile(s)",
            self.active.len()
        )];
        lines.push(self.summary());
        lines.extend(self.warnings.iter().map(Issue::to_string));
        lines.join("\n")
    }

    pub fn summary(&self) -> String {
        let lines: Vec<String> = self.active.values().map(|s| summary(s)).collect();
        lines.join("\n")
    }
}

fn summary(s: &Snapshot) -> String {
    // Count inherited tools once per (skill path, tool id).
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
        "{} rev {} agents {} tools {} triggers {} bindings {}",
        s.id.0,
        &s.revision.0[..12],
        s.agents.len(),
        tools.len(),
        triggers,
        s.bindings.len()
    )
}

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
            "no profile directories found",
        );
    }
    cross_profile(&snapshots, &mut all);
    if all.has_errors() {
        return Err(Report::from_issues(all));
    }
    Ok(ProfileSet::new(snapshots, bundles, all.into_vec()))
}

/// Enforces global entrance uniqueness; one profile owns an entire OpenAI endpoint alias.
fn cross_profile(snapshots: &BTreeMap<ProfileId, Arc<Snapshot>>, issues: &mut Issues) {
    let mut seen: HashMap<String, (&str, &str)> = HashMap::new();
    for (id, snap) in snapshots {
        for (binding_id, binding) in &snap.bindings {
            let key = owner_key(&binding.source);
            let openai = matches!(binding.source, Adapter::OpenAi { .. });
            let Some((other, other_binding)) = seen.insert(key.clone(), (&id.0, &binding_id.0))
            else {
                continue;
            };
            if openai && other == id.0 {
                continue;
            }
            let other_openai = snapshots
                .iter()
                .find(|(p, _)| p.0 == other)
                .and_then(|(_, s)| s.bindings.iter().find(|(b, _)| b.0 == other_binding))
                .is_some_and(|(_, b)| matches!(b.source, Adapter::OpenAi { .. }));
            let fix = if openai || other_openai {
                hint::OPENAI_OWNER.to_owned()
            } else {
                hint::entrance_fix(&key, true)
            };
            issues.error_at(
                "duplicate-entrance",
                &format!("{}/profile.yaml", id.0),
                format!("binding {}", binding_id.0),
                format!(
                    "entrance `{key}` is already used by profile {other} binding {other_binding}. {fix}"
                ),
            );
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

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

    pub fn set(&self) -> Arc<ProfileSet> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Loads and atomically swaps in a new set; leaves state unchanged on error or identical revision.
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

    /// Atomically swaps in a pre-validated set and returns strong refs to displaced snapshots
    /// so callers can keep them reachable via `lookup` while in-flight runs finish.
    pub fn apply(&self, mut next: ProfileSet) -> (Arc<ProfileSet>, Vec<Arc<Snapshot>>) {
        let _serial = lock(&self.reloads);
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        if next.revision == current.revision {
            return (current.clone(), Vec::new());
        }
        let mut retired = lock(&self.retired);
        retired.retain(|_, weak| weak.strong_count() > 0);
        let mut displaced = Vec::new();
        for old in current.active.values() {
            if next.active.get(&old.id).map(|n| &n.revision) != Some(&old.revision) {
                retired.insert((old.id.clone(), old.revision.clone()), Arc::downgrade(old));
                displaced.push(old.clone());
            }
        }
        next.generation = current.generation + 1;
        let next = Arc::new(next);
        *current = next.clone();
        (next, displaced)
    }

    /// Registers a restored historical snapshot by weak reference (visible to `lookup` while caller holds the `Arc`).
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

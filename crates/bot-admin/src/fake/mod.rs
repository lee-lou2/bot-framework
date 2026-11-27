//! In-memory fake store implementing `AdminRead`, `AdminAct`, and `Broker::decide` for tests and demos.
//! Mirrors real store visibility rules: invisible objects return `Missing` and audit entries chain under the same lock.

use bot_core::{admin::*, id::*, port::Error, state::*};
use sha2::{Digest, Sha256};
use std::{
    ops::{Deref, DerefMut},
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime},
};

#[derive(Clone, Debug)]
pub struct Decision {
    pub nonce: ApprovalId,
    pub actor: PrincipalId,
    pub allow: bool,
}

/// Profile-scoped call record matching the real store's `(profile, run, call)` key.
#[derive(Clone, Debug)]
pub struct OwnedCall {
    pub profile: ProfileId,
    pub view: CallView,
}

#[derive(Clone, Debug)]
pub struct OwnedEffect {
    pub profile: ProfileId,
    pub view: EffectView,
}

macro_rules! view_deref {
    ($owned:ident, $view:ident) => {
        impl Deref for $owned {
            type Target = $view;
            fn deref(&self) -> &$view {
                &self.view
            }
        }
        impl DerefMut for $owned {
            fn deref_mut(&mut self) -> &mut $view {
                &mut self.view
            }
        }
    };
}
view_deref!(OwnedCall, CallView);
view_deref!(OwnedEffect, EffectView);

/// Preapproval row paired with its last transition `(prev_epoch, paused, grant_id)` for `decide_pause`.
pub type FakePreapproval = (bot_core::grant::PreapprovalRow, Option<(i64, bool, String)>);

#[derive(Default)]
pub struct Data {
    pub runs: Vec<RunView>,
    pub calls: Vec<OwnedCall>,
    pub effects: Vec<OwnedEffect>,
    pub approvals: Vec<ApprovalView>,
    pub holds: Vec<HoldView>,
    pub audit: Vec<AuditEntry>,
    pub decisions: Vec<Decision>,
    pub resolutions: Vec<Resolution>,
    resolved: Vec<HoldKey>,
    /// Settled holds store only the verdict kind and receipt/evidence digest (never raw text) for idempotent repeat detection.
    settled: Vec<Settled>,
    next_hold: u64,
    pub preapprovals: Vec<FakePreapproval>,
    pub pause_audits: u64,
}

/// Open-hold key including `HoldKind` so legacy tool and reply holds sharing a `call_id` stay distinct.
pub type HoldKey = (ProfileId, RunId, CallId, HoldKind);

pub fn hold_key(h: &HoldView) -> HoldKey {
    (h.profile.clone(), h.run.clone(), h.call.clone(), h.kind)
}

#[derive(Clone, Debug)]
pub struct Resolution {
    pub actor: PrincipalId,
    pub hold: HoldRef,
    pub verdict: &'static str,
    pub digest: [u8; 32],
}

struct Settled {
    hold_id: HoldId,
    profile: ProfileId,
    run: RunId,
    call: CallId,
    effect: Option<EffectId>,
    actor: PrincipalId,
    verdict: (&'static str, [u8; 32]),
}

pub(crate) fn verdict_print(v: &HoldVerdict) -> (&'static str, [u8; 32]) {
    match v {
        HoldVerdict::Confirmed { receipt } => ("confirmed", digest(receipt)),
        HoldVerdict::NotExecuted { evidence } => ("not_executed", digest(evidence)),
    }
}

#[derive(Default)]
pub struct Fake {
    data: Mutex<Data>,
    /// Concurrency test hook: holds the next `n` approval reads at a barrier before returning.
    gate: Mutex<Option<(std::sync::Arc<tokio::sync::Barrier>, usize)>>,
    /// Defense-in-depth test hook: when true, returns owner-only card bodies to all viewers.
    leaky: std::sync::atomic::AtomicBool,
}

fn id<T>(make: fn(String) -> T, s: &str) -> T {
    make(s.to_owned())
}

pub fn pid(s: &str) -> PrincipalId {
    id(PrincipalId, s)
}

pub fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

pub fn run_view(
    profile: &str,
    run: &str,
    agent: &str,
    principal: &str,
    state: RunState,
) -> RunView {
    let now = SystemTime::now();
    RunView {
        profile: id(ProfileId, profile),
        run: id(RunId, run),
        root: id(RunId, run),
        parent: None,
        step: None,
        agent: id(AgentId, agent),
        principal: pid(principal),
        kind: ConversationKind::Cli,
        trigger: None,
        state,
        generation: Generation(1),
        attempts: 1,
        waiting: None,
        private_memory_read: false,
        error: None,
        created_at: now - Duration::from_secs(300),
        updated_at: now - Duration::from_secs(60),
    }
}

pub fn call_view(run: &RunView, call: &str, tool: &str, risk: Risk, state: CallState) -> OwnedCall {
    let view = CallView {
        call: id(CallId, call),
        run: run.run.clone(),
        generation: run.generation,
        tool: id(ToolId, tool),
        tool_revision: id(Revision, "t1"),
        risk,
        state,
        error: match state {
            CallState::Denied => Some(CallError::Denied),
            CallState::Failed => Some(CallError::ExecutionFailed),
            CallState::Held => Some(CallError::OutcomeUnknown),
            _ => None,
        },
        args_sha256: digest(&format!("{call}:{tool}")),
        started_at: run.created_at + Duration::from_secs(5),
        finished_at: matches!(
            state,
            CallState::Done | CallState::Failed | CallState::Denied
        )
        .then(|| run.created_at + Duration::from_secs(9)),
    };
    OwnedCall {
        profile: run.profile.clone(),
        view,
    }
}

pub fn effect_view(
    call: &OwnedCall,
    effect: &str,
    step: &str,
    state: EffectState,
    receipt: Option<&str>,
) -> OwnedEffect {
    let view = EffectView {
        effect: id(EffectId, effect),
        run: call.run.clone(),
        call: call.call.clone(),
        step: step.to_owned(),
        state,
        resolution: None,
        hold_id: (state == EffectState::Held).then(|| HoldId(format!("h-{effect}"))),
        receipt: receipt.map(str::to_owned),
        reason: (state == EffectState::Held).then(|| "outcome_unknown".to_owned()),
        args_sha256: digest(&format!("{effect}:{step}")),
        generation: call.generation,
        at: call.started_at + Duration::from_secs(2),
    };
    OwnedEffect {
        profile: call.profile.clone(),
        view,
    }
}

pub fn approval_view(call: &CallView, run: &RunView, nonce: &str, approver: &str) -> ApprovalView {
    ApprovalView {
        nonce: id(ApprovalId, nonce),
        profile: run.profile.clone(),
        run: run.run.clone(),
        generation: run.generation,
        call: call.call.clone(),
        requester: run.principal.clone(),
        approver: pid(approver),
        risk: call.risk,
        tool: call.tool.clone(),
        tool_revision: call.tool_revision.clone(),
        policy_revision: id(Revision, "p1"),
        args_sha256: call.args_sha256,
        expires_at: SystemTime::now() + Duration::from_secs(900),
        state: ApprovalState::Pending,
        render: vec![
            ("Target".to_owned(), "synthetic-event".to_owned()),
            ("Action".to_owned(), "delete".to_owned()),
        ],
        decided_by: None,
        decided_at: None,
    }
}

fn entry_hash(
    prev: &[u8; 32],
    seq: u64,
    actor: &str,
    action: &str,
    target: &str,
    verdict: &str,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(seq.to_be_bytes());
    for part in [actor, action, target, verdict] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    h.finalize().into()
}

impl Data {
    pub fn issue_hold(&mut self) -> HoldId {
        self.next_hold += 1;
        HoldId(format!("h-new-{}", self.next_hold))
    }

    /// Re-holds the first hold matching `call` with a fresh `HoldId`; use `rehold_hold` when multiple holds share a call ID.
    pub fn rehold(&mut self, call: &str, effect: Option<&str>) -> HoldId {
        match self.holds.iter().find(|h| h.call.0 == call).map(hold_key) {
            Some(key) => self.rehold_hold(&key, effect),
            None => self.issue_hold(),
        }
    }

    pub fn rehold_hold(&mut self, key: &HoldKey, effect: Option<&str>) -> HoldId {
        let new = self.issue_hold();
        let (profile, run, call, _) = key;
        self.resolved.retain(|k| k != key);
        if let Some(h) = self.holds.iter_mut().find(|h| &hold_key(h) == key) {
            h.hold_id = effect.is_none().then(|| new.clone());
        }
        if let Some(id) = effect
            && let Some(e) = self.effects.iter_mut().find(|e| {
                &e.profile == profile && &e.run == run && &e.call == call && e.effect.0 == id
            })
        {
            (e.state, e.resolution, e.receipt) = (EffectState::Held, None, None);
            (e.reason, e.hold_id) = (Some("outcome_unknown".to_owned()), Some(new.clone()));
        }
        if let Some(r) = self
            .runs
            .iter_mut()
            .find(|r| &r.profile == profile && &r.run == run)
        {
            (r.state, r.waiting) = (RunState::Waiting, Some(Waiting::Hold(call.clone())));
        }
        new
    }

    /// Simulates a worker restarting a previously resolved effect step, clearing its `NotExecuted` marker.
    pub fn restart_effect(&mut self, profile: &str, effect: &str) {
        if let Some(e) = self
            .effects
            .iter_mut()
            .find(|e| e.profile.0 == profile && e.effect.0 == effect)
        {
            (e.state, e.resolution, e.reason) = (EffectState::Started, None, None);
        }
    }

    pub fn steps_of(&self, h: &HoldView) -> Vec<EffectView> {
        if h.kind != HoldKind::Tool {
            return Vec::new();
        }
        self.effects
            .iter()
            .filter(|e| e.profile == h.profile && e.run == h.run && e.call == h.call)
            .map(|e| e.view.clone())
            .collect()
    }

    pub fn hold_open(&self, h: &HoldView) -> bool {
        let steps = self.steps_of(h);
        if steps.is_empty() || h.hold_id.is_some() {
            !self.resolved.contains(&hold_key(h))
        } else {
            steps.iter().any(|e| e.state == EffectState::Held)
        }
    }

    pub fn log(
        &mut self,
        profile: Option<&ProfileId>,
        actor: &PrincipalId,
        action: &str,
        target: &str,
        args: Option<[u8; 32]>,
        verdict: &str,
    ) {
        let (seq, prev) = self
            .audit
            .last()
            .map_or((1, [0; 32]), |e| (e.seq + 1, e.hash));
        let hash = entry_hash(&prev, seq, &actor.0, action, target, verdict);
        self.audit.push(AuditEntry {
            seq,
            at: SystemTime::now(),
            prev,
            hash,
            profile: profile.cloned(),
            actor: actor.clone(),
            action: action.to_owned(),
            target: target.to_owned(),
            args_sha256: args,
            verdict: verdict.to_owned(),
        });
    }

    fn sees_run(&self, v: &Viewer, r: &RunView) -> bool {
        v.operator || r.principal == v.principal
    }

    fn sees_approval(v: &Viewer, a: &ApprovalView) -> bool {
        v.operator || a.requester == v.principal || a.approver == v.principal
    }
}

fn operator(v: &Viewer) -> Result<(), Error> {
    if v.operator {
        Ok(())
    } else {
        Err(Error::Denied)
    }
}

/// Keyset pagination over zero-padded lexicographic sort keys so mid-pagination updates never skip or duplicate rows.
fn paged<T: Clone>(mut rows: Vec<(String, T)>, page: &Page) -> Listing<T> {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let limit = page.limit.clamp(1, 200) as usize;
    let rest: Vec<&(String, T)> = rows
        .iter()
        .filter(|(key, _)| page.after.as_deref().is_none_or(|c| key.as_str() > c))
        .collect();
    let items = rest.iter().take(limit).map(|(_, v)| v.clone()).collect();
    let next = (rest.len() > limit).then(|| rest[limit - 1].0.clone());
    Listing { items, next }
}

pub(crate) fn ms_key(t: SystemTime) -> u64 {
    u64::try_from(crate::json::ms(t).max(0)).unwrap_or(0)
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn gate_approvals(&self, n: usize) {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(n));
        *self.gate.lock().unwrap_or_else(|e| e.into_inner()) = Some((barrier, n));
    }

    fn take_gate(&self) -> Option<std::sync::Arc<tokio::sync::Barrier>> {
        let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some((b, n)) if *n > 0 => {
                *n -= 1;
                Some(b.clone())
            }
            _ => None,
        }
    }

    pub fn leak_bodies(&self, on: bool) {
        self.leaky.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn scoped(&self, viewer: &Viewer, mut a: ApprovalView) -> ApprovalView {
        let leaky = self.leaky.load(std::sync::atomic::Ordering::Relaxed);
        if !leaky && !crate::json::body_visible(&a, viewer) {
            a.render.clear();
        }
        a
    }

    pub fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn edit<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        f(&mut self.lock())
    }

    pub fn decisions(&self) -> Vec<Decision> {
        self.lock().decisions.clone()
    }

    pub fn resolutions(&self) -> Vec<Resolution> {
        self.lock().resolutions.clone()
    }

    pub fn tamper_audit(&self, seq: u64) {
        if let Some(e) = self.lock().audit.iter_mut().find(|e| e.seq == seq) {
            e.hash[0] ^= 0xff;
        }
    }

    pub fn demo() -> Self {
        let fake = Self::new();
        fake.edit(|d| {
            let mut root = run_view("personal", "r-1", "main", "local:owner", RunState::Waiting);
            root.waiting = Some(Waiting::Approval);
            let del = call_view(
                &root,
                "c-1",
                "agenda.delete",
                Risk::Irreversible,
                CallState::Awaiting,
            );
            let mut child = run_view(
                "personal",
                "r-2",
                "main/research",
                "local:owner",
                RunState::Succeeded,
            );
            (child.root, child.parent, child.step) = (
                root.run.clone(),
                Some(root.run.clone()),
                Some("lookup".to_owned()),
            );
            let list = call_view(&child, "c-0", "agenda.list", Risk::Read, CallState::Done);
            let mut morning = run_view(
                "personal",
                "r-3",
                "main",
                "service:morning",
                RunState::Succeeded,
            );
            (morning.kind, morning.trigger) =
                (ConversationKind::Scheduled, Some("morning".to_owned()));
            let mut held = run_view("personal", "r-4", "main", "local:owner", RunState::Waiting);
            held.waiting = Some(Waiting::Hold(id(CallId, "c-9")));
            held.generation = Generation(2);
            held.attempts = 2;
            let publish = call_view(&held, "c-9", "report.publish", Risk::Write, CallState::Held);
            let upload = effect_view(
                &publish,
                "e-1",
                "upload",
                EffectState::Confirmed,
                Some("file-17"),
            );
            let notify = effect_view(&publish, "e-2", "notify", EffectState::Held, None);
            let mut dead = run_view("personal", "r-5", "main", "local:owner", RunState::Dead);
            dead.error = Some("tool_failed".to_owned());
            dead.attempts = 4;
            let other = run_view("work", "r-w1", "main", "local:owner", RunState::Succeeded);
            d.approvals
                .push(approval_view(&del, &root, "ap-1", "local:owner"));
            d.holds.push(HoldView {
                // Multi-step holds carry no call-level `hold_id`; each resolvable step carries its own.
                hold_id: None,
                kind: HoldKind::Tool,
                profile: held.profile.clone(),
                run: held.run.clone(),
                call: publish.call.clone(),
                tool: publish.tool.clone(),
                reason: "outcome_unknown".to_owned(),
                since: SystemTime::now() - Duration::from_secs(1200),
                steps: vec![upload.view.clone(), notify.view.clone()],
            });
            d.calls.extend([del, list, publish]);
            d.effects.extend([upload, notify]);
            d.runs.extend([root, child, morning, held, dead, other]);
            use bot_core::grant::PreapprovalRow;
            let row = |agent: &str, trigger: &str, grant: &str, epoch, paused, used, max| {
                (
                    PreapprovalRow {
                        profile: "personal".to_owned(),
                        agent: agent.to_owned(),
                        trigger: trigger.to_owned(),
                        grant_id: grant.to_owned(),
                        epoch,
                        paused,
                        used_today: used,
                        max_per_day: max,
                    },
                    None,
                )
            };
            d.preapprovals.extend([
                row("main", "notify", "g-demo-notify", 0, false, 3, 24),
                row("main/developer", "digest", "g-demo-digest", 1, true, 0, 8),
            ]);
            let (owner, personal) = (pid("local:owner"), id(ProfileId, "personal"));
            d.log(
                Some(&personal),
                &owner,
                "profile.activate",
                "rev-1",
                None,
                "ok",
            );
            d.log(
                Some(&personal),
                &pid("service:morning"),
                "tool.call",
                "agenda.list",
                Some(digest("c-0:agenda.list")),
                "allow",
            );
        });
        fake
    }
}

mod catalog;
mod ports;

pub use catalog::{StaticCatalog, demo_catalog, personal, work};

//! 가짜 저장소: `AdminRead`·`AdminAct`·`Broker::decide`를 메모리로 구현한다(시험·데모·다른 크레이트의 시험용).
//! 실제 저장소와 같은 범위 규칙을 흉내 낸다: 운영자는 전체, 그 밖의 주체는 자기 요청과 자기 승인만 보고,
//! 운영자 전용 조회는 거절하며, 보이지 않는 대상은 존재 여부를 숨기려고 `Missing`으로 답한다.
//! 감사는 판정과 같은 락 안에서 해시 체인에 잇는다.

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

/// 가짜 저장소가 가진 호출. 실제 저장소처럼 프로필이 키의 일부다: 같은 run·호출 ID를 다른 프로필이 써도 섞이지 않는다.
/// `Deref`로 `CallView`처럼 읽는다(`call_view`가 run의 프로필을 따라 만든다).
#[derive(Clone, Debug)]
pub struct OwnedCall {
    pub profile: ProfileId,
    pub view: CallView,
}

/// 가짜 저장소가 가진 효과 단계(프로필이 키의 일부). `effect_view`가 호출의 프로필을 따라 만든다.
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

/// 사전 승인 한 행과 마지막 전환 기록(직전 세대·정지 여부·승인 ID).
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
    /// 끝난 판정의 기록: 보류 식별자·대상·운영자·판정 종류와 영수증/증거의 **지문**(원문은 보존하지 않는다).
    /// 같은 식별자로 같은 운영자·같은 판정이 다시 오면 새로 적용하지 않고 `Repeated`다.
    settled: Vec<Settled>,
    next_hold: u64,
    /// 사전 승인 행과 정지 판정에 필요한 마지막 전환 기록(`bot_core::grant::decide_pause`).
    pub preapprovals: Vec<FakePreapproval>,
    /// 정지·재개로 쓴 감사 줄 수(중복 재전달은 늘지 않는다).
    pub pause_audits: u64,
}

/// 열린 보류를 가리키는 키. 구판 데이터는 같은 프로필·run·호출 ID 아래 도구와 회신을 따로 보류할 수 있어 종류까지 넣는다.
pub type HoldKey = (ProfileId, RunId, CallId, HoldKind);

pub fn hold_key(h: &HoldView) -> HoldKey {
    (h.profile.clone(), h.run.clone(), h.call.clone(), h.kind)
}

/// 적용된 판정의 기록: 영수증·증거의 원문은 보존하지 않고 종류와 지문만 남긴다(실제 저장소와 같다).
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

/// 판정 종류와 입력 글의 지문. 같은 종류·같은 글이어야 «같은 판정»이다.
pub(crate) fn verdict_print(v: &HoldVerdict) -> (&'static str, [u8; 32]) {
    match v {
        HoldVerdict::Confirmed { receipt } => ("confirmed", digest(receipt)),
        HoldVerdict::NotExecuted { evidence } => ("not_executed", digest(evidence)),
    }
}

#[derive(Default)]
pub struct Fake {
    data: Mutex<Data>,
    /// 경합 시험용: 다음 `n`번의 승인 조회는 모두 모일 때까지 기다린 뒤 돌려준다(읽은 값은 기다리기 전의 것).
    gate: Mutex<Option<(std::sync::Arc<tokio::sync::Barrier>, usize)>>,
    /// 시험용: 참이면 소유자 전용 카드 본문도 모든 조회자에게 돌려준다(관리 API가 스스로 막는지 보려는 «새는 저장소»).
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
        // 판정할 수 있는(보류) 단계만 보류 식별자를 가진다. 시험이 정하기 쉽게 효과 ID에서 만든다.
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
            ("대상".to_owned(), "synthetic-event".to_owned()),
            ("변경".to_owned(), "삭제".to_owned()),
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
    /// 새 보류 식별자를 발급한다(저장소가 보류에 들어갈 때마다 새로 발급한다).
    pub fn issue_hold(&mut self) -> HoldId {
        self.next_hold += 1;
        HoldId(format!("h-new-{}", self.next_hold))
    }

    /// 같은 대상이 다시 보류된 것을 흉내 낸다: 새 식별자를 받아 열린 보류가 되고 run은 다시 기다린다.
    /// `effect`를 지정하면 그 단계가, 생략하면 호출 전체가 새 보류가 된다. 새 식별자를 돌려준다.
    /// 호출 ID가 하나뿐인 시험을 위한 줄임이다: 그 호출 ID를 가진 첫 보류를 다시 보류시킨다(같은 ID가 여럿이면 `rehold_hold`).
    pub fn rehold(&mut self, call: &str, effect: Option<&str>) -> HoldId {
        match self.holds.iter().find(|h| h.call.0 == call).map(hold_key) {
            Some(key) => self.rehold_hold(&key, effect),
            None => self.issue_hold(),
        }
    }

    /// `rehold`의 정확한 형태: 프로필·run·호출·종류로 보류를 가리킨다.
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

    /// 일꾼이 판정된 단계를 다시 시작한 것을 흉내 낸다: 새로 시작한 단계가 되어 «미실행 판정 뒤 재개 대기» 표지가 사라진다.
    /// 효과 ID는 프로필 안에서만 유일하다(실제 저장소의 키는 프로필+ID).
    pub fn restart_effect(&mut self, profile: &str, effect: &str) {
        if let Some(e) = self
            .effects
            .iter_mut()
            .find(|e| e.profile.0 == profile && e.effect.0 == effect)
        {
            (e.state, e.resolution, e.reason) = (EffectState::Started, None, None);
        }
    }

    /// 이 보류의 현재 단계들(효과 원장에서 읽는다). 도구 보류만 단계를 가지고(회신·민감 DM은 호출 단위 보류다),
    /// 같은 run·호출 ID를 쓰는 다른 프로필·다른 run의 단계는 섞이지 않는다(키는 프로필+run+호출).
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

    /// 보류가 열려 있는가. 판정 전의 호출 단위 발생이나 미결 단계가 있으면 열려 있다.
    /// 같은 호출 ID의 다른 보류(다른 run, 같은 호출의 회신)는 이 보류의 상태에 영향을 주지 않는다.
    pub fn hold_open(&self, h: &HoldView) -> bool {
        let steps = self.steps_of(h);
        if steps.is_empty() || h.hold_id.is_some() {
            !self.resolved.contains(&hold_key(h))
        } else {
            steps.iter().any(|e| e.state == EffectState::Held)
        }
    }

    /// 감사 한 줄을 체인 끝에 잇는다.
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

/// 쪽 나누기: 커서는 마지막으로 돌려준 행의 **정렬 키**다(행 번호가 아니다). 키는 영을 채운 숫자와 식별자를 `:`로 이어
/// 사전순이 곧 정렬 순서이고, 다음 쪽은 그 키보다 뒤의 행이다. 쪽 사이에 행이 바뀌거나 처리돼도 빠지거나 겹치지 않는다
/// (바뀐 행이 커서보다 앞으로 옮겨 가면 이미 지나간 쪽의 일이다). 실제 저장소도 같은 방식을 따른다.
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

/// 시각을 영 채운 20자리로(사전순 = 숫자순). 내림차순 정렬이 필요하면 `u64::MAX - ms`를 넣는다.
pub(crate) fn ms_key(t: SystemTime) -> u64 {
    u64::try_from(crate::json::ms(t).max(0)).unwrap_or(0)
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    /// 다음 `n`번의 `approval` 조회가 서로를 기다리게 한다. 두 요청이 모두 같은 상태를 읽은 뒤에야 판정으로 넘어간다.
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

    /// 소유자 전용 카드 본문을 거르지 않는 «새는 저장소»로 만든다(방어 시험용).
    pub fn leak_bodies(&self, on: bool) {
        self.leaky.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// 조회자에게 보이는 모양: 소유자 전용 카드의 본문은 소유자가 아니면 비운다. 실제 저장소도 같은 규칙을 따른다.
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

    /// 감사 한 줄의 해시를 바꿔 변조를 흉내 낸다.
    pub fn tamper_audit(&self, seq: u64) {
        if let Some(e) = self.lock().audit.iter_mut().find(|e| e.seq == seq) {
            e.hash[0] ^= 0xff;
        }
    }

    /// 데모·시험용 합성 데이터: 프로필 personal(승인 대기·위임 자식·예약·보류·dead)과 프로필 work.
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
                // 여러 단계의 묶음은 식별자가 없다: 판정할 수 있는 각 단계가 자기 식별자를 가진다.
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
            // 동작 중 3/24와 정지됨 0/8(중첩 에이전트 ID `main/developer`는 경로에서 한 조각으로 인코딩해 온다)
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

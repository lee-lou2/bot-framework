//! 가짜 저장소의 포트 구현: 조회 범위 규칙, 보류 판정, 브로커 승인.

use super::{
    Data, Decision, Fake, Resolution, Settled, entry_hash, hold_key, ms_key, operator, paged,
    verdict_print,
};
use crate::port::*;
use bot_core::{
    command::{CallResult, ToolCall},
    id::*,
    port::{Broker, Error, Reply},
    state::*,
};
use std::time::SystemTime;

impl AdminRead for Fake {
    fn runs(&self, viewer: &Viewer, query: RunQuery) -> Reply<'_, Listing<RunView>> {
        let d = self.lock();
        // 최근 갱신순(내림차순), 같으면 프로필·run 순.
        let rows: Vec<(String, RunView)> = d
            .runs
            .iter()
            .filter(|r| d.sees_run(viewer, r))
            .filter(|r| query.profile.as_ref().is_none_or(|p| &r.profile == p))
            .filter(|r| query.state.is_none_or(|s| r.state == s))
            .filter(|r| query.root.as_ref().is_none_or(|x| &r.root == x))
            .map(|r| {
                let key = format!(
                    "{:020}:{}:{}",
                    u64::MAX - ms_key(r.updated_at),
                    r.profile.0,
                    r.run.0
                );
                (key, r.clone())
            })
            .collect();
        let out = paged(rows, &query.page);
        Box::pin(async move { Ok(out) })
    }

    fn detail(&self, viewer: &Viewer, profile: ProfileId, run: RunId) -> Reply<'_, RunDetail> {
        let d = self.lock();
        let out = d
            .runs
            .iter()
            .find(|r| r.profile == profile && r.run == run && d.sees_run(viewer, r))
            .map(|r| {
                let mut lineage = vec![r.clone()];
                while let Some(parent) = lineage.last().and_then(|x| x.parent.clone()) {
                    match d
                        .runs
                        .iter()
                        .find(|p| p.profile == profile && p.run == parent)
                    {
                        Some(p) => lineage.push(p.clone()),
                        None => break,
                    }
                }
                lineage.reverse();
                RunDetail {
                    run: r.clone(),
                    lineage,
                    children: d
                        .runs
                        .iter()
                        .filter(|c| c.profile == profile && c.parent.as_ref() == Some(&r.run))
                        .cloned()
                        .collect(),
                    calls: d
                        .calls
                        .iter()
                        .filter(|c| c.profile == r.profile && c.run == r.run)
                        .map(|c| c.view.clone())
                        .collect(),
                    effects: d
                        .effects
                        .iter()
                        .filter(|e| e.profile == r.profile && e.run == r.run)
                        .map(|e| e.view.clone())
                        .collect(),
                }
            })
            .ok_or(Error::Missing);
        Box::pin(async move { out })
    }

    /// 정렬은 설계 계약을 따른다. 대기: 만료 임박순·nonce 오름차순. 처리됨: `decided_at` 내림차순(없으면 뒤)·nonce 오름차순.
    /// 전체: 처리된 행을 처리됨 순서로, 대기 행을 맨 뒤에. 필터는 쪽 나누기 전에 적용한다.
    fn approvals(&self, viewer: &Viewer, query: ApprovalQuery) -> Reply<'_, Listing<ApprovalView>> {
        let d = self.lock();
        let pending = |a: &ApprovalView| a.state == ApprovalState::Pending;
        // 정렬 키: 처리 시각이 있는 처리된 행(최근순) → 처리 시각 없는 처리된 행 → 대기 행(만료 임박순). 같으면 nonce 순.
        let key = |a: &ApprovalView| match (pending(a), a.decided_at) {
            (false, Some(at)) => format!("0:{:020}:{}", u64::MAX - ms_key(at), a.nonce.0),
            (false, None) => format!("1:{:020}:{}", 0, a.nonce.0),
            (true, _) => format!("2:{:020}:{}", ms_key(a.expires_at), a.nonce.0),
        };
        let rows: Vec<(String, ApprovalView)> = d
            .approvals
            .iter()
            .filter(|a| Data::sees_approval(viewer, a))
            .filter(|a| query.profile.as_ref().is_none_or(|p| &a.profile == p))
            .filter(|a| match query.filter {
                ApprovalFilter::Pending => pending(a),
                ApprovalFilter::Decided => !pending(a),
                ApprovalFilter::All => true,
            })
            .map(|a| (key(a), self.scoped(viewer, a.clone())))
            .collect();
        let out = paged(rows, &query.page);
        Box::pin(async move { Ok(out) })
    }

    fn approval(&self, viewer: &Viewer, nonce: ApprovalId) -> Reply<'_, ApprovalView> {
        let gate = self.take_gate();
        let d = self.lock();
        let out = d
            .approvals
            .iter()
            .find(|a| a.nonce == nonce && Data::sees_approval(viewer, a))
            .map(|a| self.scoped(viewer, a.clone()))
            .ok_or(Error::Missing);
        drop(d);
        Box::pin(async move {
            if let Some(g) = gate {
                g.wait().await;
            }
            out
        })
    }

    fn holds(&self, viewer: &Viewer, profile: Option<ProfileId>) -> Reply<'_, Vec<HoldView>> {
        let d = self.lock();
        let out = operator(viewer).map(|()| {
            d.holds
                .iter()
                .filter(|h| profile.as_ref().is_none_or(|p| &h.profile == p))
                .filter(|h| d.hold_open(h))
                .map(|h| HoldView {
                    steps: d.steps_of(h),
                    ..h.clone()
                })
                .collect()
        });
        Box::pin(async move { out })
    }

    fn audit(&self, viewer: &Viewer, query: AuditQuery) -> Reply<'_, Vec<AuditEntry>> {
        let d = self.lock();
        let out = operator(viewer).map(|()| {
            d.audit
                .iter()
                .rev()
                .filter(|e| query.before.is_none_or(|b| e.seq < b))
                .filter(|e| {
                    query
                        .profile
                        .as_ref()
                        .is_none_or(|p| e.profile.as_ref() == Some(p))
                })
                .take(query.limit.clamp(1, 201) as usize)
                .cloned()
                .collect()
        });
        Box::pin(async move { out })
    }

    fn verify_audit(&self, viewer: &Viewer) -> Reply<'_, AuditCheck> {
        let d = self.lock();
        let out = operator(viewer).map(|()| {
            let (mut prev, mut broken_at) = ([0u8; 32], None);
            for e in &d.audit {
                let want = entry_hash(&e.prev, e.seq, &e.actor.0, &e.action, &e.target, &e.verdict);
                if broken_at.is_none() && (e.prev != prev || e.hash != want) {
                    broken_at = Some(e.seq);
                }
                prev = e.hash;
            }
            AuditCheck {
                entries: d.audit.len() as u64,
                head: prev,
                broken_at,
            }
        });
        Box::pin(async move { out })
    }

    fn status(&self, viewer: &Viewer) -> Reply<'_, Vec<ProfileStatus>> {
        let d = self.lock();
        let out = operator(viewer).map(|()| {
            let mut names: Vec<ProfileId> = d.runs.iter().map(|r| r.profile.clone()).collect();
            names.sort();
            names.dedup();
            names
                .into_iter()
                .map(|p| {
                    let mut runs: Vec<(RunState, u64)> = Vec::new();
                    for r in d.runs.iter().filter(|r| r.profile == p) {
                        match runs.iter_mut().find(|(s, _)| *s == r.state) {
                            Some((_, n)) => *n += 1,
                            None => runs.push((r.state, 1)),
                        }
                    }
                    ProfileStatus {
                        approvals_pending: d
                            .approvals
                            .iter()
                            .filter(|a| a.profile == p && a.state == ApprovalState::Pending)
                            .count() as u64,
                        holds: d
                            .holds
                            .iter()
                            .filter(|h| h.profile == p && d.hold_open(h))
                            .count() as u64,
                        oldest_ready_secs: None,
                        runs,
                        profile: p,
                    }
                })
                .collect()
        });
        Box::pin(async move { out })
    }

    /// 운영자 전용. 실제 저장소처럼 승인마다 정지 세대·정지 여부·오늘 사용량을 한 행으로 준다.
    fn preapprovals(&self, viewer: &Viewer) -> Reply<'_, Vec<bot_core::grant::PreapprovalRow>> {
        let out = if viewer.operator {
            Ok(self
                .lock()
                .preapprovals
                .iter()
                .map(|(row, _)| row.clone())
                .collect())
        } else {
            Err(Error::Denied)
        };
        Box::pin(async move { out })
    }
}

impl AdminAct for Fake {
    /// 정지·재개: 승인 ID·정지 세대를 현재와 대조하는 규칙은 `bot_core::grant::decide_pause`로 저장소와 같다.
    fn set_pause(
        &self,
        actor: PrincipalId,
        key: bot_core::grant::TriggerKey,
        cas: bot_core::grant::GrantCas,
        pause: bool,
    ) -> Reply<'_, bot_core::grant::PauseOutcome> {
        use bot_core::grant::{PauseDecision, PauseOutcome, PauseState, decide_pause};
        let mut d = self.lock();
        let found = d.preapprovals.iter().position(|(row, _)| {
            (
                row.profile.as_str(),
                row.agent.as_str(),
                row.trigger.as_str(),
            ) == (
                key.profile.0.as_str(),
                key.agent.0.as_str(),
                key.trigger.as_str(),
            )
        });
        let out = match found {
            None => Err(Error::Missing),
            Some(i) => {
                let (row, last) = d.preapprovals[i].clone();
                let state = PauseState {
                    epoch: row.epoch,
                    paused: row.paused,
                    last,
                };
                match decide_pause(&row.grant_id, &state, &cas, pause) {
                    PauseDecision::Stale => Err(Error::Stale),
                    PauseDecision::Repeated => Ok(PauseOutcome {
                        row,
                        repeated: true,
                    }),
                    PauseDecision::Apply => {
                        let entry = &mut d.preapprovals[i];
                        entry.1 = Some((entry.0.epoch, pause, entry.0.grant_id.clone()));
                        entry.0.epoch += 1;
                        entry.0.paused = pause;
                        let row = entry.0.clone();
                        d.pause_audits += 1;
                        d.log(
                            Some(&ProfileId(row.profile.clone())),
                            &actor,
                            if pause {
                                "preapproval.pause"
                            } else {
                                "preapproval.resume"
                            },
                            &format!("{}/{}", row.agent, row.trigger),
                            None,
                            &format!("{}:epoch={}", row.grant_id, row.epoch),
                        );
                        Ok(PauseOutcome {
                            row,
                            repeated: false,
                        })
                    }
                }
            }
        };
        Box::pin(async move { out })
    }

    /// 판정은 보류 식별자와 대상(프로필·run·호출·단계)을 함께 대조한 뒤 한 번만 적용한다.
    /// - 끝난 같은 식별자: 같은 운영자·같은 판정(종류와 영수증/증거)·같은 대상이면 `Repeated`(새 보류가 생겼어도), 다르면 `Conflict`.
    /// - 열린 보류에 없는 식별자는 `Missing`, 대상이 식별자의 것과 다르면 `Conflict`.
    /// - 대상은 식별자로 찾는다: 같은 호출 ID 아래 도구·회신 보류가 함께 있거나 다른 run이 같은 호출 ID를 써도 서로 건드리지 않는다.
    /// - 보류 단계가 모두 판정된 뒤에만 run을 재개한다(민감 DM은 종료·새 요청 필요).
    fn resolve_hold(
        &self,
        actor: PrincipalId,
        hold: HoldRef,
        verdict: HoldVerdict,
    ) -> Reply<'_, HoldDecision> {
        let mut d = self.lock();
        let out = Self::decide_hold(&mut d, actor, hold, verdict);
        Box::pin(async move { out })
    }
}

impl Fake {
    fn decide_hold(
        d: &mut Data,
        actor: PrincipalId,
        hold: HoldRef,
        verdict: HoldVerdict,
    ) -> Result<HoldDecision, Error> {
        let print = verdict_print(&verdict);
        // 1) 이미 끝난 식별자.
        if let Some(done) = d.settled.iter().find(|s| s.hold_id == hold.hold_id) {
            let same = done.actor == actor
                && done.verdict == print
                && (&done.profile, &done.run, &done.call, &done.effect)
                    == (&hold.profile, &hold.run, &hold.call, &hold.effect);
            return if same {
                Ok(HoldDecision::Repeated)
            } else {
                Err(Error::Conflict)
            };
        }
        // 2) 열린 보류 중에서 이 식별자를 가진 것: 단계 없는 보류 자체의 식별자 또는 판정할 수 있는 단계의 식별자.
        let open: Vec<HoldView> = d.holds.iter().filter(|h| d.hold_open(h)).cloned().collect();
        let found = open.iter().find_map(|h| {
            if h.hold_id.as_ref() == Some(&hold.hold_id) {
                return Some((h.clone(), None));
            }
            d.steps_of(h)
                .into_iter()
                .find(|e| e.hold_id.as_ref() == Some(&hold.hold_id) && e.state == EffectState::Held)
                .map(|e| (h.clone(), Some(e.effect)))
        });
        let Some((h, step)) = found else {
            return Err(Error::Missing);
        };
        // 다단계 보류의 묶음에는 식별자가 없다: 단계의 식별자를 단계 지정 없이 보낸 판정은 형식이 틀린 요청이다.
        if step.is_some() && hold.effect.is_none() {
            return Err(Error::Invalid);
        }
        if (&h.profile, &h.run, &h.call) != (&hold.profile, &hold.run, &hold.call)
            || step != hold.effect
        {
            return Err(Error::Conflict);
        }
        // 3) 적용: 단계는 확정(영수증) 또는 «시작함+미실행 판정 표지», 호출 전체 보류는 판정 표시.
        match &step {
            None => d.resolved.push(hold_key(&h)),
            Some(id) => {
                if let Some(e) = d.effects.iter_mut().find(|e| {
                    e.profile == h.profile && e.run == h.run && e.call == h.call && &e.effect == id
                }) {
                    let receipt = match &verdict {
                        HoldVerdict::Confirmed { receipt } => Some(receipt.clone()),
                        HoldVerdict::NotExecuted { .. } => None,
                    };
                    e.state = if receipt.is_some() {
                        EffectState::Confirmed
                    } else {
                        EffectState::Started
                    };
                    e.resolution = receipt.is_none().then_some(EffectResolution::NotExecuted);
                    (e.receipt, e.reason, e.hold_id) = (receipt, None, None); // 판정한 단계는 더 판정할 수 없다
                }
            }
        }
        if !d.hold_open(&h)
            && let Some(r) = d
                .runs
                .iter_mut()
                .find(|r| r.profile == h.profile && r.run == h.run)
        {
            r.waiting = None;
            // 민감 DM은 본문이 없어 다시 실행하지 못한다: 처리됨이면 완료, 미실행이면 거절로 끝내고 새 요청이 필요하다.
            // 그 밖의 호출은 같은 호출로 다시 이어 간다.
            r.state = match (h.kind, &verdict) {
                (HoldKind::Tool | HoldKind::Reply, _) => RunState::Ready,
                (HoldKind::Private, HoldVerdict::Confirmed { .. }) => RunState::Succeeded,
                (HoldKind::Private, HoldVerdict::NotExecuted { .. }) => {
                    r.error = Some("not_executed".to_owned());
                    RunState::Dead
                }
            };
        }
        let target = match &step {
            Some(id) => format!("{}:{}", h.call.0, id.0),
            None => h.call.0.clone(),
        };
        d.log(
            Some(&h.profile),
            &actor,
            "hold.resolve",
            &target,
            None,
            print.0,
        );
        d.settled.push(Settled {
            hold_id: hold.hold_id.clone(),
            profile: h.profile.clone(),
            run: h.run.clone(),
            call: h.call.clone(),
            effect: step,
            actor: actor.clone(),
            verdict: print,
        });
        d.resolutions.push(Resolution {
            actor,
            hold,
            verdict: print.0,
            digest: print.1,
        });
        Ok(HoldDecision::Applied)
    }
}

impl Broker for Fake {
    fn call(&self, _call: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(async { Err(Error::Invalid) })
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        let mut d = self.lock();
        let found = d.approvals.iter().position(|a| a.nonce == nonce);
        let out = match found {
            None => Err(Error::Missing),
            Some(i) if d.approvals[i].approver != actor => Err(Error::Denied),
            Some(i) if d.approvals[i].state != ApprovalState::Pending => Err(Error::Stale),
            Some(i) => {
                let a = &mut d.approvals[i];
                a.state = if allow {
                    ApprovalState::Approved
                } else {
                    ApprovalState::Denied
                };
                (a.decided_by, a.decided_at) = (Some(actor.clone()), Some(SystemTime::now()));
                let (profile, run, call, hash) = (
                    a.profile.clone(),
                    a.run.clone(),
                    a.call.clone(),
                    a.args_sha256,
                );
                if let Some(r) = d
                    .runs
                    .iter_mut()
                    .find(|r| r.profile == profile && r.run == run)
                {
                    (r.state, r.waiting) = (RunState::Ready, None);
                }
                if let Some(c) = d
                    .calls
                    .iter_mut()
                    .find(|c| c.profile == profile && c.call == call && c.run == run)
                {
                    c.state = if allow {
                        CallState::Running
                    } else {
                        CallState::Denied
                    };
                }
                d.log(
                    Some(&profile),
                    &actor,
                    "approval.decide",
                    &call.0,
                    Some(hash),
                    if allow { "allow" } else { "deny" },
                );
                d.decisions.push(Decision {
                    nonce,
                    actor,
                    allow,
                });
                Ok(())
            }
        };
        Box::pin(async move { out })
    }
}

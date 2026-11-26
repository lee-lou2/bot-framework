//! `bot_core::port`의 트레이트 구현. 다른 크레이트는 이 경계로만 저장소를 쓴다.
//! 내부 `Fail`은 여기서 `Error`로 바뀐다(DB 오류는 `Unavailable`).

use crate::{
    active::Extra,
    audit,
    db::{Fail, Store},
};
use bot_core::{
    admin::{
        AdminAct, AdminRead, ApprovalQuery, ApprovalView, AuditCheck, AuditQuery, HoldDecision,
        HoldRef, HoldVerdict, HoldView, Listing, ProfileStatus, RunDetail, RunQuery, RunView,
        Viewer,
    },
    api::ApprovalBlock,
    command::*,
    grant::{
        Authority, Evidence, GrantCas, GrantId, GrantPlan, GrantSpec, Granted, PauseOutcome,
        PreapprovalRow, TriggerKey,
    },
    id::*,
    intake::{
        DeliveryKey, DeliveryOutcome, Outbox, Outgoing, RequestKey, RequestState, RunProgress,
    },
    ledger::{
        ApprovalState, CallLedger, CallRecord, Decision, FinishCall, OpenApproval, PrivateLedger,
        PrivateOutcome, PrivatePermit, PrivateRequest, PrivateStart, StartCall, StoredApproval,
    },
    port::{
        AuditLog, DelegationStore, EffectJournal, EffectPermit, MemoryStore, Reply, RunAdmin,
        RunStore, ScheduleStore,
    },
    profile::ActiveRevisions,
    schedule::{
        AdvanceSchedule, DeliveryDraft, DeliveryFence, DeliveryTarget, DueKey, DuePage,
        ScheduleSpec, ScheduleSummary, ScheduledReplies, UnsentReason, UserSchedules, Wake,
    },
    state::{ConversationKind, RunContext},
};
use std::collections::BTreeMap;

/// 내부 비동기 함수를 `Reply`로 감싼다.
macro_rules! reply {
    ($body:expr) => {
        Box::pin(async move { $body.await.map_err(Fail::into_core) })
    };
}

impl RunStore for Store {
    fn accept(&self, command: Accept) -> Reply<'_, RunId> {
        reply!(self.accept_run(command))
    }

    fn accept_with(&self, command: Accept, extras: AcceptExtras) -> Reply<'_, RunId> {
        reply!(self.accept_run_with(command, extras))
    }

    fn status(&self, key: RequestKey) -> Reply<'_, Option<RequestState>> {
        reply!(async move { self.status_of(&key).await })
    }

    fn progress(&self, profile: ProfileId, run: RunId) -> Reply<'_, Option<RunProgress>> {
        reply!(async move { self.progress_of(&profile, &run).await })
    }

    fn claim(&self) -> Reply<'_, Option<RunContext>> {
        reply!(self.claim_run())
    }

    fn release_for_shutdown(&self, attempt: Attempt) -> Reply<'_, ()> {
        reply!(async move { self.release_for_shutdown_run(&attempt).await })
    }

    fn renew(&self, attempt: Attempt) -> Reply<'_, Beat> {
        reply!(async move { self.renew_run(&attempt).await })
    }

    fn checkpoint_batch(&self, attempt: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        reply!(async move { self.checkpoint_batch_run(&attempt, steps).await })
    }

    fn finish(&self, attempt: Attempt, done: Finish) -> Reply<'_, Completion> {
        reply!(async move { self.finish_run(&attempt, done).await })
    }

    fn finish_with(
        &self,
        attempt: Attempt,
        done: Finish,
        outcome: RunOutcome,
    ) -> Reply<'_, Completion> {
        reply!(async move { self.finish_run_with(&attempt, done, Some(outcome)).await })
    }

    fn fail(&self, attempt: Attempt, failure: Failure) -> Reply<'_, Retry> {
        reply!(async move { self.fail_run(&attempt, failure).await })
    }

    fn history(&self, attempt: Attempt, limit: u32) -> Reply<'_, Vec<Turn>> {
        reply!(async move { self.history_of(&attempt, limit).await })
    }
}

impl RunAdmin for Store {
    fn resume(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()> {
        reply!(async move { self.resume_run(&profile, &run, &actor, &reason).await })
    }

    fn cancel(
        &self,
        profile: ProfileId,
        run: RunId,
        actor: PrincipalId,
        reason: String,
    ) -> Reply<'_, ()> {
        reply!(async move { self.cancel_run(&profile, &run, &actor, &reason).await })
    }
}

impl AuditLog for Store {
    fn append(&self, entry: AuditEntry) -> Reply<'_, u64> {
        reply!(async move {
            let mut tx = self.pool.begin().await?;
            let mut item =
                audit::Entry::new(&entry.actor, &entry.action, &entry.target, &entry.verdict);
            item.args = entry.args_sha256;
            let seq = audit::append(&mut tx, &entry.profile.0, &item).await?;
            tx.commit().await?;
            Ok(seq as u64)
        })
    }

    fn verify(&self) -> Reply<'_, ChainReport> {
        reply!(audit::verify_chain(self))
    }
}

impl ScheduleStore for Store {
    fn accept_occurrence(&self, occurrence: Occurrence, command: Accept) -> Reply<'_, RunId> {
        reply!(self.accept_occurrence_run(occurrence, command))
    }

    fn snapshot(&self) -> Reply<'_, ScheduleSnapshot> {
        reply!(self.schedule_snapshot())
    }
}

impl DelegationStore for Store {
    fn result(&self, command: Delegate) -> Reply<'_, Option<JsonPayload>> {
        reply!(self.delegate_result(command))
    }

    fn delegate(&self, command: Delegate) -> Reply<'_, RunId> {
        reply!(self.delegate_run(command))
    }
}

impl Outbox for Store {
    fn claim(&self, kinds: &[ConversationKind], limit: u32) -> Reply<'_, Vec<Outgoing>> {
        let kinds = kinds.to_vec();
        reply!(async move { self.outbox_claim(&kinds, limit).await })
    }

    fn begin(&self, key: DeliveryKey, checked_revision: Revision) -> Reply<'_, ()> {
        reply!(async move { self.outbox_begin(&key, &checked_revision).await })
    }

    fn settle(&self, key: DeliveryKey, outcome: DeliveryOutcome) -> Reply<'_, ()> {
        reply!(async move { self.outbox_settle(&key, outcome).await })
    }
}

impl ActiveRevisions for Store {
    fn current(&self) -> Reply<'_, Option<Revision>> {
        reply!(self.current_active())
    }

    fn replace(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
    ) -> Reply<'_, ()> {
        reply!(async move {
            let extra = Extra {
                prepared: None,
                grants: &[],
            };
            self.replace_active(expected, next, profiles, seeds, schedules, extra)
                .await
                .map(|_| ())
        })
    }

    fn replace_prepared(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
        prepared: std::collections::BTreeSet<(ProfileId, Revision)>,
    ) -> Reply<'_, ()> {
        reply!(async move {
            let extra = Extra {
                prepared: Some(&prepared),
                grants: &[],
            };
            self.replace_active(expected, next, profiles, seeds, schedules, extra)
                .await
                .map(|_| ())
        })
    }

    fn replace_granted(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
        plan: GrantPlan,
    ) -> Reply<'_, Vec<(GrantId, GrantSpec)>> {
        reply!(async move {
            let extra = Extra {
                prepared: Some(&plan.prepared),
                grants: &plan.grants,
            };
            self.replace_active(expected, next, profiles, seeds, schedules, extra)
                .await
        })
    }
}

impl CallLedger for Store {
    fn approval_context(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, RunContext> {
        reply!(async move { self.approval_context(&nonce, &actor).await })
    }

    fn lookup(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CallRecord>> {
        reply!(async move { self.call_lookup(&attempt, &call).await })
    }

    fn active_revision(&self, attempt: Attempt) -> Reply<'_, Option<Revision>> {
        reply!(async move { self.call_active_revision(&attempt).await })
    }

    fn step(&self, attempt: Attempt, key: String) -> Reply<'_, Option<JsonPayload>> {
        reply!(async move { self.call_step(&attempt, &key).await })
    }

    fn context(&self, attempt: Attempt) -> Reply<'_, RunContext> {
        reply!(async move { self.call_context(&attempt).await })
    }

    fn begin(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args_sha256: [u8; 32],
    ) -> Reply<'_, CallRecord> {
        reply!(async move { self.call_begin(&attempt, &call, &tool, args_sha256).await })
    }

    fn open_approval(&self, open: OpenApproval) -> Reply<'_, ()> {
        reply!(self.approval_open(open))
    }

    fn approval(&self, nonce: ApprovalId) -> Reply<'_, StoredApproval> {
        reply!(async move { self.approval_get(&nonce).await })
    }

    fn supersede(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, ()> {
        reply!(async move { self.approval_supersede(&nonce, &actor).await })
    }

    fn decide(&self, decision: Decision) -> Reply<'_, ApprovalState> {
        reply!(self.approval_decide(decision))
    }

    fn start(&self, start: StartCall) -> Reply<'_, ()> {
        reply!(self.call_start(start))
    }

    fn start_with(&self, start: StartCall, authority: Authority) -> Reply<'_, Granted> {
        reply!(self.call_start_with(start, authority))
    }

    fn evidence(&self, attempt: Attempt) -> Reply<'_, Option<Evidence>> {
        reply!(async move { self.call_evidence(&attempt).await })
    }

    fn finish(&self, finish: FinishCall) -> Reply<'_, ()> {
        reply!(self.call_finish(finish))
    }

    fn finish_blocked(&self, finish: FinishCall, why: ApprovalBlock) -> Reply<'_, ()> {
        reply!(self.call_finish_with(finish, Some(why)))
    }

    fn finish_pass(
        &self,
        finish: FinishCall,
        pos: u32,
        runnable: std::collections::BTreeSet<AgentId>,
    ) -> Reply<'_, bot_core::pass::PassNext> {
        reply!(self.call_finish_pass(finish, pos, runnable))
    }
}

impl bot_core::slack::BotThreads for Store {
    fn note(&self, thread: bot_core::slack::BotThread) -> Reply<'_, ()> {
        reply!(async move { self.thread_note(&thread).await })
    }

    fn known(&self, thread: bot_core::slack::BotThread) -> Reply<'_, bool> {
        reply!(async move { self.thread_known(&thread).await })
    }
}

impl EffectJournal for Store {
    fn request(&self, request: EffectRequest) -> Reply<'_, EffectPermit> {
        reply!(self.effect_request(request))
    }

    fn lookup(&self, request: EffectRequest, tool: ToolId) -> Reply<'_, Option<EffectStatus>> {
        reply!(async move { self.effect_lookup(request, &tool).await })
    }

    fn status(&self, attempt: Attempt, call: CallId, effect: EffectId) -> Reply<'_, EffectStatus> {
        reply!(async move { self.effect_status(&attempt, &call, &effect).await })
    }

    fn unresolved(&self, attempt: Attempt, call: CallId) -> Reply<'_, Vec<EffectStatus>> {
        reply!(async move { self.effect_unresolved(&attempt, &call).await })
    }

    fn record(&self, attempt: Attempt, call: CallId, status: EffectStatus) -> Reply<'_, ()> {
        reply!(async move { self.effect_record(&attempt, &call, status).await })
    }
}

impl MemoryStore for Store {
    fn preview(&self, attempt: Attempt, id: MemoryId) -> Reply<'_, MemoryPreview> {
        reply!(async move { self.memory_preview(&attempt, &id).await })
    }

    fn share(&self, start: StartCall, id: MemoryId) -> Reply<'_, MemoryId> {
        reply!(self.memory_share(start, id))
    }

    fn search(&self, attempt: Attempt, query: String) -> Reply<'_, Vec<MemoryHit>> {
        reply!(async move { self.memory_search(&attempt, &query).await })
    }

    fn change(&self, attempt: Attempt, change: MemoryChange) -> Reply<'_, MemoryId> {
        reply!(async move { self.memory_change(&attempt, change).await })
    }

    fn seed(&self, seed: MemorySeed) -> Reply<'_, ()> {
        reply!(self.memory_seed(seed))
    }
}

impl PrivateLedger for Store {
    fn begin(&self, request: PrivateRequest) -> Reply<'_, PrivatePermit> {
        reply!(self.private_begin(request))
    }

    fn start(&self, start: PrivateStart) -> Reply<'_, ()> {
        reply!(self.private_start(start))
    }

    fn finish(&self, permit: Attempt, outcome: PrivateOutcome) -> Reply<'_, ()> {
        reply!(async move { self.private_finish(&permit, outcome).await })
    }
}

impl AdminRead for Store {
    fn runs(&self, viewer: &Viewer, query: RunQuery) -> Reply<'_, Listing<RunView>> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_runs(&viewer, query).await })
    }

    fn detail(&self, viewer: &Viewer, profile: ProfileId, run: RunId) -> Reply<'_, RunDetail> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_detail(&viewer, &profile, &run).await })
    }

    fn approvals(&self, viewer: &Viewer, query: ApprovalQuery) -> Reply<'_, Listing<ApprovalView>> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_approvals(&viewer, query).await })
    }

    fn approval(&self, viewer: &Viewer, nonce: ApprovalId) -> Reply<'_, ApprovalView> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_approval(&viewer, &nonce).await })
    }

    fn holds(&self, viewer: &Viewer, profile: Option<ProfileId>) -> Reply<'_, Vec<HoldView>> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_holds(&viewer, profile.as_ref()).await })
    }

    fn audit(
        &self,
        viewer: &Viewer,
        query: AuditQuery,
    ) -> Reply<'_, Vec<bot_core::admin::AuditEntry>> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_audit(&viewer, query).await })
    }

    fn verify_audit(&self, viewer: &Viewer) -> Reply<'_, AuditCheck> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_verify(&viewer).await })
    }

    fn status(&self, viewer: &Viewer) -> Reply<'_, Vec<ProfileStatus>> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_status(&viewer).await })
    }

    fn preapprovals(&self, viewer: &Viewer) -> Reply<'_, Vec<PreapprovalRow>> {
        let viewer = viewer.clone();
        reply!(async move { self.admin_preapprovals(&viewer).await })
    }
}

impl AdminAct for Store {
    fn resolve_hold(
        &self,
        actor: PrincipalId,
        hold: HoldRef,
        verdict: HoldVerdict,
    ) -> Reply<'_, HoldDecision> {
        reply!(async move { self.resolve_hold_run(&actor, &hold, &verdict).await })
    }

    fn set_pause(
        &self,
        actor: PrincipalId,
        key: TriggerKey,
        cas: GrantCas,
        pause: bool,
    ) -> Reply<'_, PauseOutcome> {
        reply!(async move { self.pause_run(&actor, &key, &cas, pause).await })
    }
}

impl ScheduledReplies for Store {
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<JsonPayload>> {
        reply!(async move { self.delivery_any_receipt(&attempt).await })
    }

    fn target(&self, attempt: Attempt) -> Reply<'_, Option<DeliveryTarget>> {
        reply!(async move { self.delivery_target(&attempt).await })
    }

    fn finish_unsent(
        &self,
        fence: DeliveryFence,
        target: DeliveryTarget,
        reason: UnsentReason,
    ) -> Reply<'_, JsonPayload> {
        reply!(self.delivery_finish_unsent(fence, target, reason))
    }

    fn draft(
        &self,
        attempt: Attempt,
        completion: Option<Finish>,
    ) -> Reply<'_, Option<DeliveryDraft>> {
        reply!(async move { self.delivery_draft(&attempt, completion).await })
    }

    fn enqueue(&self, approval: StartCall) -> Reply<'_, ()> {
        reply!(self.delivery_enqueue(approval))
    }
}

impl UserSchedules for Store {
    fn create(&self, start: StartCall, spec: ScheduleSpec) -> Reply<'_, ScheduleId> {
        reply!(self.schedule_create(start, spec))
    }

    fn list(&self, attempt: Attempt, checked: Revision) -> Reply<'_, Vec<ScheduleSummary>> {
        reply!(async move { self.schedule_list(&attempt, &checked).await })
    }

    fn cancel(&self, start: StartCall, id: ScheduleId) -> Reply<'_, ()> {
        reply!(self.schedule_cancel(start, id))
    }

    fn finish_with_wake(
        &self,
        attempt: Attempt,
        call: CallId,
        checked: Revision,
        result: JsonPayload,
        wake: Wake,
    ) -> Reply<'_, ScheduleId> {
        reply!(async move {
            self.schedule_wake(&attempt, &call, &checked, result, wake)
                .await
        })
    }

    fn due(&self, after: Option<DueKey>, limit: u32) -> Reply<'_, DuePage> {
        reply!(self.schedule_due(after, limit))
    }

    fn advance(&self, command: AdvanceSchedule) -> Reply<'_, Option<RunId>> {
        reply!(self.schedule_advance(command))
    }
}

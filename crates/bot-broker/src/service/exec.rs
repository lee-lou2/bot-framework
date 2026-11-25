use super::{BrokerService, call::Flow};
use crate::{
    classify::{Class, classify},
    ports::*,
    recovery::{Facts, Plan, plan},
    text::json_quote,
};
use bot_core::{
    command::*,
    id::Attempt,
    ledger::*,
    port::Error,
    profile::{Destination, Snapshot},
    state::*,
};

/// 복구 판정의 결과.
pub(super) enum Recovered {
    /// 이 결과로 끝난다(영수증으로 완료했거나 보류를 유지한다).
    Finished(CallResult),
    /// 미실행이 입증돼 호출이 `Open`으로 돌아왔다. 이어서 현재 권한·승인을 거친다.
    Open,
}

impl BrokerService {
    /// 판정에 쓴 판본이 이 프로세스의 카탈로그에서 아직 활성인가(시작 직전의 값싼 확인. 원장이 `checked_revision`으로 같은
    /// 잠금에서 한 번 더 닫는다).
    pub(super) fn still_current_local(&self, flow: &Flow) -> Result<(), Error> {
        self.revision_local(&flow.attempt, &flow.snapshot)
    }

    fn revision_local(&self, attempt: &Attempt, snapshot: &Snapshot) -> Result<(), Error> {
        let latest = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        if latest.revision != snapshot.revision
            || latest.policy_revision != snapshot.policy_revision
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// 결과를 돌려주기 직전의 확인: 이 프로세스의 카탈로그와, 저장소가 같은 시점에서 보는 활성 판본·현재 claim을 모두 본다
    /// (다른 프로세스가 새 판본을 게시했거나 claim이 넘어갔으면 결과를 숨긴다). 결과를 돌려주기 전 마지막 `await`여야 한다.
    pub(super) async fn still_current(&self, flow: &Flow) -> Result<(), Error> {
        self.revision_current(&flow.attempt, &flow.snapshot).await
    }

    pub(super) async fn revision_current(
        &self,
        attempt: &Attempt,
        snapshot: &Snapshot,
    ) -> Result<(), Error> {
        self.revision_local(attempt, snapshot)?;
        match self.parts.ledger.active_revision(attempt.clone()).await? {
            Some(active) if active == snapshot.revision => Ok(()),
            Some(_) => Err(Error::Conflict),
            None => Err(Error::Denied),
        }
    }

    pub(super) async fn finish(&self, flow: &Flow, outcome: Outcome) -> Result<(), Error> {
        self.parts
            .ledger
            .finish(FinishCall {
                attempt: flow.attempt.clone(),
                call: flow.call.clone(),
                outcome,
            })
            .await
    }

    /// 시작을 커밋한 뒤에만 실행기를 부르고, 결과 종류에 따라 호출을 닫는다.
    pub(super) async fn execute(
        &self,
        flow: &Flow,
        class: &Class,
        approval: Option<(Approval, Vec<String>)>,
    ) -> Result<CallResult, Error> {
        let owner = format!(
            "{}.{}.{}",
            flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
        );
        // 판정에 쓴 판본이 아직 활성인지 시작 직전에 다시 본다. 판정과 시작 사이에 철회됐으면 시작하지 않고
        // (같은 호출 ID로 다시 들어오면 새 판본으로 다시 판정한다) 실행기도 부르지 않는다.
        // 재확인과 커밋 사이의 틈은 원장이 `checked_revision`으로 같은 잠금에서 닫는다.
        self.still_current_local(flow)?;
        self.parts
            .ledger
            .start(StartCall {
                checked_revision: flow.snapshot.revision.clone(),
                risk: class.risk,
                tool_revision: flow.resolved.tool.revision.clone(),
                attempt: flow.attempt.clone(),
                call: flow.call.clone(),
                approval,
                private_recall: flow.ctx.private_memory_read,
                owner: owner.clone(),
            })
            .await?;
        let reply_to = matches!(flow.resolved.tool.destination, Some(Destination::Reply))
            .then(|| flow.ctx.conversation.clone());
        let job = ToolJob {
            attempt: flow.attempt.clone(),
            principal: flow.ctx.principal.clone(),
            origin: flow.ctx.origin,
            call: flow.call.clone(),
            tool: flow.resolved.clone(),
            args: flow.sealed.canonical.clone(),
            env: flow.env.clone(),
            reply_to,
            owner,
        };
        // 시작 뒤 실행기 자체가 실패하면 보냈는지 알 수 없다.
        let ran = self
            .parts
            .runner
            .run(job)
            .await
            .unwrap_or(Ran::Unknown("runner_error"));
        match ran {
            Ran::Done(payload) => {
                if flow.resolved.tool.effects && !self.steps_settled(flow).await {
                    self.finish(flow, Outcome::Held("effects_unresolved".into()))
                        .await?;
                    return Ok(CallResult::Held(flow.call.clone()));
                }
                // 모델이 보는 결과에서 후속 확인 요청(`_meta["bot/wake"]`)은 늘 떼어 낸다. 요청이 있으면 결과와 한 커밋으로 예약한다.
                let extracted = super::wake::extract(&payload, self.parts.clock.now());
                if extracted.request.is_some() {
                    return self.finish_with_wake(flow, extracted).await;
                }
                self.finish(flow, Outcome::Done(payload.clone())).await?;
                // 결과는 먼저 원장에 보존했다. 마지막 await 직후에도 판정에 쓴 판본이 활성일 때만 호출자에게 돌려준다.
                self.still_current(flow).await?;
                Ok(CallResult::Done(payload))
            }
            // 서버가 실행 전에 확정적으로 거절했다: 외부 효과가 없고, 모델은 고정 오류 결과를 본다.
            Ran::Rejected => {
                let payload = JsonPayload(crate::mcp::error_payload());
                self.finish(flow, Outcome::Done(payload.clone())).await?;
                self.still_current(flow).await?;
                Ok(CallResult::Done(payload))
            }
            // 보내기 전에 이쪽이 확정적으로 거절했다(도구 정의 불일치 등): 외부 효과가 없고 호출은 거절로 닫힌다. 고정 코드를 감사에 남긴다.
            Ran::Refused(why) => {
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "call.refuse",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    why,
                )
                .await?;
                self.finish(flow, Outcome::Blocked).await?;
                Err(Error::Denied)
            }
            Ran::NotSent => {
                self.finish(flow, Outcome::Released).await?;
                Err(Error::Unavailable)
            }
            // 읽기는 반복해도 안전하다고 선언한 도구다. 쓰기는 보냈는지 모르므로 멈춘다.
            Ran::Unknown(_) if class.risk == Risk::Read => {
                self.finish(flow, Outcome::Released).await?;
                Err(Error::Unavailable)
            }
            Ran::Unknown(why) => {
                self.finish(flow, Outcome::Held(why.into())).await?;
                Ok(CallResult::Held(flow.call.clone()))
            }
            // 결과를 막았어도 도구는 실행됐다. 읽기는 외부 효과가 없으므로 거절로 닫지만, 쓰기는 효과가 불명이므로 보류한다.
            Ran::Blocked(why) => {
                self.note(
                    &flow.attempt.profile,
                    &flow.ctx.principal,
                    "call.blocked",
                    &flow.tool,
                    Some(flow.sealed.sha256),
                    why,
                )
                .await?;
                if class.risk == Risk::Read {
                    self.finish(flow, Outcome::Blocked).await?;
                    return Err(Error::Denied);
                }
                self.finish(flow, Outcome::Held(why.into())).await?;
                Ok(CallResult::Held(flow.call.clone()))
            }
        }
    }

    /// 단계 원장에 미결 단계가 없는가. 조회하지 못하면 확인된 것이 아니다.
    async fn steps_settled(&self, flow: &Flow) -> bool {
        self.parts
            .effects
            .unresolved(flow.attempt.clone(), flow.call.clone())
            .await
            .is_ok_and(|steps| steps.is_empty())
    }

    /// 옛 시도가 시작했거나 멈춘 호출을 같은 호출 ID로 다시 실행해도 되는지 판정한다.
    /// 다시 실행해도 되면 호출을 `Open`으로 되돌린다(미실행 입증). 관리자 판정(`Held`의 `resolution`)은
    /// 옛 도구 종료와 수신처 미실행 증거 확인을 포함하므로 프로세스를 다시 확인하지 않는다.
    pub(super) async fn recover(&self, flow: &Flow, phase: &CallPhase) -> Result<Recovered, Error> {
        let (reaped, resolution, started) = match phase {
            // 원격 서버로 간 호출은 연결을 닫아도 원격 실행이 계속될 수 있어 `reap` 성공이 «끝났다»의 증명이 아니다(`reap_proves_end`)
            CallPhase::Started { owner, .. } => (
                self.parts.runner.reap(owner.clone()).await.is_ok()
                    && self
                        .parts
                        .runner
                        .reap_proves_end(&flow.resolved.tool.server),
                None,
                true,
            ),
            CallPhase::Held { resolution, .. } => (true, resolution.clone(), false),
            _ => return Ok(Recovered::Open),
        };
        let tool = &flow.resolved.tool;
        let unresolved = if tool.effects {
            self.parts
                .effects
                .unresolved(flow.attempt.clone(), flow.call.clone())
                .await
                .ok()
                .map(|steps| steps.len())
        } else {
            Some(0)
        };
        let risk = classify(tool, &flow.snapshot.policy.approve, false).risk;
        let facts = Facts {
            class: risk,
            effects: tool.effects,
            reaped,
            unresolved,
            resolution,
        };
        match plan(&facts) {
            Plan::Resume => {
                self.finish(flow, Outcome::Released).await?;
                Ok(Recovered::Open)
            }
            Plan::Complete(receipt) => {
                let payload = JsonPayload(format!("{{\"receipt\":{}}}", json_quote(&receipt)));
                self.finish(flow, Outcome::Done(payload.clone())).await?;
                // 영수증은 먼저 원장에 보존했다. 마지막 await 직후에도 판본이 활성일 때만 돌려준다.
                self.still_current(flow).await?;
                Ok(Recovered::Finished(CallResult::Done(payload)))
            }
            Plan::Hold(code) => {
                if started {
                    self.finish(flow, Outcome::Held(code.into())).await?;
                }
                Ok(Recovered::Finished(CallResult::Held(flow.call.clone())))
            }
        }
    }
}

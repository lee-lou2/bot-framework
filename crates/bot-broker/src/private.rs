//! private DM 접수(`PrivateIntake`). 민감 본문은 지정한 도구에만 전달하고 일반 큐·감사·승인 카드·모델 맥락·추적에
//! 평문으로 남기지 않는다. 본문은 메모리에서만 처리하고 저장소에는 접수 키와 프로필별 HMAC 지문만 남는다.
//!
//! 일반 호출 원장(`CallLedger`)·워커·모델·기억·회신 포트에는 private 권한을 열지 않는다. 전용 원장(`PrivateLedger`)만
//! `begin`(리스 점유) → `start`(전송 직전 한 번) → `finish`(고정된 결과 종류만)를 바꾼다. 도구의 결과·오류·영수증은 저장하지
//! 않고 버린다(도구가 입력을 결과에 되돌려 줄 수 있다).
//!
//! 순서: 게이트웨이가 넘긴 판본이 지금 활성 판본과 같은지 → 현재 판본의 private 트리거(그 바인딩·에이전트·도구)와 대조 →
//! 승인 카드가 필요한 도구·단계 원장을 쓰는 도구(`effects`)는 거절 → 주체의 현재 권한 → 입력 봉인(검증·정규화) →
//! 프로필별 HMAC 지문 → `begin`. 같은 접수 키의 재전달은 호출을 늘리지 않는다(`InProgress`는 ACK하지 않고 다시 시도하게 한다).
//! 시작 전 실패는 리스를 반납해(`RetryBeforeStart`) 같은 입력의 재전달이 이어 가게 하고, 전송 뒤 결과를 모르면 보류한다.

use crate::{
    classify::classify,
    keyed::hmac,
    ports::{Ran, Sealer, ToolJob, ToolRunner},
    rate::{Limiter, RateConfig},
    resolve::{Resolved, find_tool, pinned_to_image, selected_env},
    scope::effective,
    sys::Clock,
    text::hex,
};
use bot_core::{
    id::*,
    intake::{PrivateCall, PrivateDecision, PrivateIntake},
    ledger::{PrivateLedger, PrivateOutcome, PrivatePermit, PrivateRequest, PrivateStart},
    port::{Error, Reply, SecretStore},
    profile::{Adapter, Destination, ProfileCatalog, TriggerSource, TriggerTarget},
    state::{Conversation, ConversationKind, RunOrigin},
    trace::{SpanIds, SpanRecord, Tracer, key},
};
use std::sync::Arc;

/// 프로필별 지문 키의 자격 이름. 운영자가 프로필마다 등록한다. 없으면 안전하게 중복을 가릴 수 없으므로 접수하지 않는다.
pub const FINGERPRINT_SECRET: &str = "private-intake";

/// 호스트만 쓰는 자격 이름인가. 도구의 환경으로는 이름이 같아도 풀어 전달하지 않는다(일반 호출·비공개 접수 모두).
pub fn reserved_secret(name: &SecretName) -> bool {
    name.as_str() == FINGERPRINT_SECRET
}

pub struct PrivateParts {
    pub catalog: Arc<dyn ProfileCatalog>,
    pub ledger: Arc<dyn PrivateLedger>,
    pub secrets: Arc<dyn SecretStore>,
    pub sealer: Arc<dyn Sealer>,
    pub runner: Arc<dyn ToolRunner>,
    pub tracer: Arc<dyn Tracer>,
    pub clock: Arc<dyn Clock>,
}

pub struct PrivateGate {
    parts: PrivateParts,
    limiter: Arc<Limiter>,
}

/// 접수를 마친 한 건의 실행에 필요한 값. 원문은 `canonical`뿐이고 실행기에만 간다.
struct Exec {
    attempt: Attempt,
    principal: PrincipalId,
    /// 게이트웨이가 인증한 입구(대화 종류와 발신자)로 정한 출처.
    origin: RunOrigin,
    tool: ToolId,
    resolved: Resolved,
    canonical: String,
    /// 도구가 고른 환경 변수(일반 호출과 같은 규칙으로 읽은 값).
    env: Vec<(String, String)>,
    fingerprint: [u8; 32],
    checked_revision: Revision,
    reply_to: Option<Conversation>,
    private_pass: bool,
}

fn error_class(error: &Error) -> &'static str {
    match error {
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Unavailable => "unavailable",
        Error::Invalid | Error::Missing => "invalid",
    }
}

impl PrivateIntake for PrivateGate {
    fn receive(&self, call: PrivateCall) -> Reply<'_, PrivateDecision> {
        Box::pin(async move {
            let (profile, request) = (call.key.profile.clone(), call.key.request.0.clone());
            let mut label = "rejected";
            let result = self.receive_inner(call, &mut label).await;
            self.span(
                &profile,
                &request,
                "broker.private.intake",
                label,
                result.as_ref().err(),
            );
            result
        })
    }
}

impl PrivateGate {
    pub fn new(parts: PrivateParts, rate: RateConfig) -> Self {
        let limiter = Limiter::new(rate, Arc::clone(&parts.clock));
        Self { parts, limiter }
    }

    async fn receive_inner(
        &self,
        call: PrivateCall,
        label: &mut &'static str,
    ) -> Result<PrivateDecision, Error> {
        let PrivateCall {
            key,
            revision,
            conversation,
            agent,
            tool,
            args,
        } = call;
        let parts = &self.parts;
        let snapshot = parts.catalog.current(&key.profile).ok_or(Error::Denied)?;
        // 게이트웨이가 확인한 판본은 지금 활성 판본이어야 한다(다르면 지금 판본으로 바꿔치기하지 않고 거절한다).
        if revision != snapshot.revision {
            return Err(Error::Stale);
        }
        // 민감 입력은 1:1 대화(DM)로만 받는다: 대화 종류가 Direct이고 바인딩이 이 에이전트(또는 조상)의 Slack DM이어야 한다.
        let dm = snapshot.bindings.get(&key.binding).is_some_and(|b| {
            (b.agent == agent || agent.0.starts_with(&format!("{}/", b.agent.0)))
                && matches!(b.source, Adapter::SlackDm { .. })
        });
        if conversation.kind != ConversationKind::Direct || !dm {
            return Err(Error::Denied);
        }
        // 지금 판본에 이 바인딩·에이전트·도구를 가리키는 private 트리거가 있어야 한다.
        let owner = snapshot.agents.get(&agent).ok_or(Error::Denied)?;
        let trigger = owner.triggers.iter().find(|t| {
            t.private
                && matches!(&t.on, TriggerSource::Event { binding, .. } if *binding == key.binding)
                && matches!(&t.target, TriggerTarget::Tool { tool: target, .. } if *target == tool)
        });
        let private_pass = trigger.is_some_and(|t| t.private_pass);
        let declared_tool = find_tool(&snapshot, &agent, &tool)
            .filter(|_| trigger.is_some())
            .ok_or(Error::Denied)?;
        // 승인 카드는 인자를 보여 줘야 하고, 단계 원장은 입력이 섞인 인자 해시를 저장하므로 둘 다 민감 입력에는 쓰지 않는다.
        if declared_tool.effects
            || classify(declared_tool, &snapshot.policy.approve, false).needs_approval()
        {
            return Err(Error::Denied);
        }
        if declared_tool
            .secrets
            .values()
            .any(|name| !snapshot.secrets.contains(name))
        {
            return Err(Error::Denied);
        }
        let scopes = effective(&snapshot, &key.principal, &agent).ok_or(Error::Denied)?;
        if declared_tool.scopes.is_empty() || !declared_tool.scopes.is_subset(&scopes) {
            return Err(Error::Denied);
        }
        // 설치한 서버가 있어야 하고, 도구 판본에 설치 판본을 묶는다(일반 호출과 같은 규칙).
        let image = parts
            .runner
            .installed(&declared_tool.server)
            .ok_or(Error::Denied)?;
        // 샌드박스가 실행할 수 없는 서버(상태 볼륨을 요구하는데 지원하지 않음)는 접수를 기록하기 전에 거절한다.
        parts.runner.admit(&declared_tool.server)?;
        let runnable = pinned_to_image(declared_tool.clone(), &image);
        // 도구가 고른 환경 변수의 값은 이 에이전트의 환경에서만 읽는다(일반 호출과 같다. 이름이 없으면 시작하지 않는다).
        let env = selected_env(declared_tool, &agent, &snapshot, &snapshot).ok_or(Error::Denied)?;
        let sealed = parts.sealer.seal(declared_tool, &args)?;
        drop(args);
        let secret = SecretName::parse(FINGERPRINT_SECRET).ok_or(Error::Unavailable)?;
        let hmac_key = parts
            .secrets
            .for_profile(key.profile.clone())
            .await?
            .resolve(secret)
            .await
            .map_err(|_| Error::Unavailable)?
            .0;
        let fingerprint = hmac(
            &hmac_key,
            &[
                key.profile.0.as_bytes(),
                key.binding.0.as_bytes(),
                key.principal.0.as_bytes(),
                key.request.0.as_bytes(),
                agent.0.as_bytes(),
                tool.0.as_bytes(),
                // 회신 목적지(대화 ID·스레드)를 최초 요청에 묶는다: 같은 인자라도 다른 대화·스레드면 다른 요청이다.
                conversation.id.as_bytes(),
                conversation.thread.as_deref().unwrap_or("").as_bytes(),
                &[u8::from(conversation.thread.is_some())],
                sealed.canonical.as_bytes(),
            ],
        );
        let permit = parts
            .ledger
            .begin(PrivateRequest {
                key: key.clone(),
                fingerprint,
                revision: revision.clone(),
                agent,
                tool: tool.clone(),
                conversation: conversation.clone(),
                scopes,
            })
            .await?;
        match permit {
            PrivatePermit::Start(attempt) => {
                *label = "accepted";
                // 출처는 게이트웨이가 인증한 입구(대화 종류·발신자)로 정한다. 모델 인자나 요청 ID로 추정하지 않는다.
                let origin = RunOrigin::from_ingress(&conversation.kind, &key.principal);
                let reply_to = matches!(runnable.destination, Some(Destination::Reply))
                    .then_some(conversation);
                let run = Exec {
                    attempt,
                    origin,
                    principal: key.principal,
                    resolved: Resolved::new(tool.clone(), runnable).ok_or(Error::Invalid)?,
                    tool,
                    canonical: sealed.canonical,
                    env,
                    fingerprint,
                    checked_revision: snapshot.revision.clone(),
                    reply_to,
                    private_pass,
                };
                let result = self.execute(run).await;
                if result.is_err() {
                    *label = "failed";
                }
                result
            }
            // 같은 전달이 아직 처리 중이거나 시작 여부가 정해지지 않았다: ACK하지 않고 다시 시도하게 한다.
            PrivatePermit::InProgress => {
                *label = "in_progress";
                Err(Error::Unavailable)
            }
            // 이미 끝났거나 내구 보류다. 호출을 늘리지 않는다.
            PrivatePermit::Completed | PrivatePermit::Held => {
                *label = "duplicate";
                Ok(PrivateDecision::Consumed)
            }
            PrivatePermit::Rejected => {
                *label = "duplicate";
                Err(Error::Denied)
            }
        }
    }

    /// 리스를 쥔 접수 한 건을 실행한다. 시작 기록 전의 실패는 리스를 반납해 같은 입력의 재전달이 이어 가게 하고,
    /// 시작한 뒤에는 결과 종류만 고정 어휘로 남긴다(도구가 돌려준 본문·오류·영수증은 버린다).
    async fn execute(&self, run: Exec) -> Result<PrivateDecision, Error> {
        let (ledger, profile) = (&self.parts.ledger, run.attempt.profile.clone());
        let release = |attempt: Attempt| async move {
            let _ = ledger
                .finish(attempt, PrivateOutcome::RetryBeforeStart)
                .await;
        };
        let Ok(_slot) = self.limiter.admit(&profile, &run.principal) else {
            release(run.attempt).await;
            return Err(Error::Unavailable);
        };
        // 판정에 쓴 판본이 아직 활성인지 전송 직전에 다시 본다. 저장소도 `start`에서 같은 잠금 안에서 대조한다.
        let still = self
            .parts
            .catalog
            .current(&profile)
            .is_some_and(|s| s.revision == run.checked_revision);
        if !still {
            release(run.attempt).await;
            return Err(Error::Stale);
        }
        let started = ledger
            .start(PrivateStart {
                attempt: run.attempt.clone(),
                tool: run.tool.clone(),
                fingerprint: run.fingerprint,
                checked_revision: run.checked_revision.clone(),
            })
            .await;
        if let Err(error) = started {
            // 저장소가 시작을 커밋하지 않았다면 반납된다. 커밋했는데 응답을 잃었다면 반납은 거절되고 보류로 이어진다.
            release(run.attempt).await;
            return Err(error);
        }
        let tag = hex(&run.fingerprint[..8]);
        let job = ToolJob {
            attempt: run.attempt.clone(),
            principal: run.principal,
            origin: run.origin,
            call: CallId(format!("p-{tag}")),
            tool: run.resolved,
            args: run.canonical,
            env: run.env,
            reply_to: run.reply_to,
            owner: format!(
                "private.{}.{}.{tag}",
                run.attempt.run.0, run.attempt.generation.0
            ),
        };
        let (outcome, passed) = match self.parts.runner.run(job).await {
            // 도구가 오류로 끝났다고 알려도(`isError`) 일부 효과가 났을 수 있다: 성공으로 적지 않고 보류한다.
            Ok(Ran::Done(payload)) if reports_error(&payload.0) => (PrivateOutcome::Held, false),
            // 민감 DM 전용 도구가 «처리하지 않음» 표식을 돌려줬다: `private_pass: true`이고 유효한 빈 표식일 때만 일반 DM 대화로 넘긴다.
            Ok(Ran::Done(payload)) if crate::service::pass::carries_marker(&payload.0) => {
                let (_, marker) = crate::service::pass::split(&payload);
                if run.private_pass && marker.is_some_and(|m| m.valid) {
                    (PrivateOutcome::Completed, true)
                } else {
                    (PrivateOutcome::Held, false)
                }
            }
            Ok(Ran::Done(_)) => (PrivateOutcome::Completed, false),
            // 서버가 실행 전에 확정적으로 거절했다(잘못된 요청·인자): 외부 효과가 없다.
            Ok(Ran::Rejected | Ran::Refused(_)) => (PrivateOutcome::Rejected, false),
            Ok(Ran::NotSent) => (PrivateOutcome::NotSent, false),
            // 결과를 막았어도 도구는 이미 실행됐다(외부 효과가 불명이다): 거절로 단정하지 않고 보류한다.
            // 보냈는지 모르거나 실행기가 실패했을 때도 보류하고 원문으로 다시 실행하지 않는다.
            Ok(Ran::Blocked(_) | Ran::Unknown(_)) | Err(_) => (PrivateOutcome::Held, false),
        };
        let result = match outcome {
            PrivateOutcome::Completed if passed => Ok(PrivateDecision::Pass),
            PrivateOutcome::Completed | PrivateOutcome::Held => Ok(PrivateDecision::Consumed),
            PrivateOutcome::NotSent | PrivateOutcome::RetryBeforeStart => Err(Error::Unavailable),
            PrivateOutcome::Rejected => Err(Error::Denied),
        };
        self.span(
            &profile,
            &run.attempt.run.0,
            "broker.private.exec",
            outcome_label(&outcome),
            result.as_ref().err(),
        );
        ledger.finish(run.attempt, outcome).await?;
        result
    }

    /// 계측: 분류만 남기고 원문·주체는 싣지 않는다. 모든 스팬에 private 표지를 붙인다.
    fn span(
        &self,
        profile: &ProfileId,
        anchor: &str,
        name: &str,
        outcome: &'static str,
        error: Option<&Error>,
    ) {
        let anchor = Attempt {
            profile: profile.clone(),
            run: RunId(format!("private-{anchor}")),
            generation: Generation(0),
            claim: None,
        };
        let mut span = SpanRecord::start(profile, name, SpanIds::attempt(&anchor))
            .attr(key::BOT_PRIVATE, true)
            .attr(key::BOT_OUTCOME, outcome);
        if let Some(error) = error {
            span = span.fail(error_class(error));
        }
        span.end(self.parts.tracer.as_ref());
    }
}

/// 도구 결과가 `isError: true`인가. 읽지 못하면 성공으로 보지 않는다.
fn reports_error(payload: &str) -> bool {
    match crate::json::parse(payload) {
        Some(serde_json::Value::Object(map)) => {
            map.get("isError") != Some(&serde_json::Value::Bool(false))
        }
        _ => true,
    }
}

fn outcome_label(outcome: &PrivateOutcome) -> &'static str {
    match outcome {
        PrivateOutcome::Completed => "completed",
        PrivateOutcome::Held => "held",
        PrivateOutcome::NotSent => "not_sent",
        PrivateOutcome::RetryBeforeStart => "retry",
        PrivateOutcome::Rejected => "rejected",
    }
}

#[cfg(test)]
mod tests;

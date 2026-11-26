//! 관측: 호스트 `admin.trace`·`admin.logs`·`admin.issues` 수집기를 한 번 띄워 이 프로세스의 모든 역할이 함께 쓴다.
//! 알려진 자격 값은 내보내는 글·로그·이슈·관리 응답에서 가린다 — 시작 때 읽은 값뿐 아니라 이 프로세스가 비밀 보관소에서
//! 읽어 쓰는 모든 값이 쓰이기 전에 가림 장치에 들어가므로(`Watched`) 키를 교체해도 새 값이 바로 가려진다.
use super::watched::{Pace, Watched, declared, read_round};
use crate::{host::HostFile, logging, secrets::env::EnvStore};
use bot_admin::{
    Exporter, Fence, IssueExporter, LogExporter,
    issues::IssueOptions,
    trace::{Options, Redactor},
};
use bot_core::{
    issue::{IssueEvent, IssueLevel, IssueReporter},
    port::SecretStore,
    trace::{SpanRecord, Tracer, Val, key},
};
use bot_profile::Registry;
use std::{sync::Arc, time::Duration};

/// 정상 흐름·사용자 거절이라 이슈 트래커에 올리지 않을 오류 분류.
const BENIGN_SPAN_ERRORS: [&str; 10] = [
    "cancelled",
    "denied",
    "forbidden",
    "invalid",
    "lost",
    "no_route",
    "parked",
    "rate_limited",
    "too_large",
    "unregistered",
];

/// 스팬을 추적 수집기에 넘기면서, 비정상 실패 스팬은 이슈 트래커에도 자동 보고하는 래퍼.
struct ReportingTracer {
    inner: Arc<Exporter>,
    issues: Arc<IssueExporter>,
}

impl Tracer for ReportingTracer {
    fn record(&self, span: SpanRecord) {
        if let Some(error_class) = span.error
            && !BENIGN_SPAN_ERRORS.contains(&error_class)
        {
            let private = span
                .attrs
                .iter()
                .any(|(k, v)| *k == key::BOT_PRIVATE && *v == Val::Bool(true));
            let mut ev = IssueEvent::new(
                IssueLevel::Error,
                error_class,
                format!("{} failed ({error_class})", span.name),
                span.name.clone(),
            )
            .profile(span.profile.clone())
            .trace_id(span.ids.trace)
            .span_id(span.ids.span);
            ev.timestamp_nano = span.end_nano;
            if !private {
                for tag_key in [key::BOT_RUN_ID, key::BOT_AGENT, key::GEN_AI_TOOL_NAME] {
                    if let Some(val) = span.str(tag_key) {
                        ev = ev.tag(tag_key, val);
                    }
                }
            }
            self.issues.report(ev);
        }
        self.inner.record(span);
    }
}

/// 이 프로세스의 관측 입력
pub struct Observe {
    pub exporter: Arc<Exporter>,
    pub logs: Arc<LogExporter>,
    pub issues: Arc<IssueExporter>,
    /// 알려진 비밀 값을 담은 **살아 있는** 가림 장치(관리 응답 검사·추적·로그·이슈가 같은 목록을 쓴다)
    pub redactor: Redactor,
    /// 모든 역할이 써야 하는 비밀 보관소: 읽는 값을 `redactor`에 더하고 그대로 넘겨 준다.
    pub secrets: Arc<Watched>,
    /// 전역 로그·이슈 싱크 수명 가드(프로세스·시험 종료 시 자동 해제)
    _sinks: Option<logging::SinkGuard>,
}

impl Observe {
    /// 모든 역할에 같은 수집기를 넘기는 추적 입력(이슈 트래커가 켜져 있으면 실패 스팬을 자동 보고한다)
    pub fn tracer(&self) -> Arc<dyn Tracer> {
        if self.issues.enabled() {
            Arc::new(ReportingTracer {
                inner: self.exporter.clone(),
                issues: self.issues.clone(),
            })
        } else {
            self.exporter.clone()
        }
    }

    /// 활성화 판본 확인 문을 추적·로그·이슈 내보내기 모두에 단다.
    pub fn set_gate(&self, check: Fence, timeout: Duration) {
        self.exporter.set_gate(check.clone(), timeout);
        self.logs.set_gate(check.clone(), timeout);
        self.issues.set_gate(check, timeout);
    }

    /// 종료할 때 추적·로그·이슈의 남은 큐를 모두 비운다.
    pub async fn flush(&self) {
        let timeout = Duration::from_secs(5);
        tokio::join!(
            self.exporter.flush(timeout),
            self.logs.flush(timeout),
            self.issues.flush(timeout),
        );
    }
}

/// `bot doctor`가 모델 별칭 판정(`models::build`)에 넘기는 **자리만 채우는** 보관소.
pub(super) fn inert_secrets() -> Arc<Watched> {
    Arc::new(Watched::new(
        Arc::new(EnvStore::default()),
        Redactor::default(),
    ))
}

/// 수집기 설정(`admin.trace`·`admin.logs`·`admin.issues`)으로 내보내기 작업을 띄운다.
pub async fn start(
    host: &HostFile,
    registry: &Registry,
    secrets: Arc<dyn SecretStore>,
) -> Result<Observe, String> {
    let redactor = Redactor::default();
    let secrets = Arc::new(Watched::new(secrets, redactor.clone()));
    read_round(
        &secrets,
        &declared(host, registry),
        Pace::new(Duration::ZERO),
    )
    .await;
    let store = Some(secrets.clone() as Arc<dyn SecretStore>);
    let exporter = Exporter::start(
        host.admin.trace.clone(),
        store.clone(),
        redactor.clone(),
        Options::default(),
    )?;
    let logs = LogExporter::start(
        host.admin.logs.clone(),
        store.clone(),
        redactor.clone(),
        Options::default(),
    )?;
    let issues = IssueExporter::start(
        host.admin.issues.clone(),
        store,
        redactor.clone(),
        IssueOptions::default(),
    )?;
    let sinks = (logs.enabled() || issues.enabled())
        .then(|| logging::attach_sinks(Some(logs.clone()), Some(issues.clone())));
    Ok(Observe {
        exporter,
        logs,
        issues,
        redactor,
        secrets,
        _sinks: sinks,
    })
}

#[cfg(test)]
mod tests;

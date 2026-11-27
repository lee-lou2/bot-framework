//! 관리 API와 UI, OTLP 관측(`bot serve --role admin`).
//!
//! - 조회·판정 계약은 `bot_core::admin`이고 저장소가 구현한다. 이 크레이트는 DB를 모른다.
//! - 인증은 `auth`(Host 확인·실패 한도·상수 시간 비교·루프백 기본·CSRF)가 모든 길에 같은 순서로 건다.
//! - 승인은 `Broker::decide`, 보류 판정은 `AdminAct`로만 한다. 감사 행은 그쪽이 상태 변경과 같은 커밋에 쓴다.
//!
//! 수락 시험: shows_held_receipt, isolates_profile_traces.

pub mod api;
pub mod assets;
pub mod auth;
pub mod fence;
pub mod issues;
pub mod json;
pub mod logs;
pub mod map;
pub mod obs;
pub mod preapproval;
pub mod resp;
mod run_control;
pub mod settings;
pub mod trace;

#[cfg(any(test, feature = "testing"))]
pub mod fake;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::from_fn_with_state,
    routing::{get, post},
};
use bot_core::{
    admin::{AdminAct, AdminRead},
    id::ProfileId,
    port::Broker,
    profile::ProfileCatalog,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

pub use auth::{Authenticator, StaticTokens, hosts_for};
pub use fence::{Fence, FenceFuture};
pub use issues::IssueExporter;
pub use logs::LogExporter;
pub use resp::LeakCheck;
pub use settings::AdminSettings;
pub use trace::Exporter;

/// 호스트 활성화 지문(`base`)에 관측 설정(`admin.trace`·`admin.logs`·`admin.issues`)을 차례로 묶는다.
/// 각 절이 꺼져 있으면 해당 해시 단계를 건너뛰므로 기존 배포의 활성화 지문 바이트가 그대로 유지된다.
pub fn obs_fingerprint(base: [u8; 32], settings: &AdminSettings) -> [u8; 32] {
    let base = match trace::fingerprint(&settings.trace) {
        None => base,
        Some(fp) => {
            let mut hash = Sha256::new();
            hash.update(b"bot-authority-trace-v1\0");
            hash.update(base);
            hash.update(fp);
            hash.finalize().into()
        }
    };
    let base = match logs::fingerprint(&settings.logs) {
        None => base,
        Some(fp) => {
            let mut hash = Sha256::new();
            hash.update(b"bot-authority-logs-v1\0");
            hash.update(base);
            hash.update(fp);
            hash.finalize().into()
        }
    };
    match issues::fingerprint(&settings.issues) {
        None => base,
        Some(fp) => {
            let mut hash = Sha256::new();
            hash.update(b"bot-authority-issues-v1\0");
            hash.update(base);
            hash.update(fp);
            hash.finalize().into()
        }
    }
}

/// 디코딩한 JSON의 키·문자열에 알려진 비밀 값이 들어 있는가.
fn holds_secret(red: &trace::Redactor, v: &Value) -> bool {
    match v {
        Value::String(s) => red.has_value(s),
        Value::Array(a) => a.iter().any(|x| holds_secret(red, x)),
        Value::Object(o) => o
            .iter()
            .any(|(k, x)| red.has_value(k) || holds_secret(red, x)),
        _ => false,
    }
}

pub struct AdminConfig {
    /// 허용하는 `Host` 값(포트 포함). DNS 리바인딩을 막는다.
    pub hosts: Vec<String>,
    /// 실패 횟수 한도·집계 창·차단 시간(기본 5회·60초·60초).
    pub max_failures: usize,
    pub window: Duration,
    pub block: Duration,
    /// 응답에 비밀 값이 섞였는지 검사한다(참이면 통과). 조립하는 쪽이 알려진 비밀 값으로 만든다.
    pub leak_check: Option<LeakCheck>,
    /// 추적 내보내기(상태·추적 링크·관리 판정 스팬에 쓴다). 없으면 꺼진 것으로 나간다.
    pub traces: Option<Arc<Exporter>>,
    /// OTLP 로그 내보내기(상태 표시에 쓴다). 없으면 꺼진 것으로 나간다.
    pub logs: Option<Arc<LogExporter>>,
    /// 이슈 트래커 내보내기(상태 표시에 쓴다). 없으면 꺼진 것으로 나간다.
    pub issues: Option<Arc<IssueExporter>>,
    /// «같은 집합 확인» 훅과 기다리는 시간(`fence`). 없으면 확인하지 않는다.
    pub fence: Option<Fence>,
    pub fence_timeout: Duration,
}

impl AdminConfig {
    /// 바인드 주소와 루프백 이름만 허용하는 기본값.
    pub fn new(bind: SocketAddr) -> Self {
        Self {
            hosts: hosts_for(bind, &[]),
            max_failures: 5,
            window: Duration::from_secs(60),
            block: Duration::from_secs(60),
            leak_check: None,
            traces: None,
            logs: None,
            issues: None,
            fence: None,
            fence_timeout: fence::DEFAULT_TIMEOUT,
        }
    }

    /// 응답을 내보내기 직전에 «이 프로세스가 활성화한 프로필 집합이 저장소의 활성 판본과 같은가»를 확인하는 훅(참이면 통과).
    /// 거짓이거나 시간 안에 끝나지 않으면 `/v1/admin/*` 응답은 본문 없이 503 `unavailable`이다(옛 매핑으로 조회 결과를 내주지 않는다).
    /// 승인·보류 판정은 핸들러 전에도 확인한다. 정적 화면 자산은 확인하지 않는다.
    pub fn fence(mut self, check: impl Fn() -> FenceFuture + Send + Sync + 'static) -> Self {
        self.fence = Some(Arc::new(check));
        self
    }

    /// 훅이 응답하지 않을 때 기다리는 시간(기본 3초). 넘으면 «다르다»로 본다.
    pub fn fence_timeout(mut self, timeout: Duration) -> Self {
        self.fence_timeout = timeout;
        self
    }

    /// 알려진 비밀 값이 응답에 섞이면 내보내지 않는다. 직렬화한 글이 아니라 디코딩한 JSON의 키와 문자열 원값을 본다
    /// (직렬화하면 따옴표·역슬래시·줄바꿈이 이스케이프돼 그런 글자가 든 자격 값을 놓친다). 모양은 보지 않아 해시·판본은 통과한다.
    pub fn leak_values(self, values: impl IntoIterator<Item = String>) -> Self {
        self.leak_redactor(trace::Redactor::new(values))
    }

    /// `leak_values`와 같되 **살아 있는 목록**을 쓴다: 호스트가 이 가림 장치에 값을 더하면(키 교체 뒤 새 값을 쓰기 전에) 다음 응답부터 그 값도 검사한다.
    pub fn leak_redactor(mut self, red: trace::Redactor) -> Self {
        self.leak_check = Some(Arc::new(move |v| !holds_secret(&red, v)));
        self
    }

    pub fn traces(mut self, exporter: Arc<Exporter>) -> Self {
        self.traces = Some(exporter);
        self
    }

    pub fn logs(mut self, exporter: Arc<LogExporter>) -> Self {
        self.logs = Some(exporter);
        self
    }

    pub fn issues(mut self, exporter: Arc<IssueExporter>) -> Self {
        self.issues = Some(exporter);
        self
    }

    /// 프록시 뒤에서 쓰는 외부 호스트 이름(포트 포함)을 더한다.
    pub fn host(mut self, host: &str) -> Self {
        self.hosts.push(host.trim().to_ascii_lowercase());
        self
    }
}

/// 조립하는 쪽(bot-cli)이 넘기는 입력.
pub struct AdminDeps {
    pub read: Arc<dyn AdminRead>,
    pub act: Arc<dyn AdminAct>,
    pub broker: Arc<dyn Broker>,
    pub auth: Arc<dyn Authenticator>,
    /// 활성 프로필 스냅숏(구조 지도의 원천).
    pub profiles: Arc<dyn ProfileCatalog>,
}

/// 선택 입력(추적·로그·이슈 내보내기). 없으면 해당 응답이 꺼진 것으로 나간다.
#[derive(Default)]
pub struct Extra {
    pub(crate) exporter: Option<Arc<Exporter>>,
    pub(crate) logs: Option<Arc<LogExporter>>,
    pub(crate) issues: Option<Arc<IssueExporter>>,
}

impl Extra {
    pub(crate) fn obs(&self, profile: &ProfileId) -> Value {
        match self.exporter.as_ref().and_then(|e| e.stats(profile)) {
            Some(stats) => stats.json(),
            None => serde_json::json!({"enabled": false}),
        }
    }

    pub(crate) fn logs_obs(&self) -> Value {
        match &self.logs {
            Some(exporter) if exporter.enabled() => exporter.stats().json(),
            _ => serde_json::json!({"enabled": false}),
        }
    }

    pub(crate) fn issues_obs(&self) -> Value {
        match &self.issues {
            Some(exporter) if exporter.enabled() => exporter.stats().json(),
            _ => serde_json::json!({"enabled": false}),
        }
    }

    /// 이 프로필의 이 추적을 여는 링크(틀이 설정된 경우만).
    fn link(&self, profile: &str, trace_hex: &str) -> Option<String> {
        let bytes: Vec<u8> = (0..trace_hex.len() / 2)
            .map_while(|i| u8::from_str_radix(trace_hex.get(i * 2..i * 2 + 2)?, 16).ok())
            .collect();
        let trace: [u8; 16] = bytes.try_into().ok()?;
        self.exporter
            .as_ref()?
            .link(&ProfileId(profile.to_owned()), &trace)
    }

    /// 응답 안의 `profile`·`trace_id`를 가진 객체마다 `trace_link`를 붙인다.
    pub(crate) fn decorate(&self, v: &mut Value) {
        match v {
            Value::Object(o) => {
                let link = match (o.get("profile"), o.get("trace_id")) {
                    (Some(Value::String(p)), Some(Value::String(t))) => self.link(p, t),
                    _ => None,
                };
                if let Some(link) = link {
                    o.insert("trace_link".into(), Value::String(link));
                }
                o.values_mut().for_each(|x| self.decorate(x));
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.decorate(x)),
            _ => {}
        }
    }

    /// 운영 판정을 그 run 시도의 추적에 한 스팬으로 남긴다(승인 nonce·인자는 싣지 않는다).
    pub(crate) fn decision(
        &self,
        at: &bot_core::id::Attempt,
        tag: &str,
        action: &'static str,
        verdict: &'static str,
        risk: Option<&'static str>,
    ) {
        use trace::{SpanIds, SpanRecord, key};
        let Some(e) = &self.exporter else { return };
        // 같은 호출을 여러 번 판정해도 스팬 ID가 겹치지 않게 동작·시각을 섞는다(승인 nonce는 쓰지 않는다).
        let ids =
            SpanIds::attempt(at).child("admin", &format!("{action}:{tag}:{}", trace::now_nano()));
        let mut span = SpanRecord::start(&at.profile, format!("admin {action}"), ids)
            .attr(key::BOT_ADMIN_ACTION, action)
            .attr(key::BOT_ADMIN_VERDICT, verdict)
            .attr(key::BOT_RUN_ID, at.run.0.clone());
        if let Some(r) = risk {
            span = span.attr(key::BOT_APPROVAL_RISK, r);
        }
        span.end(e.as_ref());
    }
}

/// 관리 라우트. 받아 줄 신원이 없으면 `None`이다(닫힌 채 시작).
pub fn router(cfg: AdminConfig, deps: AdminDeps) -> Option<Router> {
    if !deps.auth.enabled() {
        return None;
    }
    let guard = auth::Guard::new(
        deps.auth,
        &cfg.hosts,
        cfg.max_failures,
        cfg.window,
        cfg.block,
    );
    let st = Arc::new(api::St {
        read: deps.read,
        act: deps.act,
        broker: deps.broker,
        profiles: deps.profiles,
        guard: guard.clone(),
        leak: cfg.leak_check,
        started: Instant::now(),
        extra: Extra {
            exporter: cfg.traces,
            logs: cfg.logs,
            issues: cfg.issues,
        },
    });
    let gate = cfg.fence.map(|check| {
        Arc::new(fence::Gate {
            check,
            timeout: cfg.fence_timeout,
        })
    });
    let app = Router::new()
        .route("/v1/admin/me", get(api::me))
        .route("/v1/admin/runs", get(api::runs))
        .route("/v1/admin/runs/{profile}/{run}", get(api::run_detail))
        .route(
            "/v1/admin/runs/{profile}/{run}/resume",
            post(run_control::resume),
        )
        .route(
            "/v1/admin/runs/{profile}/{run}/cancel",
            post(run_control::cancel),
        )
        .route("/v1/admin/approvals", get(api::approvals))
        .route("/v1/admin/approvals/{nonce}/decide", post(api::decide))
        .route("/v1/admin/holds", get(api::holds))
        .route(
            "/v1/admin/holds/{profile}/{run}/{call}/resolve",
            post(api::resolve),
        )
        .route("/v1/admin/preapprovals", get(preapproval::list))
        .route(
            "/v1/admin/preapprovals/{profile}/{agent}/{trigger}/pause",
            post(preapproval::pause),
        )
        .route(
            "/v1/admin/preapprovals/{profile}/{agent}/{trigger}/resume",
            post(preapproval::resume),
        )
        .route("/v1/admin/audit", get(api::audit))
        .route("/v1/admin/audit/verify", get(api::audit_verify))
        .route("/v1/admin/status", get(api::status))
        .route("/v1/admin/map", get(api::map))
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/ui") }),
        )
        .route("/ui", get(assets::index))
        .route("/ui/", get(assets::index))
        .route("/ui/{*path}", get(assets::asset))
        .fallback(api::not_found)
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(st);
    // 인증 안쪽에 둔다: 인증을 통과한 요청만 «같은 집합 확인»을 거친다.
    let app = match gate {
        Some(g) => app.layer(from_fn_with_state(g, fence::layer)),
        None => app,
    };
    Some(app.layer(from_fn_with_state(guard, auth::guard)))
}

/// 루프백이 아닌 주소에서 관리 화면을 열려면 운영자가 명시해야 한다.
pub fn check_bind(addr: SocketAddr, remote_allowed: bool) -> Result<(), String> {
    if addr.ip().is_loopback() || remote_allowed {
        Ok(())
    } else {
        Err("루프백이 아닌 주소에서 관리 화면을 열려면 원격 허용을 명시해야 한다".to_owned())
    }
}

/// 주소를 검사하고 연다. 실제 주소는 `local_addr()`로 알 수 있다(포트 0 지원).
pub async fn bind(
    addr: SocketAddr,
    remote_allowed: bool,
) -> std::io::Result<tokio::net::TcpListener> {
    check_bind(addr, remote_allowed)
        .map_err(|m| std::io::Error::new(std::io::ErrorKind::InvalidInput, m))?;
    tokio::net::TcpListener::bind(addr).await
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // 접속 출처(피어 주소)를 요청에 실어 인증 실패 한도가 출처마다 따로 세게 한다.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_binding_needs_an_explicit_flag() {
        let local: SocketAddr = "127.0.0.1:8080".parse().expect("주소");
        let v6: SocketAddr = "[::1]:8080".parse().expect("주소");
        let any: SocketAddr = "0.0.0.0:8080".parse().expect("주소");
        assert!(check_bind(local, false).is_ok() && check_bind(v6, false).is_ok());
        assert!(check_bind(any, false).is_err());
        assert!(check_bind(any, true).is_ok());
    }
}

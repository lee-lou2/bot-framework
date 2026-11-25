//! 관측: 호스트 `admin.trace`에 설정한 수집기로 보내는 `Exporter` 하나를 이 프로세스의 모든 역할(gateway·admin·broker…)이
//! 함께 쓴다. 단독 gateway도 설정된 수집기를 쓴다. 알려진 자격 값은 내보내는 글과 관리 응답에서 가린다 — 시작 때 읽은 값뿐 아니라 이 프로세스가 비밀 보관소에서
//! 읽어 쓰는 모든 값이 쓰이기 전에 가림 장치에 들어가므로(`Watched`) 키를 교체해도 새 값이 바로 가려진다. 스스로 읽지 않는 값은 주기적으로 다시 읽어
//! 따라잡는다(`watched::reread`).
use super::watched::{Pace, Watched, declared, read_round};
use crate::{host::HostFile, secrets::env::EnvStore};
use bot_admin::{
    Exporter,
    trace::{Options, Redactor},
};
use bot_core::{port::SecretStore, trace::Tracer};
use bot_profile::Registry;
use std::{sync::Arc, time::Duration};

/// 이 프로세스의 관측 입력
pub struct Observe {
    pub exporter: Arc<Exporter>,
    /// 알려진 비밀 값을 담은 **살아 있는** 가림 장치(관리 응답 검사·내보내는 글이 같은 목록을 쓴다)
    pub redactor: Redactor,
    /// 모든 역할이 써야 하는 비밀 보관소: 읽는 값을 `redactor`에 더하고 그대로 넘겨 준다. 역할의 입력은 이 타입(`Arc<Watched>`)만 받는다 —
    /// 날 보관소(`Arc<dyn SecretStore>`)를 넘기면 컴파일이 되지 않는다.
    pub secrets: Arc<Watched>,
}

impl Observe {
    /// 모든 역할에 같은 수집기를 넘기는 추적 입력
    pub fn tracer(&self) -> Arc<dyn Tracer> {
        self.exporter.clone()
    }

    /// 종료할 때 남은 큐를 비운다.
    pub async fn flush(&self) {
        self.exporter.flush(Duration::from_secs(5)).await;
    }
}

/// `bot doctor`가 모델 별칭 판정(`models::build`)에 넘기는 **자리만 채우는** 보관소: 비어 있는 개발용 보관소를 가림 장치로 감싼 것이라 아무 값도
/// 읽지 않고 어떤 역할에도 넘기지 않는다. 보관소를 감싸는 곳을 이 파일 하나로 두는 규칙(`watched` 구조 시험)을 지키려고 여기에 둔다.
pub(super) fn inert_secrets() -> Arc<Watched> {
    Arc::new(Watched::new(
        Arc::new(EnvStore::default()),
        Redactor::default(),
    ))
}

/// 수집기 설정(`admin.trace`)으로 내보내기 작업을 띄운다. 설정이 없으면 아무것도 내보내지 않는다. 잘못된 설정이면 Err(시작 전).
/// 이 프로세스가 아는 프로필의 자격 중 읽을 수 있는 것은 미리 읽어 가림 목록에 넣어 둔다 — 없는 자격은 건너뛴다(관측은 모든 자격이 있어야 시작하는 역할이 아니다).
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
    let exporter = Exporter::start(
        host.admin.trace.clone(),
        Some(secrets.clone() as Arc<dyn SecretStore>),
        redactor.clone(),
        Options::default(),
    )?;
    Ok(Observe {
        exporter,
        redactor,
        secrets,
    })
}

#[cfg(test)]
mod tests;

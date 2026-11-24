//! 이 프로세스가 비밀 보관소에서 **읽어 쓰는 모든 값**을 쓰기 전에 가림 장치(`Redactor`)에 더하는 보관소 래퍼, 그리고 선언된 비밀 이름 전체를 시작 뒤에도
//! 주기적으로 다시 읽는 작업.
//!
//! 시작 때 한 번 읽은 목록만으로는 키를 교체한 뒤의 새 값이 내보내는 글·관리 응답에서 가려지지 않는다. 값을 읽는 길(모델 서비스 키·도구 자격·수집기 헤더)이
//! 모두 이 래퍼를 거치므로 새 값은 쓰이기 **전에** 목록에 들어가고, 이미 목록에 있는 값(교체 전 값 포함)은 이 프로세스가 사는 동안 계속 가려진다.
//! 그래도 이 프로세스가 **스스로 읽지 않는 값**은 모른다 — 역할을 나눠 띄우면 관리 프로세스는 모델 키·도구 자격을 읽는 쪽이 아니라서, 다른 프로세스가 읽는
//! 교체된 새 값이 DB로 새어 들어와도 관리 응답 검사가 알아보지 못한다. 그래서 주기적으로(`BOT_SECRETS_REREAD_SECS`) 선언된 이름 전체를 다시 읽는다.
//!
//! 이 래퍼를 만드는 곳은 `trace::start` 하나다. 모든 역할의 입력은 그것이 만든 `Arc<Watched>`만 받으므로(`Arc<dyn SecretStore>`를 받는 입력은 없다) 가림 장치를
//! 거치지 않는 보관소가 역할에 닿을 길이 없다.
use crate::{host::HostFile, validate::needed_secrets};
use bot_admin::trace::Redactor;
use bot_core::{
    id::{ProfileId, SecretName},
    port::{ProfileSecrets, Reply, SecretStore, SecretValue},
    profile::ProfileCatalog,
};
use bot_profile::Registry;
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

pub struct Watched {
    inner: Arc<dyn SecretStore>,
    redactor: Redactor,
}

impl Watched {
    /// 만드는 곳은 `trace::start` 하나다(`serve/watched/tests.rs`가 소스를 훑어 지킨다).
    pub(super) fn new(inner: Arc<dyn SecretStore>, redactor: Redactor) -> Self {
        Self { inner, redactor }
    }

    /// 시험용: 가림 목록을 따로 보지 않는 역할 시험이 날 보관소 대신 넘긴다.
    #[cfg(test)]
    pub(super) fn plain(inner: Arc<dyn SecretStore>) -> Arc<Self> {
        Arc::new(Self::new(inner, Redactor::default()))
    }
}

impl SecretStore for Watched {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        Box::pin(async move {
            let inner = self.inner.for_profile(profile).await?;
            Ok(Box::new(WatchedProfile {
                inner,
                redactor: self.redactor.clone(),
            }) as Box<dyn ProfileSecrets>)
        })
    }
}

struct WatchedProfile {
    inner: Box<dyn ProfileSecrets>,
    redactor: Redactor,
}

impl ProfileSecrets for WatchedProfile {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        Box::pin(async move {
            let value = self.inner.resolve(name).await?;
            // 값이 호출한 쪽으로 넘어가기 전에 더한다. 글이 아닌 값(바이트)은 글에 섞여 나갈 수 없으니 건너뛴다.
            if let Ok(text) = std::str::from_utf8(&value.0) {
                add_forms(&self.redactor, text);
            }
            Ok(value)
        })
    }
}

/// 읽은 값을 가림 목록에 더한다: 읽은 그대로와, 앞뒤 공백·줄바꿈을 뗀 모양. 보관소 파일 끝의 줄바꿈 때문에 «값\n»만 목록에 있으면 그 값을 떼어 쓰는 쪽
/// (게이트웨이의 `slack/creds.rs::fetch`가 `trim()`한다)이 내보내는 글의 «값»은 가려지지 않는다. 공백뿐인 값은 더하지 않는다(어떤 글의 공백이든 가리게 된다).
fn add_forms(redactor: &Redactor, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    redactor.add(text);
    redactor.add(trimmed);
}

/// 프로필별로 읽을 비밀 이름
pub(super) type Plan = Vec<(ProfileId, Vec<SecretName>)>;

/// 이 프로세스가 아는 프로필(활성 집합)이 선언한 비밀 이름 전체: 입구(Slack·HTTP)·모델 서비스·도구 자격. 이름이 하나도 없는 프로필은 뺀다.
pub(super) fn declared(host: &HostFile, registry: &Registry) -> Plan {
    registry
        .active()
        .iter()
        .map(|(profile, snapshot)| {
            let names = needed_secrets(snapshot, host)
                .iter()
                .filter_map(|name| SecretName::parse(name))
                .collect::<Vec<_>>();
            (profile.clone(), names)
        })
        .filter(|(_, names)| !names.is_empty())
        .collect()
}

/// 다시 읽기의 속도 제한. 키체인은 읽을 때마다 `security`를, SOPS는 기억(30초)이 끝났을 때 `sops`를 부르므로 몰아 치지 않는다.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pace {
    /// 한 바퀴가 끝난 뒤 다음 바퀴를 시작하기까지의 시간(끝에서 시작까지라 바퀴가 겹치지 않는다)
    pub every: Duration,
    /// 프로필 핸들이나 값 하나를 읽는 데 줄 수 있는 시간. 잠긴 키체인·응답 없는 보관소가 바퀴를 붙들지 못하게 한다.
    pub read: Duration,
    /// 읽기 사이의 쉼
    pub gap: Duration,
}

impl Pace {
    /// `every`는 `reread`만 쓴다(시작 때 한 번 읽는 `read_round`는 쓰지 않는다).
    pub(super) fn new(every: Duration) -> Self {
        Self {
            every,
            read: Duration::from_secs(5),
            gap: Duration::from_millis(25),
        }
    }
}

/// 한 바퀴의 결과. 센 것만 담는다(값도 이름도 없다).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Round {
    /// 읽은 값(읽는 순간 가림 목록에 들어갔다)
    pub read: usize,
    /// 보관소에 없거나 읽지 못한 값
    pub failed: usize,
    /// 줄 수 있는 시간을 넘겨 포기한 읽기
    pub timed_out: usize,
}

/// 선언된 이름을 한 바퀴 읽는다. 읽는 보관소가 값을 가림 목록에 더하므로 여기서는 값을 쥐지도 기록하지도 않는다(읽자마자 버린다).
/// 없는 자격·읽지 못한 값은 건너뛰고, 줄 수 있는 시간을 넘긴 읽기는 포기해 다음 이름으로 간다.
pub(super) async fn read_round(secrets: &Watched, plan: &Plan, pace: Pace) -> Round {
    let mut round = Round::default();
    for (profile, names) in plan {
        let handle = match timeout(pace.read, secrets.for_profile(profile.clone())).await {
            Ok(Ok(handle)) => handle,
            Ok(Err(_)) => {
                round.failed += names.len();
                continue;
            }
            Err(_) => {
                round.timed_out += names.len();
                continue;
            }
        };
        for name in names {
            match timeout(pace.read, handle.resolve(name.clone())).await {
                Ok(Ok(_)) => round.read += 1,
                Ok(Err(_)) => round.failed += 1,
                Err(_) => round.timed_out += 1,
            }
            if !pace.gap.is_zero() {
                sleep(pace.gap).await;
            }
        }
    }
    round
}

/// 다음 바퀴까지의 대기. 시간을 넘긴 읽기가 있던 바퀴 뒤에는 두 배씩 늘리고(최대 `every`의 10배), 없는 바퀴가 오면 원래 주기로 돌아온다 —
/// 잠긴 키체인이나 응답 없는 보관소를 주기마다 두드리지 않는다.
pub(super) fn next_wait(every: Duration, current: Duration, round: &Round) -> Duration {
    if round.timed_out > 0 {
        current.saturating_mul(2).min(every.saturating_mul(10))
    } else {
        every
    }
}

/// 시작 뒤에도 선언된 비밀 이름 전체를 주기적으로 다시 읽는다(`stop`이 오면 끝난다). 시작 때 읽은 값 이후에 교체된 새 값이 가림 목록에 들어간다.
pub(super) async fn reread(secrets: Arc<Watched>, plan: Plan, pace: Pace, stop: CancellationToken) {
    if plan.is_empty() {
        return;
    }
    let mut wait = pace.every;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = sleep(wait) => {}
        }
        let round = tokio::select! {
            () = stop.cancelled() => return,
            round = read_round(&secrets, &plan, pace) => round,
        };
        wait = next_wait(pace.every, wait, &round);
        // 개수만 남긴다(이름도 값도 싣지 않는다)
        tracing::debug!(
            read = round.read,
            failed = round.failed,
            timed_out = round.timed_out,
            "선언된 비밀을 다시 읽었어요"
        );
    }
}

#[cfg(test)]
mod tests;

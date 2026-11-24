//! 모델 샌드박스를 얻는 방법: 이미 열린 것(시험의 합성 샌드박스) 또는 Apple VM 어댑터(운영).
//!
//! VM은 순서가 정해져 있다 — 타입이 강제한다(`ContainerSandbox::start` → `Anchored` → 프런트를 그 주소에 실제로 연다 → `relay`):
//! 닻 VM이 호스트 전용 망의 게이트웨이 주소를 지키는 동안에만 그 주소에 바인드할 수 있으므로 프런트는 **닻을 먼저 띄운 뒤** 연다.
//! 릴레이 주소는 프런트가 실제로 연 값(`local_addr`, 포트 0이면 OS가 정한 포트)이 단일 원본이고 어댑터가 init(`BOT_RELAY_IP`·`BOT_RELAY_PORT`)과
//! Claude(`ANTHROPIC_BASE_URL`)에 같은 값을 준다. 0.0.0.0·loopback에는 열지 않는다.
use crate::{
    execution::Apple,
    host::HostFile,
    install,
    serve::{runner::default_work, upstream::HttpsUpstream},
};
use bot_broker::model::Upstream;
use bot_core::sandbox::ModelSandbox;
use bot_runtime::model::container::{Anchored, ContainerConfig, ContainerSandbox, Entry, Health};
use std::{
    fs::DirBuilder,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// 모델 샌드박스를 얻는 방법
#[derive(Clone)]
pub enum Launch {
    /// 이미 열린 샌드박스와 프런트가 듣는 주소(시험의 합성 샌드박스)
    Ready {
        sandbox: Arc<ModelSandbox>,
        bind: SocketAddr,
        /// 샌드박스가 CLI의 파일 읽기를 마운트 안으로 가둔다고 보증하는가
        confined_reads: bool,
    },
    /// Apple VM(운영): 닻을 먼저 띄우고 그 게이트웨이 주소에 프런트를 연 뒤 릴레이를 알려 샌드박스를 얻는다
    Container {
        config: Box<ContainerConfig>,
        /// 호스트 설정의 `sandbox.apple.relay_bind`. 생략하면 닻이 확인한 게이트웨이 IP에서 OS가 빈 포트를 정한다
        relay_bind: Option<SocketAddr>,
    },
}

/// 운영 조각(호스트 설정에서): Apple VM 어댑터와 HTTPS 상류 전송. `sandbox.apple`이 없으면 Err — 다른 샌드박스로 대신하지 않는다.
pub fn operational(
    host: &HostFile,
    installs: Option<&Path>,
    uid: u32,
) -> Result<super::ClaudeParts, String> {
    let apple = host.sandbox.apple.as_ref().ok_or(
        "claude-cli 별칭이 있는데 호스트 설정에 sandbox.apple(다이제스트로 고정한 이미지와 호스트 전용 망)이 없어요 — docs/model-image.md",
    )?;
    let workspace = match &apple.work_root {
        Some(path) => path.clone(),
        None => {
            let base = install::default_base(installs, std::env::var("HOME").ok().as_deref())?;
            default_work(&base).join("models")
        }
    };
    private_dir(&workspace, uid)?;
    Ok(super::ClaudeParts {
        launch: Launch::Container {
            config: Box::new(container_config(apple, &workspace)),
            relay_bind: apple.relay_bind,
        },
        upstream: Arc::new(HttpsUpstream::new()?) as Arc<dyn Upstream>,
        workspace,
        park: None,
    })
}

/// 호스트 설정 → 어댑터 설정(운영은 이미지의 신뢰 init이 있어야 한다: 방화벽 없이 시작하는 길은 없다)
fn container_config(apple: &Apple, workspace: &Path) -> ContainerConfig {
    let mut config = ContainerConfig::new(&apple.image, &apple.network, workspace)
        .with_entry(Entry::model_image());
    if let Some(binary) = apple.binary.as_ref().and_then(|path| path.to_str()) {
        config.program = binary.to_owned();
    }
    match &apple.instance {
        Some(name) => config.named(name),
        None => config,
    }
}

/// 호출별 입력(대화 본문이 들어간다)·작업 폴더가 생기는 곳: 본인 소유이고 소유자만 접근한다(0700). 없으면 만든다.
fn private_dir(path: &Path, uid: u32) -> Result<(), String> {
    let shown = path.display();
    if std::fs::symlink_metadata(path).is_err() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("모델 작업 폴더를 만들지 못했어요({shown}): {e}"))?;
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("모델 작업 폴더를 읽지 못했어요({shown}): {e}"))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{shown}: 모델 작업 폴더는 본인 소유이고 소유자만 접근할 수 있는(0700) 폴더여야 해요"
        ));
    }
    Ok(())
}

/// 프런트를 열 주소: 설정이 없으면 닻이 확인한 게이트웨이 IP의 빈 포트(0), 있으면 그 IP가 게이트웨이 IP와 **정확히 같을 때만** 그 주소다.
pub(super) fn relay_target(
    gateway: Ipv4Addr,
    relay_bind: Option<SocketAddr>,
) -> Result<SocketAddr, String> {
    match relay_bind {
        None => Ok(SocketAddr::new(IpAddr::V4(gateway), 0)),
        Some(bind) if bind.ip() == IpAddr::V4(gateway) => Ok(bind),
        Some(bind) => Err(format!(
            "sandbox.apple.relay_bind({bind})가 닻이 확인한 게이트웨이 주소({gateway})와 달라요 — 생략하거나 그 주소를 적어요"
        )),
    }
}

/// 운영 VM 어댑터와 «지금 끝내는 중» 표지: 닻이 내려갔다는 알림을 의도한 종료와 구별한다.
pub(super) struct Vm {
    sandbox: Arc<ContainerSandbox>,
    stopping: Arc<AtomicBool>,
}

impl Vm {
    /// 닻과 이번 실행의 VM을 치우고 인스턴스 소유권을 놓는다.
    pub(super) async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.sandbox.shutdown().await;
    }
}

/// 프런트가 열릴 주소를 정한 상태: 아직 릴레이를 알리지 않았다
pub(super) enum Pending {
    Ready(Arc<ModelSandbox>),
    Anchored(Anchored),
}

impl Launch {
    /// 프런트가 들을 주소와 `confined_reads`를 정한다. VM이면 여기서 닻을 띄운다 — 실패하면 아무것도 남지 않는다.
    pub(super) async fn prepare(self) -> Result<(SocketAddr, bool, Pending), String> {
        match self {
            Self::Ready {
                sandbox,
                bind,
                confined_reads,
            } => Ok((bind, confined_reads, Pending::Ready(sandbox))),
            Self::Container { config, relay_bind } => {
                let anchored = ContainerSandbox::start(*config).await.map_err(|e| {
                    format!(
                        "모델 VM을 시작하지 못했어요({}) — container 서비스·이미지·망·인스턴스 이름을 확인하세요(docs/model-image.md)",
                        e.0
                    )
                })?;
                match relay_target(anchored.address(), relay_bind) {
                    // VM 어댑터는 CLI의 파일 읽기를 마운트 안으로 가둔다
                    Ok(bind) => Ok((bind, true, Pending::Anchored(anchored))),
                    Err(error) => {
                        anchored.abort().await;
                        Err(error)
                    }
                }
            }
        }
    }
}

impl Pending {
    /// 프런트를 열지 못했다: 닻과 이번 실행의 VM을 치운다.
    pub(super) async fn abort(self) {
        if let Self::Anchored(anchored) = self {
            anchored.abort().await;
        }
    }

    /// 프런트가 실제로 연 포트를 알려 실행 가능한 샌드박스를 얻는다(어댑터가 받고 있는지 확인한다). 실패하면 닻을 치우고 Err.
    pub(super) async fn open(self, port: u16) -> Result<(Arc<ModelSandbox>, Option<Vm>), String> {
        match self {
            Self::Ready(sandbox) => Ok((sandbox, None)),
            Self::Anchored(anchored) => {
                let vm = anchored.relay(port).await.map_err(|e| {
                    format!("모델 VM이 릴레이({port})를 확인하지 못했어요({})", e.0)
                })?;
                let vm = Arc::new(vm);
                let stopping = Arc::new(AtomicBool::new(false));
                if let Some(mut health) = vm.health() {
                    // 닻이 내려가면 새 모델 작업은 어댑터가 거절한다. 운영자가 알 수 있게 오르내림만 한 줄로 남긴다(의도한 종료는 빼고).
                    let quiet = stopping.clone();
                    tokio::spawn(async move {
                        while health.changed().await.is_ok() {
                            if quiet.load(Ordering::SeqCst) {
                                continue;
                            }
                            match *health.borrow() {
                                Health::Up { generation } => {
                                    eprintln!("모델 VM 닻: 올랐어요(세대 {generation})");
                                }
                                Health::Down => eprintln!(
                                    "모델 VM 닻: 내려갔어요 — 다시 뜰 때까지 새 모델 작업은 거절돼요"
                                ),
                            }
                        }
                    });
                }
                Ok((
                    vm.clone() as Arc<ModelSandbox>,
                    Some(Vm {
                        sandbox: vm,
                        stopping,
                    }),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests;

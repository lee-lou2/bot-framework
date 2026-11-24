//! 프런트가 바인드할 주소를 보증하는 닻 VM과 인스턴스 소유권.
//!
//! 호스트 전용 망의 게이트웨이 IP는 그 망에 붙은 VM이 있을 때만 호스트에 생긴다(실측). 프런트는 그 IP에만 바인드해야 하므로(0.0.0.0·loopback 금지),
//! 관리 라벨을 단 최소 VM(아무것도 하지 않고 기다리는 프로세스) 하나를 어댑터 수명 동안 둔다. 주소는 설정이 아니라 망에서 확인한 값이다.
//! - 닻이 죽으면 감시 작업이 다시 띄운다. 못 띄우는 동안(`Health::Down`)은 모델 VM을 띄우지 않는다(닫힌 채 실패). 한 번 바인드한 소켓은 닻이 다시 떠도
//!   그대로 쓸 수 있다(실측: 인터페이스가 사라졌다 돌아와도 같은 주소의 리스너가 다시 닿는다).
//! - 소유: 인스턴스별 잠금(`lock.rs`, 호스트 사용자 전용 고정 경로)을 닻이 사는 동안 쥔다. 같은 인스턴스 이름으로 어댑터를 둘 띄워 서로의 닻·VM을 정리하는 일을
//!   시작 때 거절한다(프로세스가 죽으면 OS가 잠금을 푼다). 그래서 이 인스턴스의 다른 실행(incarnation)이 남긴 VM은 죽은 프로세스의 고아다.
//! - 종료(`Anchor::shutdown`)는 감시를 멈추고 이번 실행이 띄운 VM을 모두 치운다. 핸들을 그냥 놓아도 감시가 멈추며 닻을 끝낸다.

use super::{ContainerProcess, Filter, Inner, StartError, args, lock::InstanceLock};
use bot_core::sandbox::ModelProcess;
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

/// 다시 띄우기를 기다리는 처음·최대 간격(실패할수록 두 배)
const RELAUNCH_FIRST: Duration = Duration::from_millis(500);
const RELAUNCH_MAX: Duration = Duration::from_secs(10);
/// 종료 때 감시 작업을 기다리는 시간
const STOP_WITHIN: Duration = Duration::from_secs(20);
/// 호스트에 게이트웨이 주소가 이만큼 연달아 없으면 닻을 다시 띄운다.
const ADDRESS_MISSES: u32 = 3;

/// 닻의 상태. `Up`의 세대는 다시 띄울 때마다 오른다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Health {
    Up { generation: u64 },
    Down,
}

/// 망이 알려 준 게이트웨이가 프런트가 바인드해도 되는 주소인가: 사설 IPv4(10/8·172.16/12·192.168/16) 하나여야 한다.
/// 0.0.0.0·loopback·링크 로컬·다중 전송·브로드캐스트·공인 주소는 모두 사설이 아니라서 여기서 걸린다.
pub(super) fn gateway_ip(text: &str) -> Result<Ipv4Addr, StartError> {
    let ip: Ipv4Addr = text.parse().map_err(|_| StartError("gateway_invalid"))?;
    if !ip.is_private() {
        return Err(StartError("gateway_invalid"));
    }
    Ok(ip)
}

/// 닻 VM을 띄우고 게이트웨이 주소가 호스트에 생길 때까지 기다린다. 그 사이 VM이 끝나거나 시간이 지나면 아무것도 남기지 않고 실패한다.
async fn launch(inner: &Inner, address: Ipv4Addr) -> Result<ContainerProcess, StartError> {
    let mut process = inner
        .run_vm(args::Role::Anchor, None)
        .await
        .map_err(|_| StartError("anchor_spawn_failed"))?;
    let probe = inner.hooks.host_has.clone();
    let ready = async {
        while !probe(address) {
            sleep(Duration::from_millis(100)).await;
        }
    };
    let outcome = tokio::select! {
        () = ready => Ok(()),
        _ = process.wait() => Err(StartError("anchor_exited")),
        () = sleep(inner.hooks.ready_within) => Err(StartError("anchor_not_ready")),
    };
    match outcome {
        Ok(()) => Ok(process),
        Err(error) => {
            process.kill().await;
            Err(error)
        }
    }
}

/// 호스트의 게이트웨이 주소가 연달아 사라져 있으면 끝난다. 닻 프로세스가 살아 있어도(예: 같은 기계의 다른 VM이 기본 망을 오르내리며 vmnet이 호스트 전용 망의
/// 호스트 쪽 인터페이스를 다시 만들었다) 그 주소가 없으면 VM이 프런트에 닿지 못한다: 닻을 다시 띄워 주소를 되살린다.
async fn address_lost(inner: &Inner, address: Ipv4Addr) {
    let mut missing = 0;
    loop {
        sleep(inner.hooks.address_poll).await;
        if (inner.hooks.host_has)(address) {
            missing = 0;
        } else {
            missing += 1;
            if missing >= ADDRESS_MISSES {
                return;
            }
        }
    }
}

/// 멈추라는 신호(또는 핸들이 사라짐)가 올 때까지 기다린다.
async fn stopped(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() || rx.changed().await.is_err() {
            return;
        }
    }
}

/// 닻을 지켜본다: 끝나면 `Down`으로 알리고 다시 띄운다(실패하면 간격을 늘려 계속). 멈추라는 신호가 오면 닻을 끝내고 `Down`으로 닫는다.
async fn supervise(
    inner: Arc<Inner>,
    address: Ipv4Addr,
    first: ContainerProcess,
    health: watch::Sender<Health>,
    mut stop: watch::Receiver<bool>,
) {
    let owner = args::anchor_owner(&inner.config);
    let mut process = first;
    let mut generation = 1u64;
    loop {
        tokio::select! {
            _ = process.wait() => {}
            () = address_lost(&inner, address) => process.kill().await,
            () = stopped(&mut stop) => {
                process.kill().await;
                let _ = health.send(Health::Down);
                return;
            }
        }
        let _ = health.send(Health::Down);
        let mut delay = RELAUNCH_FIRST;
        process = loop {
            tokio::select! {
                () = sleep(delay) => {}
                () = stopped(&mut stop) => return,
            }
            // 죽은 닻이 남긴 것을 치운다(최선): 닻의 소유 표지의 VM만 본다
            let _ = inner.reap_matching(Filter::Owner(&owner)).await;
            let launched = tokio::select! {
                launched = launch(&inner, address) => launched,
                () = stopped(&mut stop) => return,
            };
            match launched {
                Ok(process) => break process,
                Err(_) => delay = (delay * 2).min(RELAUNCH_MAX),
            }
        };
        generation += 1;
        let _ = health.send(Health::Up { generation });
    }
}

/// 어댑터 수명 동안 쥐는 닻. 놓으면 닻을 끝낸다.
pub(super) struct Anchor {
    health: watch::Receiver<Health>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    inner: Arc<Inner>,
    _lock: InstanceLock,
}

impl Anchor {
    /// 닻을 띄우고(주소가 호스트에 생길 때까지 기다린다) 감시를 시작한다. 건강은 `Inner::gate`에 건다.
    pub(super) async fn start(inner: Arc<Inner>, lock: InstanceLock) -> Result<Anchor, StartError> {
        let address = *inner.address.get().ok_or(StartError("gateway_invalid"))?;
        let process = launch(&inner, address).await?;
        let (health_tx, health) = watch::channel(Health::Up { generation: 1 });
        let _ = inner.gate.set(health.clone());
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            inner.clone(),
            address,
            process,
            health_tx,
            stop_rx,
        ));
        Ok(Anchor {
            health,
            stop,
            task: Some(task),
            inner,
            _lock: lock,
        })
    }

    pub(super) fn health(&self) -> watch::Receiver<Health> {
        self.health.clone()
    }

    /// 감시를 멈추고 이번 실행이 띄운 VM(닻과 모델 VM)을 모두 치운다. 그 뒤 모델 VM은 거절된다. 소유권은 이 핸들이 끝날 때 놓인다.
    pub(super) async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = timeout(STOP_WITHIN, task).await;
        }
        let _ = self.inner.reap_matching(Filter::Mine).await;
    }
}

impl Drop for Anchor {
    /// 그냥 놓아도 감시가 멈추고 닻을 끝낸다(모델 VM은 `Down`으로 거절된다).
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

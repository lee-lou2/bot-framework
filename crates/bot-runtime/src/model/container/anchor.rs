//! Anchor VM supervisor that keeps the host-only vmnet gateway IP present and gates model VM spawns.

use super::{ContainerProcess, Filter, Inner, StartError, args, lock::InstanceLock};
use bot_core::sandbox::ModelProcess;
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

const RELAUNCH_FIRST: Duration = Duration::from_millis(500);
const RELAUNCH_MAX: Duration = Duration::from_secs(10);
const STOP_WITHIN: Duration = Duration::from_secs(20);
const ADDRESS_MISSES: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Health {
    Up { generation: u64 },
    Down,
}

/// Requires a private RFC 1918 IPv4 address so the relay never binds to wildcard, loopback, link-local, or public IPs.
pub(super) fn gateway_ip(text: &str) -> Result<Ipv4Addr, StartError> {
    let ip: Ipv4Addr = text.parse().map_err(|_| StartError("gateway_invalid"))?;
    if !ip.is_private() {
        return Err(StartError("gateway_invalid"));
    }
    Ok(ip)
}

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

/// Detects vmnet interface resets that drop the host gateway IP even while the anchor VM stays alive.
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

async fn stopped(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() || rx.changed().await.is_err() {
            return;
        }
    }
}

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

pub(super) struct Anchor {
    health: watch::Receiver<Health>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    inner: Arc<Inner>,
    _lock: InstanceLock,
}

impl Anchor {
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

    pub(super) async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = timeout(STOP_WITHIN, task).await;
        }
        let _ = self.inner.reap_matching(Filter::Mine).await;
    }
}

impl Drop for Anchor {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

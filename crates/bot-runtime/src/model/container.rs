//! Apple `container` VM 어댑터(`Sandbox<ModelSpec>`): 모델 CLI를 호스트 전용(`--internal`) 망의 경량 VM에서 돌린다.
//!
//! - 시작은 타입으로 강제된다: `ContainerSandbox::start`가 설정·인스턴스 소유권·망(호스트 전용)·이미지(로컬, 다이제스트로 고정)·남은 고아·닻을 모두 확인해
//!   `Anchored`를 주고, 호스트가 프런트를 그 주소에 실제로 연 뒤 `Anchored::relay`로 알려야 `Sandbox`를 얻는다. 점검 없이 실행하는 길은 없다.
//! - 격리: 루트 파일시스템 읽기 전용(+`/tmp`만 tmpfs)·`--cap-drop ALL`·DNS 없음·호스트 전용 망·입력 읽기 전용 마운트와 scratch 하나만 쓰기.
//!   호스트 환경은 상속하지 않고 명세의 `env`만 준다(비밀은 명령줄이 아니라 클라이언트 환경으로). 운영은 이미지의 신뢰 init이 VM 안에 방화벽(릴레이만 허용)을 건다.
//! - 프런트가 바인드할 주소: 게이트웨이 IP는 그 망에 붙은 VM이 있을 때만 호스트에 생긴다(실측). 관리 라벨을 단 닻 VM이 그 주소를 지키고(`anchor.rs`),
//!   죽으면 다시 띄우며, 못 띄우는 동안 모델 VM은 거절된다. 릴레이 주소(IP:포트)는 호스트가 실제로 연 값으로 고정되고 명세의 환경이 바꾸지 못한다.
//! - 소유: 모든 VM에 `bot.instance`(이 호스트 인스턴스)·`bot.incarnation`(이번 프로세스 실행)·`bot.owner`·`bot.managed` 라벨과 접두사 이름을 붙인다.
//!   정리는 인스턴스 라벨·이름 모양이 정확히 같은 것만 보고, 소유권을 확인하지 못하면 지우지 않는다. 인스턴스는 잠금 파일로 한 프로세스만 쓴다.
//! - 모든 `spawn`은 닻·망·이미지를 다시 확인한다. 확인하지 못하면 실행하지 않는다(닫힌 채 실패).

mod anchor;
mod args;
mod lock;
mod process;
#[cfg(test)]
mod tests;

pub use anchor::Health;
pub use args::{ContainerConfig, Entry, Refused, owner_of};
pub use process::ContainerProcess;

use anchor::Anchor;
use args::{
    INCARNATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL, OWNER_LABEL, Plan, Role, is_ours,
    label_value, pin,
};
use bot_core::{
    id::Attempt,
    port::{Error, Reply},
    sandbox::{ModelProcess, ModelSpec, Sandbox},
};
use lock::{Identity, lock_instance};
use serde_json::Value;
use std::{
    net::{Ipv4Addr, SocketAddr},
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::watch,
    time::{Instant, timeout},
};

/// `container` 명령 하나를 기다리는 시간
const COMMAND_WITHIN: Duration = Duration::from_secs(30);
/// 릴레이가 받고 있는지 연결해 보는 시간
const RELAY_UP_WITHIN: Duration = Duration::from_secs(2);
/// 정리가 사라진 것을 확인할 때까지 다시 시도하는 횟수(뜨는 중인 VM은 `kill`·`delete`가 거절될 수 있다)
const REAP_ROUNDS: usize = 12;
/// `reap_matching` 전체 마감(`WorkerConfig::cleanup` 기본값 10초에 맞춰 정리 명령들이 이를 넘기지 않게 자른다)
const REAP_WITHIN: Duration = Duration::from_secs(10);

/// 시작하지 못한 이유(고정 낱말). 호스트는 프런트도 모델 실행도 열지 않는다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartError(pub &'static str);

/// 점검에서 알게 된 것.
struct Checked {
    /// 호스트 전용 망에서 호스트의 주소(VM이 프런트에 닿는 곳)
    gateway: String,
    /// 시작 때 로컬에서 고정한 이미지 참조(`이름@다이제스트`)
    image: String,
}

/// 호스트에 그 주소가 있는가(= 그 주소에 바인드할 수 있는가). 닻이 뜨면 게이트웨이 IP가 호스트에 생긴다.
fn host_has(address: Ipv4Addr) -> bool {
    std::net::TcpListener::bind((address, 0)).is_ok()
}

/// 호스트의 릴레이가 그 주소·포트에서 받고 있는가.
fn relay_listening(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, RELAY_UP_WITHIN).is_ok()
}

/// 호스트 쪽 사실을 보는 곳(시험이 바꾼다).
#[derive(Clone)]
struct Hooks {
    host_has: Arc<dyn Fn(Ipv4Addr) -> bool + Send + Sync>,
    relay_up: Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>,
    ready_within: Duration,
    /// 정리를 다시 시도하기 전에 쉬는 시간
    reap_pause: Duration,
    /// `reap_matching` 한 번이 쓸 수 있는 전체 마감(하위 명령 한도를 이 남은 시간에 맞춰 자른다)
    reap_within: Duration,
    /// 닻이 살아 있는 동안 호스트에 게이트웨이 주소가 아직 있는지 보는 간격
    address_poll: Duration,
    /// 모델 VM을 띄우기 전 확인(`ready`)에 줄 시간. `spawn`은 워커의 취소 신호 밖에서 기다리므로 컨테이너 서비스가 멈춰도 이 안에 끝난다(넘기면 닫힌 채 실패).
    ready_check: Duration,
}

impl Hooks {
    fn real() -> Self {
        Self {
            host_has: Arc::new(host_has),
            relay_up: Arc::new(relay_listening),
            ready_within: Duration::from_secs(60),
            reap_pause: Duration::from_millis(500),
            reap_within: REAP_WITHIN,
            address_poll: Duration::from_secs(1),
            ready_check: Duration::from_secs(5),
        }
    }
}

/// 이번 프로세스 실행의 표지(재시작마다 다르다). 같은 인스턴스의 이전 실행이 남긴 VM과 구별한다.
fn new_incarnation() -> String {
    bot_core::random::random_hex(8)
}

/// 정리할 VM의 종류. 어느 것이든 이 인스턴스의 라벨·이름 모양이 정확히 같은 것만 본다.
enum Filter<'a> {
    /// 이전 실행(다른 incarnation)이 남긴 것
    Orphans,
    /// 이 소유 표지의 것(실행과 무관)
    Owner(&'a str),
    /// 이번 실행이 띄운 것
    Mine,
}

/// 어댑터 상태. 닻 감시 작업이 같이 쥔다.
pub(crate) struct Inner {
    config: ContainerConfig,
    incarnation: String,
    serial: AtomicU64,
    /// 시작 때 확정하는 값들(한 번만 정해진다)
    pinned: OnceLock<String>,
    address: OnceLock<Ipv4Addr>,
    relay: OnceLock<SocketAddr>,
    gate: OnceLock<watch::Receiver<Health>>,
    /// 인스턴스 잠금 파일의 신원(어댑터가 사는 동안 같은 파일이어야 모델 VM을 띄운다)
    lock: OnceLock<Identity>,
    hooks: Hooks,
}

impl Inner {
    fn new(config: ContainerConfig, hooks: Hooks) -> Self {
        Self {
            config,
            incarnation: new_incarnation(),
            serial: AtomicU64::new(0),
            pinned: OnceLock::new(),
            address: OnceLock::new(),
            relay: OnceLock::new(),
            gate: OnceLock::new(),
            lock: OnceLock::new(),
            hooks,
        }
    }

    /// `container` 클라이언트: 호스트 환경을 상속하지 않고 필요한 것만 준다.
    fn client(&self) -> Command {
        let mut command = Command::new(&self.config.program);
        command.env_clear();
        for key in ["PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.stdin(Stdio::null()).kill_on_drop(true);
        command
    }

    /// 명령을 돌려 표준 출력을 JSON으로 읽는다. 실패(실행·종료 코드·시간·JSON)는 모두 `None`이다.
    async fn json(&self, args: &[&str], within: Duration) -> Option<Value> {
        let output = timeout(
            within,
            self.client()
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        output
            .status
            .success()
            .then(|| serde_json::from_slice(&output.stdout).ok())
            .flatten()
    }

    async fn ok(&self, args: &[&str], within: Duration) -> bool {
        timeout(
            within,
            self.client()
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|status| status.success())
    }

    /// 망 한 개의 `inspect` 첫 항목(호스트 전용인지·게이트웨이를 읽는다).
    async fn network(&self) -> Option<Value> {
        let found = self
            .json(
                &["network", "inspect", &self.config.network],
                COMMAND_WITHIN,
            )
            .await?;
        found.as_array()?.first().cloned()
    }

    /// 망이 호스트 전용인지 확인하고(없으면 `--internal`로 만든다) 이미지가 로컬에 있는지 보고 다이제스트로 고정한다.
    /// 어느 하나라도 어긋나면 시작하지 않는다.
    async fn preflight(&self) -> Result<Checked, StartError> {
        let mut found = self.network().await;
        if found.is_none() {
            let label = format!("{MANAGED_LABEL}=1");
            let mut create = vec!["network", "create", "--internal", "--label", &label];
            if let Some(subnet) = &self.config.subnet {
                create.extend(["--subnet", subnet]);
            }
            create.push(&self.config.network);
            let created = self.ok(&create, COMMAND_WITHIN).await;
            if !created {
                return Err(StartError("network_create_failed"));
            }
            found = self.network().await;
        }
        let first = found.ok_or(StartError("network_unreadable"))?;
        if first["configuration"]["mode"] != "hostOnly" {
            return Err(StartError("network_not_internal"));
        }
        let gateway = first["status"]["ipv4Gateway"]
            .as_str()
            .filter(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
            .ok_or(StartError("network_no_gateway"))?
            .to_owned();
        let inspected = self
            .json(&["image", "inspect", &self.config.image], COMMAND_WITHIN)
            .await
            .ok_or(StartError("image_missing"))?;
        let item = inspected
            .as_array()
            .and_then(|a| a.first())
            .unwrap_or(&inspected);
        let digest = item["configuration"]["descriptor"]["digest"]
            .as_str()
            .ok_or(StartError("image_unpinnable"))?;
        let image = pin(&self.config.image, digest).ok_or(StartError("image_unpinnable"))?;
        Ok(Checked { gateway, image })
    }

    /// 모델 VM을 띄워도 되는가: 닻이 살아 있고, 망이 아직 호스트 전용이며 같은 게이트웨이이고, 고정한 이미지가 로컬에 있어야 한다.
    /// 하나라도 확인하지 못하면 아니다(닫힌 채 실패).
    async fn ready(&self) -> bool {
        let (Some(gate), Some(address), Some(image), Some(lock)) = (
            self.gate.get(),
            self.address.get(),
            self.pinned.get(),
            self.lock.get(),
        ) else {
            return false;
        };
        // 인스턴스 소유를 보증하는 잠금 파일이 지워지거나 바뀌었다면 다른 어댑터가 같은 인스턴스를 시작했을 수 있다: 닫힌 채 실패한다
        if !lock.holds() {
            return false;
        }
        if !matches!(*gate.borrow(), Health::Up { .. }) {
            return false;
        }
        // 닻 프로세스가 살아 있어도 호스트에 게이트웨이 주소가 없으면 VM이 프런트에 닿지 못한다
        if !(self.hooks.host_has)(*address) {
            return false;
        }
        let inspect = ["image", "inspect", image];
        let (network, present) = tokio::join!(self.network(), self.ok(&inspect, COMMAND_WITHIN));
        present
            && network.is_some_and(|n| {
                n["configuration"]["mode"] == "hostOnly"
                    && n["status"]["ipv4Gateway"].as_str() == Some(address.to_string().as_str())
            })
    }

    /// 이 인스턴스의 VM ID들(관리 라벨·인스턴스 라벨·이름 모양이 정확히 맞는 것만). 조회가 실패하면 `None`(모름).
    /// 라벨이 없거나 다르면 우리 것이 아니므로 보존한다.
    async fn listed(&self, filter: &Filter<'_>, within: Duration) -> Option<Vec<String>> {
        let all = self
            .json(&["ls", "--all", "--format", "json"], within)
            .await?;
        let instance = label_value(&self.config.instance);
        Some(
            all.as_array()?
                .iter()
                .filter(|c| {
                    let labels = &c["configuration"]["labels"];
                    let incarnation = labels[INCARNATION_LABEL].as_str();
                    labels[MANAGED_LABEL] == "1"
                        && labels[INSTANCE_LABEL].as_str() == Some(instance.as_str())
                        && match filter {
                            Filter::Orphans => incarnation != Some(self.incarnation.as_str()),
                            Filter::Owner(owner) => {
                                labels[OWNER_LABEL].as_str() == Some(label_value(owner).as_str())
                            }
                            Filter::Mine => incarnation == Some(self.incarnation.as_str()),
                        }
                })
                .filter_map(|c| {
                    c["configuration"]["id"]
                        .as_str()
                        .or_else(|| c["id"].as_str())
                        .filter(|id| is_ours(&self.config.prefix, id))
                        .map(str::to_owned)
                })
                .collect(),
        )
    }

    /// 걸리는 VM을 모두 끝내고 사라진 것을 확인한다. 끝낸 수. 공유 마감(`reap_within`) 안에서 각 명령의 시간 한도를 남은 시간에 맞춰 자른다.
    /// 뜨는 중인 VM은 한 번에 안 지워질 수 있어 사라질 때까지 몇 번 다시 하고, 마감 안에 확인하지 못하면 `Unavailable`이다.
    async fn reap_matching(&self, filter: Filter<'_>) -> Result<usize, Error> {
        let deadline = Instant::now() + self.hooks.reap_within;
        let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
        let found = self
            .listed(&filter, within)
            .await
            .ok_or(Error::Unavailable)?;
        if found.is_empty() {
            return Ok(0);
        }
        let mut left = found.clone();
        for round in 0..REAP_ROUNDS {
            if round > 0 {
                let pause =
                    process::budget(deadline, self.hooks.reap_pause).ok_or(Error::Unavailable)?;
                tokio::time::sleep(pause).await;
            }
            for id in &left {
                let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
                let _ = self.ok(&["kill", id], within).await;
                let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
                let _ = self.ok(&["delete", "--force", id], within).await;
            }
            // 사라진 것을 확인해야 성공한다. 다시 읽지 못하면 "모름"이다
            let within = process::budget(deadline, COMMAND_WITHIN).ok_or(Error::Unavailable)?;
            left = self
                .listed(&filter, within)
                .await
                .ok_or(Error::Unavailable)?
                .into_iter()
                .filter(|id| found.contains(id))
                .collect();
            if left.is_empty() {
                return Ok(found.len());
            }
        }
        Err(Error::Unavailable)
    }

    /// `container run` 클라이언트를 띄운다. 모델 VM은 명세로, 닻 VM은 고정 명령으로 만든다.
    async fn run_vm(
        &self,
        role: Role,
        spec: Option<&ModelSpec>,
    ) -> Result<ContainerProcess, Error> {
        let image = self.pinned.get().ok_or(Error::Unavailable)?;
        let plan = Plan {
            image,
            incarnation: &self.incarnation,
            serial: self.serial.fetch_add(1, Ordering::Relaxed),
            relay: self.relay.get().copied(),
        };
        // 안전하지 않은 명세는 띄우지 않는다(폴백 없음)
        let (command, stdin) = match (role, spec) {
            (Role::Model, Some(spec)) => (
                args::build(&self.config, spec, &plan).map_err(|_| Error::Invalid)?,
                spec.stdin.clone(),
            ),
            (Role::Anchor, _) => (
                args::build_anchor(&self.config, &plan).map_err(|_| Error::Invalid)?,
                None,
            ),
            (Role::Model, None) => return Err(Error::Invalid),
        };
        let mut client = self.client();
        client
            .args(&command.args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &command.secret_env {
            client.env(key, value);
        }
        let mut child = client.spawn().map_err(|_| Error::Unavailable)?;
        if let (Some(body), Some(mut pipe)) = (stdin, child.stdin.take()) {
            tokio::spawn(async move {
                let _ = pipe.write_all(&body).await;
                let _ = pipe.shutdown().await;
            });
        }
        ContainerProcess::new(child, self.config.program.clone(), command.name)
            .ok_or(Error::Unavailable)
    }
}

/// 점검과 닻을 마쳐 프런트가 바인드할 주소를 아는 단계. 아직 모델 VM을 띄울 수 없다.
pub struct Anchored {
    inner: Arc<Inner>,
    anchor: Anchor,
    address: Ipv4Addr,
}

impl Anchored {
    /// 망에서 확인한 게이트웨이 주소: 프런트는 이 주소에만 바인드한다(0.0.0.0·loopback 금지).
    pub fn address(&self) -> Ipv4Addr {
        self.address
    }

    /// 이 주소의 `port`(0이면 임시 포트)에 바인드할 소켓 주소.
    pub fn bind_addr(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.address.into(), port)
    }

    /// 닻의 건강. `Down`인 동안 모델 VM은 거절되고, `Up`의 세대가 오르면 닻이 다시 떴다.
    pub fn health(&self) -> watch::Receiver<Health> {
        self.anchor.health()
    }

    /// 호스트가 프런트를 이 주소의 `port`에 실제로 열었다고 알린다. 받고 있는지 확인한 뒤에야 실행 가능한 어댑터를 준다.
    /// 릴레이 주소는 이때 고정되어 모델 VM의 `BOT_RELAY_IP`·`BOT_RELAY_PORT`·`ANTHROPIC_BASE_URL`이 된다.
    pub async fn relay(self, port: u16) -> Result<ContainerSandbox, StartError> {
        let relay = SocketAddr::new(self.address.into(), port);
        let probe = self.inner.hooks.relay_up.clone();
        let up = port != 0
            && tokio::task::spawn_blocking(move || probe(relay))
                .await
                .unwrap_or(false);
        if !up || self.inner.relay.set(relay).is_err() {
            self.anchor.shutdown().await;
            return Err(StartError(if port == 0 {
                "relay_port"
            } else {
                "relay_unreachable"
            }));
        }
        Ok(ContainerSandbox {
            inner: self.inner,
            anchor: Mutex::new(Some(self.anchor)),
        })
    }

    /// 프런트를 열지 못했다: 닻과 이번 실행의 VM을 치우고 소유권을 놓는다.
    pub async fn abort(self) {
        self.anchor.shutdown().await;
    }
}

/// 실행 가능한 어댑터: 점검된 망·고정한 로컬 이미지·닻·실제로 연 릴레이를 함께 쥔다. `ContainerSandbox::start`와 `Anchored::relay`로만 얻는다.
pub struct ContainerSandbox {
    inner: Arc<Inner>,
    anchor: Mutex<Option<Anchor>>,
}

impl ContainerSandbox {
    /// 시작: 설정 확인 → 인스턴스 소유권 잠금 → 망·이미지 점검(이미지는 다이제스트로 고정) → 게이트웨이 확인 → 이전 실행이 남긴 이 인스턴스의 고아 정리 → 닻.
    /// 어느 단계든 실패하면 아무것도 남기지 않고 오류다. 설정이 무효하면 `container`를 한 번도 부르지 않는다.
    pub async fn start(config: ContainerConfig) -> Result<Anchored, StartError> {
        Self::start_with(config, Hooks::real()).await
    }

    async fn start_with(config: ContainerConfig, hooks: Hooks) -> Result<Anchored, StartError> {
        args::valid(&config).map_err(|_| StartError("config"))?;
        // 운영은 이미지의 신뢰 init(방화벽)이 있어야 한다. 시험만 `unfirewalled`로 끈다
        if config.entry.is_none() && !config.unfirewalled {
            return Err(StartError("firewall_required"));
        }
        let lock = lock_instance(&config).await?;
        let inner = Arc::new(Inner::new(config, hooks));
        let _ = inner.lock.set(lock.identity());
        let checked = inner.preflight().await?;
        let address = anchor::gateway_ip(&checked.gateway)?;
        let _ = inner.pinned.set(checked.image);
        let _ = inner.address.set(address);
        inner
            .reap_matching(Filter::Orphans)
            .await
            .map_err(|_| StartError("cleanup_failed"))?;
        let anchor = Anchor::start(inner.clone(), lock).await?;
        Ok(Anchored {
            inner,
            anchor,
            address,
        })
    }

    /// 시험용: 호스트 쪽 확인을 바꿔 시작한다.
    #[cfg(test)]
    async fn start_probed(
        config: ContainerConfig,
        host_has: impl Fn(Ipv4Addr) -> bool + Send + Sync + 'static,
        relay_up: impl Fn(SocketAddr) -> bool + Send + Sync + 'static,
        ready_within: Duration,
    ) -> Result<Anchored, StartError> {
        let hooks = Hooks {
            host_has: Arc::new(host_has),
            relay_up: Arc::new(relay_up),
            ready_within,
            reap_pause: Duration::from_millis(1),
            reap_within: REAP_WITHIN,
            address_poll: Duration::from_millis(20),
            ready_check: Duration::from_secs(5),
        };
        Self::start_with(config, hooks).await
    }

    /// 프런트가 열린 게이트웨이 주소.
    pub fn address(&self) -> Ipv4Addr {
        self.inner
            .address
            .get()
            .copied()
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// 닻의 건강.
    pub fn health(&self) -> Option<watch::Receiver<Health>> {
        self.inner.gate.get().cloned()
    }

    /// 닻과 이번 실행의 VM을 모두 치우고 인스턴스 소유권을 놓는다. 그 뒤 모델 VM은 거절된다.
    pub async fn shutdown(&self) {
        let anchor = self
            .anchor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(anchor) = anchor {
            anchor.shutdown().await;
        }
    }

    /// 이 시도의 소유 표지(`reap`·라벨이 쓰는 값).
    pub fn owner_of(attempt: &Attempt) -> String {
        format!(
            "{}.{}.g{}.c{}",
            attempt.profile.0,
            attempt.run.0,
            attempt.generation.0,
            attempt.claim.map_or(0, |c| c.0)
        )
    }
}

impl Sandbox<ModelSpec> for ContainerSandbox {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async move {
            // 매번 확인한다: 닻·망·이미지 중 하나라도 확인하지 못하면(시간 안에 끝내지 못해도) 모델을 돌리지 않는다
            if !matches!(
                timeout(self.inner.hooks.ready_check, self.inner.ready()).await,
                Ok(true)
            ) {
                return Err(Error::Unavailable);
            }
            let process = self.inner.run_vm(Role::Model, Some(&spec)).await?;
            Ok(Box::new(process) as Box<dyn ModelProcess>)
        })
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        let owner = owner.to_owned();
        Box::pin(async move {
            self.inner
                .reap_matching(Filter::Owner(&owner))
                .await
                .map(|_| ())
        })
    }
}

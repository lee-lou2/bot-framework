//! srt(Anthropic sandbox-runtime) 어댑터: 도구 서버를 OS 수준(macOS Seatbelt·Linux bubblewrap)으로 가둔다(`Sandbox` 구현).
//!
//! 우리 보장은 srt가 아니라 이 어댑터와 시험(카나리)이 증명한다. srt는 0.x라 설정 형식이 바뀔 수 있으므로 판본을 상수
//! (`PINNED_VERSION`)에 고정하고, 없거나 판본이 다르면 열지 않는다(폴백 없음. 호출자가 다른 판본에 맞춰 통과시킬 수 없다).
//! - 네트워크: srt의 허용 도메인은 우리 허용 목록(선언 ∩ 운영자 상한)과 같게 두고, srt의 프록시가 내보내는 연결은
//!   브로커가 연 `CONNECT` 프록시(호출 전용 자격, 주소 검사)를 거치게 한다(`parentProxy`). 허용할 곳이 없으면 네트워크는 0이다.
//! - 파일: 읽기는 운영자가 정한 곳(홈·프로필·데이터·비밀 폴더)과 작업 루트를 막고 이 호출의 작업 폴더·서버 설치 폴더만 다시 연다.
//!   쓰기는 이 호출의 작업 폴더뿐이다(srt가 늘 쓰기 가능으로 두는 `/tmp/claude`도 `denyWrite`로 막는다). 읽기도 막는다: macOS는 `denyRead`로 막히고,
//!   Linux의 srt는 이 폴더를 다시 붙이므로 `bwrap` 대리 실행 파일이 샌드박스 안에서 빈 읽기 전용 tmpfs로 덮는다(`overlay`).
//!   설정 파일(프록시 자격 포함)은 작업 폴더 밖에 0600으로 둔다.
//! - 환경: **srt 런처의 환경은 호스트가 고정한다**(`PATH`·`CLAUDE_CODE_TMPDIR`뿐). 도구가 선언한 자격은 이름이 무엇이든
//!   런처 환경에 두지 않고, 고정 이름의 운반 변수(`BOT_CARRY_<번호>`)로 넘긴 뒤 격리된 자식 안에서 선언한 이름으로 옮겨 단다.
//!   그래서 선언한 이름이 런처의 동작(`PATH`·`NODE_OPTIONS` 등)을 바꿀 수 없다. 로더·셸·srt 자신의 변수 이름은 아예 거절한다.
//! - 종료: 프로세스 그룹 전체를 끝내고 사라진 것을 확인한 뒤에야 성공한다(좀비만 남은 그룹은 사라진 것으로 본다). 조회가 실패하면 "사라짐"이 아니라 "모름"이다.
//!   `reap`은 종료를 증명할 기록이 없으면(다른 호스트·손상·메타 폴더 읽기 실패) 성공하지 않는다.
//! - 아직 지원하지 않는 것: 표준 입출력 밖 통로(FD)는 srt가 주지 않으므로 단계 원장 채널은 프록시 경유(`effects_via_proxy`)로 한다.

mod env;
mod overlay;
mod tools;

use crate::{
    sandbox::{Handle, Pipe, Process, Sandbox, SandboxSpec},
    sys::Entropy,
    text::hex,
};
use bot_core::{
    install::ServerState,
    port::{Error, Reply},
};
pub use env::forbidden_env_name;
use env::{carry_script, safe};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::timeout,
};
use tools::{Group, Tools, find_tool, private_dir, write_private};

/// 시험한 srt 판본. 계약이 바뀔 수 있어 고정한다. 설정으로 바꿀 수 없다.
pub const PINNED_VERSION: &str = "0.0.83";

/// 운반 변수 이름 앞부분. 런처 환경에는 이 고정 이름만 올라간다.
const CARRY: &str = "BOT_CARRY_";

/// 호스트가 자식에게 넘겨 줄 수 있는 비밀 아닌 변수 이름.
const HOST_ENV: [&str; 1] = ["BOT_EFFECT_URL"];

/// 종료 표식을 남겨 두는 기간(복구가 같은 소유자를 다시 확인할 수 있게 한다).
const TOMBSTONE_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

const PROXY_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

#[derive(Clone, Debug)]
pub struct ServerCommand {
    /// 서버를 띄우는 명령(운영자가 설치한 실행 파일 경로와 인자).
    pub argv: Vec<String>,
    /// 승인에 묶인 설치 판본. `SandboxSpec.image`와 같아야 한다.
    pub revision: String,
    /// 서버 파일이 있는 폴더(읽기를 다시 연다).
    pub install_dir: PathBuf,
    /// 운영자가 설치한 서버가 호출 간 전용 상태 볼륨을 요구하는가. srt 어댑터는 영속 볼륨을 지원하지 않으므로 `Required`는
    /// 자격을 풀거나 프로세스를 시작하기 전에 명시적으로 거절한다(영속 볼륨은 별도 어댑터의 격리·용량·잠금을 검증한 뒤에 쓴다).
    pub state: ServerState,
}

#[derive(Clone, Debug)]
pub struct SrtConfig {
    pub binary: PathBuf,
    /// 호출별 작업 폴더를 만드는 곳. 이 폴더 자체는 도구가 읽지 못한다.
    pub work_root: PathBuf,
    pub servers: BTreeMap<String, ServerCommand>,
    /// 도구가 읽지 못해야 하는 곳: 호스트 홈·프로필 루트·데이터·비밀·DB 폴더.
    pub deny_read: Vec<PathBuf>,
    /// srt 자신이 필요한 `PATH`(node·bubblewrap 등).
    pub path: String,
    /// 프로세스 그룹이 사라지기를 기다리는 최대 시간.
    pub terminate_wait: Duration,
    /// 호스트 도구 경로를 직접 정한다. `None`이면 표준 위치에서 찾는다(시험이 바꿔 끼운다).
    pub host_tools: Option<HostTools>,
}

/// 프로세스 그룹을 다루는 호스트 도구의 절대 경로.
#[derive(Clone, Debug)]
pub struct HostTools {
    pub kill: PathBuf,
    pub pgrep: PathBuf,
    pub ps: PathBuf,
}

struct Live {
    pgid: u32,
    work: PathBuf,
}

pub struct SrtSandbox {
    cfg: Arc<SrtConfig>,
    tools: Tools,
    entropy: Arc<dyn Entropy>,
    live: Arc<Mutex<HashMap<String, Live>>>,
    /// Linux의 실제 `bwrap`(srt가 부를 것). 있으면 호출마다 대리 실행 파일을 만들어 srt가 그것을 부르게 한다: 공용 임시 폴더를 샌드박스 안에서 비워 덮는다.
    /// 다른 운영체제는 `None`이다(`overlay`).
    real_bwrap: Option<PathBuf>,
}

/// srt 설정 JSON. 순수 함수라 시험으로 정책을 직접 확인한다.
pub fn settings(spec: &SandboxSpec, work: &Path, install: &Path, cfg: &SrtConfig) -> Value {
    let paths = |items: &[&Path]| -> Vec<String> {
        items.iter().map(|p| p.display().to_string()).collect()
    };
    let mut deny_read: Vec<&Path> = cfg.deny_read.iter().map(PathBuf::as_path).collect();
    deny_read.push(&cfg.work_root);
    // srt가 늘 쓰기 가능으로 두는 공용 임시 폴더는 읽지도 쓰지도 못하게 한다(호출 사이에 데이터를 주고받는 통로를 막는다).
    let shared_tmp = [Path::new("/tmp/claude"), Path::new("/private/tmp/claude")];
    deny_read.extend(shared_tmp);
    let mut network = json!({
        "allowedDomains": spec.egress.as_ref().map(|_| spec.egress_hosts.clone()).unwrap_or_default(),
        "deniedDomains": [],
        "strictAllowlist": true,
        "allowLocalBinding": false,
        "allowUnixSockets": [work.display().to_string()],
    });
    if let Some(endpoint) = &spec.egress {
        network["parentProxy"] = json!({"http": endpoint.url, "https": endpoint.url});
    }
    json!({
        "network": network,
        "filesystem": {
            "denyRead": paths(&deny_read),
            "allowRead": paths(&[work, install]),
            "allowWrite": paths(&[work]),
            "denyWrite": paths(&shared_tmp),
        },
    })
}

impl SrtSandbox {
    /// srt가 있고 판본이 고정 상수와 같은지 확인한다. 아니면 열지 않는다(폴백 없음).
    pub async fn open(cfg: SrtConfig, entropy: Arc<dyn Entropy>) -> Result<Self, Error> {
        let tools = match cfg.host_tools.clone() {
            Some(tools) => tools,
            None => Tools {
                kill: find_tool(&["/bin/kill", "/usr/bin/kill"]).ok_or(Error::Unavailable)?,
                pgrep: find_tool(&["/usr/bin/pgrep", "/bin/pgrep"]).ok_or(Error::Unavailable)?,
                ps: find_tool(&["/bin/ps", "/usr/bin/ps"]).ok_or(Error::Unavailable)?,
            },
        };
        let output = timeout(
            Duration::from_secs(15),
            Command::new(&cfg.binary)
                .arg("--version")
                .env_clear()
                .env("PATH", &cfg.path)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let seen = text
            .split_whitespace()
            .last()
            .map(|t| t.trim_start_matches('v'));
        if !output.status.success() || seen != Some(PINNED_VERSION) {
            return Err(Error::Unavailable);
        }
        private_dir(&cfg.work_root).map_err(|_| Error::Unavailable)?;
        let real_bwrap = overlay::locate(std::env::consts::OS, &cfg.path);
        let sandbox = Self {
            cfg: Arc::new(cfg),
            tools,
            entropy,
            live: Arc::default(),
            real_bwrap,
        };
        private_dir(&sandbox.meta_dir()).map_err(|_| Error::Unavailable)?;
        sandbox.purge_old_tombstones();
        Ok(sandbox)
    }

    fn meta_dir(&self) -> PathBuf {
        self.cfg.work_root.join(".srt")
    }

    fn purge_old_tombstones(&self) {
        let Ok(entries) = std::fs::read_dir(self.meta_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let old = path.extension().and_then(|e| e.to_str()) == Some("done")
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .is_some_and(|age| age > TOMBSTONE_KEEP);
            if old {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// 종료를 확인한 호출의 흔적을 지우고 표식만 남긴다. 표식이 있으면 같은 소유자의 `reap`이 다시 성공한다.
    fn settle(meta_dir: &Path, name: &str, work: &Path) {
        let _ = std::fs::remove_dir_all(work);
        let _ = std::fs::remove_file(meta_dir.join(format!("{name}.json")));
        let _ = std::fs::remove_file(meta_dir.join(format!("{name}.bwrap")));
        let _ = write_private(&meta_dir.join(format!("{name}.done")), "done\n");
        let _ = std::fs::remove_file(meta_dir.join(format!("{name}.pid")));
    }

    /// 설치한 서버와 승인에 묶인 판본이 같고 srt가 실행할 수 있는 서버인가(상태 볼륨을 요구하면 거절).
    fn admitted(&self, server: &str, image: &str) -> Result<&ServerCommand, Error> {
        let found = self
            .cfg
            .servers
            .get(server)
            .filter(|s| s.revision == image)
            .ok_or(Error::Denied)?;
        if found.state == ServerState::Required {
            return Err(Error::Denied);
        }
        Ok(found)
    }

    async fn start(&self, spec: SandboxSpec) -> Result<Process, Error> {
        // 설치한 서버와 승인에 묶인 판본이 같아야 하고, 상태 볼륨을 요구하는 서버는 어떤 자격·설정도 만들기 전에 거절한다.
        let server = self.admitted(&spec.server, &spec.image)?;
        if spec.effects {
            return Err(Error::Unavailable);
        }
        // 선언한 이름이 런처·셸·srt의 동작을 바꿀 수 있으면 띄우지 않는다. 프록시 변수는 srt가 스스로 정한다.
        let secrets: Vec<&(String, String)> = spec
            .env
            .iter()
            .filter(|(k, _)| !PROXY_VARS.contains(&k.as_str()))
            .collect();
        if secrets
            .iter()
            .any(|(k, v)| forbidden_env_name(k) || v.contains('\0'))
        {
            return Err(Error::Denied);
        }
        // 호스트가 정한 비밀 아닌 변수는 허용한 이름만 받는다.
        if spec
            .host_env
            .iter()
            .any(|(k, v)| !HOST_ENV.contains(&k.as_str()) || v.contains('\0'))
        {
            return Err(Error::Denied);
        }
        let mut random = [0u8; 16];
        if !self.entropy.fill(&mut random) {
            return Err(Error::Unavailable);
        }
        let name = format!("{}-{}", safe(&spec.owner), hex(&random));
        let (work, meta_dir) = (self.cfg.work_root.join(&name), self.meta_dir());
        let (settings_path, meta) = (
            meta_dir.join(format!("{name}.json")),
            meta_dir.join(format!("{name}.pid")),
        );
        private_dir(&meta_dir)
            .and_then(|_| private_dir(&work))
            .map_err(|_| Error::Unavailable)?;
        let mut value = settings(&spec, &work, &server.install_dir, &self.cfg);
        if let Some(real) = &self.real_bwrap {
            let launcher = meta_dir.join(format!("{name}.bwrap"));
            overlay::write_launcher(&launcher, real).map_err(|_| Error::Unavailable)?;
            let launcher = std::path::absolute(launcher).map_err(|_| Error::Unavailable)?;
            value["bwrapPath"] = json!(launcher.display().to_string());
        }
        write_private(&settings_path, &value.to_string()).map_err(|_| Error::Unavailable)?;
        // 띄우기 전에 의도를 먼저 적는다. 띄운 뒤 그룹 번호를 채우기 전에 죽으면 종료를 증명할 수 없는 기록이 남는다.
        write_private(&meta, &format!("pending\n{}\n", work.display()))
            .map_err(|_| Error::Unavailable)?;
        let mut command = Command::new(&self.cfg.binary);
        command
            .arg("--settings")
            .arg(&settings_path)
            .arg("--")
            .env_clear()
            .env("PATH", &self.cfg.path)
            // srt는 자식의 TMPDIR을 이 값(없으면 모두가 쓰는 /tmp/claude)으로 정한다. 호출별 작업 폴더로 돌린다.
            .env("CLAUDE_CODE_TMPDIR", &work)
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if secrets.is_empty() && spec.host_env.is_empty() {
            command.args(&server.argv);
        } else {
            // 자격은 고정 이름의 운반 변수로만 런처에 올리고, 격리된 자식 안에서 선언한 이름으로 옮겨 단다.
            let all: Vec<&(String, String)> = secrets
                .iter()
                .copied()
                .chain(spec.host_env.iter())
                .collect();
            let names: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
            command
                .args(["/bin/sh", "-c", &carry_script(&names), "sh"])
                .args(&server.argv);
            for (i, (_, value)) in all.iter().enumerate() {
                command.env(format!("{CARRY}{i}"), value);
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                Self::settle(&meta_dir, &name, &work);
                return Err(Error::Unavailable);
            }
        };
        let (Some(pgid), Some(stdin), Some(stdout), Some(mut stderr)) = (
            child.id(),
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
        ) else {
            let _ = child.start_kill();
            return Err(Error::Unavailable);
        };
        // 진단 출력은 비워 내보내되 쌓지 않는다(막히지 않게만 읽는다).
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            while matches!(stderr.read(&mut sink).await, Ok(n) if n > 0) {}
        });
        write_private(&meta, &format!("{pgid}\n{}\n", work.display()))
            .map_err(|_| Error::Unavailable)?;
        self.live.lock().unwrap_or_else(|e| e.into_inner()).insert(
            spec.owner.clone(),
            Live {
                pgid,
                work: work.clone(),
            },
        );
        Ok(Process {
            stdio: Pipe {
                read: Box::new(stdout),
                write: Box::new(stdin),
            },
            effects: None,
            handle: Box::new(SrtHandle {
                child,
                pgid,
                owner: spec.owner,
                name,
                work,
                meta_dir,
                tools: self.tools.clone(),
                live: Arc::clone(&self.live),
                wait: self.cfg.terminate_wait,
            }),
        })
    }
}

struct SrtHandle {
    child: Child,
    pgid: u32,
    owner: String,
    name: String,
    work: PathBuf,
    meta_dir: PathBuf,
    tools: Tools,
    live: Arc<Mutex<HashMap<String, Live>>>,
    wait: Duration,
}

impl Handle for SrtHandle {
    fn terminate(self: Box<Self>) -> Reply<'static, ()> {
        Box::pin(async move {
            let mut me = *self;
            me.tools.kill_group(me.pgid).await;
            let _ = timeout(me.wait, me.child.wait()).await;
            if me.tools.gone_within(me.pgid, me.wait).await {
                me.live
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&me.owner);
                SrtSandbox::settle(&me.meta_dir, &me.name, &me.work);
                Ok(())
            } else {
                Err(Error::Unavailable)
            }
        })
    }
}

impl Sandbox for SrtSandbox {
    fn spawn(&self, spec: SandboxSpec) -> Reply<'_, Process> {
        Box::pin(self.start(spec))
    }

    /// srt는 표준 입출력 밖의 통로(FD)를 주지 않는다. 단계 원장은 브로커가 연 프록시의 전용 주소로 받는다.
    fn effects_via_proxy(&self) -> bool {
        true
    }

    /// 자격을 풀기 전에 미리 묻는다: 설치한 판본과 같고 상태 볼륨을 요구하지 않을 때만 받는다.
    fn admit(&self, server: &str, image: &str) -> Result<(), Error> {
        self.admitted(server, image).map(|_| ())
    }

    /// 이 소유 표지가 남긴 프로세스를 끝내고 사라진 것을 **증명**한다. 증명할 수 없으면 성공하지 않는다:
    /// 메타 폴더를 읽지 못하거나, 이 소유자의 기록이 하나도 없거나(다른 호스트·손상), 기록이 불완전하거나, 조회·종료가 실패하면
    /// `Unavailable`이다. 이미 끝낸 소유자는 종료 표식으로 다시 확인한다.
    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        let owner = owner.to_owned();
        Box::pin(async move {
            let wait = self.cfg.terminate_wait;
            // 이 프로세스가 띄운 것은 메모리 기록으로 먼저 끝낸다.
            let live = self
                .live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&owner);
            if let Some(live) = live {
                self.tools.kill_group(live.pgid).await;
                if !self.tools.gone_within(live.pgid, wait).await {
                    self.live
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(owner.clone(), live);
                    return Err(Error::Unavailable);
                }
                let _ = std::fs::remove_dir_all(&live.work);
            }
            let prefix = format!("{}-", safe(&owner));
            let entries = match std::fs::read_dir(self.meta_dir()) {
                Ok(entries) => entries,
                Err(_) => return Err(Error::Unavailable),
            };
            let (mut proved, mut running) = (false, Vec::new());
            for entry in entries {
                let path = entry.map_err(|_| Error::Unavailable)?.path();
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_owned();
                if !stem.starts_with(&prefix) {
                    continue;
                }
                match path.extension().and_then(|e| e.to_str()) {
                    Some("done") => proved = true,
                    Some("pid") => running.push((stem, path)),
                    _ => {}
                }
            }
            for (stem, path) in running {
                let text = std::fs::read_to_string(&path).map_err(|_| Error::Unavailable)?;
                let mut lines = text.lines();
                // 그룹 번호를 아직 못 적은 기록(띄우다 죽음)은 종료를 증명할 수 없다.
                let pgid: u32 = lines
                    .next()
                    .and_then(|l| l.parse().ok())
                    .ok_or(Error::Unavailable)?;
                let work = PathBuf::from(lines.next().ok_or(Error::Unavailable)?);
                match self.tools.probe(pgid).await {
                    Group::Unknown => return Err(Error::Unavailable),
                    Group::Alive => {
                        // 번호가 다른 프로세스에 재사용됐을 수 있다. srt 명령줄에 우리 이름이 있는 선두만 우리 것이다.
                        let leader = self
                            .tools
                            .leader_command(pgid)
                            .await
                            .ok_or(Error::Unavailable)?;
                        if !leader.contains(&stem) {
                            return Err(Error::Unavailable);
                        }
                        self.tools.kill_group(pgid).await;
                        if !self.tools.gone_within(pgid, wait).await {
                            return Err(Error::Unavailable);
                        }
                    }
                    Group::Gone => {}
                }
                Self::settle(&self.meta_dir(), &stem, &work);
                proved = true;
            }
            if proved {
                Ok(())
            } else {
                Err(Error::Unavailable)
            }
        })
    }
}

#[cfg(test)]
mod tests;

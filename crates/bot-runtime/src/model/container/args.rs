//! `container run` 인자 조립과 입력 검증(순수 함수). 실행 명세에서 온 값은 모두 여기서 엄격하게 거른 뒤에만 명령줄에 오른다.
//! - 마운트 원본은 호스트가 정한 `mount_root` 아래의 실제 폴더여야 한다(심볼릭 링크를 풀어 확인). 쉼표·따옴표·줄바꿈이 든 경로는 `--mount` 값을 깨뜨리므로 거절한다.
//! - 비밀 같은 환경 값(`KEY`·`TOKEN`·`SECRET`·`PASSWORD`·`AUTH`가 이름에 든 것)은 `-e NAME`(값 없이 이름만)으로 넘기고 값은 클라이언트 프로세스의
//!   환경에 둔다: 호스트의 `ps`에 보이는 명령줄에 값이 없다.

use bot_core::sandbox::ModelSpec;
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// 고정 이미지의 신뢰 진입(호스트가 정한다: 모델 요청 인자로 고르지 않는다).
/// 모델 VM은 이미지의 고정 ENTRYPOINT(`bot-init`)가 root로 시작해 방화벽(릴레이만 허용)을 걸고 capability를 모두 버린 뒤 비root로 고정 Claude를 exec한다.
#[derive(Clone, Debug)]
pub struct Entry {
    /// 닻 VM의 별도 대기 진입(비root·capability 0·URL 없음).
    pub anchor: String,
    /// 닻 VM의 사용자(비root)
    pub anchor_user: (u32, u32),
    /// 모델 VM이 시작할 때 필요한 최소 capability(`NET_ADMIN` 등, 이미지의 init 실측으로 정한다). 나머지는 `--cap-drop ALL`이다.
    pub model_caps: Vec<String>,
}

impl Entry {
    /// 운영 이미지(`images/model`)의 계약: 닻은 `/usr/local/bin/bot-anchor`(비root `10001:10001`·capability 0), 모델 init(`bot-init`)이 시작할 때 쓰는 capability는
    /// 방화벽(`NET_ADMIN`)과 사용자 전환(`SETUID`·`SETGID`)과 bounding set 비우기(`SETPCAP`)뿐이다. 이미지의 init이 바뀌면 이 값도 함께 바꾼다.
    pub fn model_image() -> Self {
        Self {
            anchor: "/usr/local/bin/bot-anchor".into(),
            anchor_user: (10001, 10001),
            model_caps: ["NET_ADMIN", "SETGID", "SETPCAP", "SETUID"]
                .map(String::from)
                .into(),
        }
    }
}

/// 어댑터 설정. 이미지는 시작 때 로컬에서 다이제스트로 고정한다(태그가 나중에 옮겨 가도 실행은 고정한 것).
#[derive(Clone, Debug)]
pub struct ContainerConfig {
    /// `container` 실행 파일(기본 `container`)
    pub program: String,
    pub image: String,
    /// 호스트 전용(`--internal`) 망 이름
    pub network: String,
    /// 컨테이너 이름 접두사(이름의 모양 검사에 쓰인다)
    pub prefix: String,
    /// 이 호스트 인스턴스(워커)의 이름: 소유 라벨·소유권 잠금의 기준이다. 같은 기계에서 동시에 도는 어댑터마다 달라야 하고, 같으면 시작이 거절된다.
    pub instance: String,
    /// 마운트 원본이 이 폴더 아래여야 한다
    pub mount_root: PathBuf,
    pub cpus: u32,
    pub memory_mib: u32,
    /// 고정 이미지의 신뢰 진입. 없으면(그리고 `unfirewalled`가 아니면) 시작이 거절된다.
    pub entry: Option<Entry>,
    /// 방화벽 진입 없이 시작한다(시험 전용: 호스트 전용 망만으로 격리를 보는 카나리). 운영은 켜지 않는다.
    pub unfirewalled: bool,
    /// 인스턴스 소유권 잠금 폴더. **운영은 비운다**: 비우면 호스트 사용자 전용 고정 경로(`~/.local/state/bot-framework/locks`)라 작업 폴더·설정과 무관하게
    /// 같은 인스턴스 이름은 늘 같은 잠금이다. 시험만 격리한 폴더(절대 경로)를 준다.
    pub lock_dir: Option<PathBuf>,
    /// 호스트 전용 망을 새로 만들 때 쓸 사설 IPv4 부분망(`192.168.200.0/24` 꼴). 없으면 `container`가 정한다.
    /// 같은 기계에서 다른 망(다른 프로젝트·다른 인스턴스)과 부분망이 겹치면 연결이 끊기므로, 겹칠 수 있는 환경에서는 고정한다.
    /// 이미 있는 망에는 쓰이지 않는다(그 망의 부분망을 그대로 쓴다).
    pub subnet: Option<String>,
}

/// `a.b.c.d/길이` 꼴의 사설 IPv4 부분망인가(길이 16~29).
pub(super) fn subnet_ok(text: &str) -> bool {
    let Some((address, length)) = text.split_once('/') else {
        return false;
    };
    let (Ok(address), Ok(length)) = (address.parse::<std::net::Ipv4Addr>(), length.parse::<u8>())
    else {
        return false;
    };
    address.is_private() && (16..=29).contains(&length)
}

impl ContainerConfig {
    pub fn new(
        image: impl Into<String>,
        network: impl Into<String>,
        mount_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: "container".into(),
            image: image.into(),
            network: network.into(),
            prefix: "botvm".into(),
            instance: "botvm".into(),
            mount_root: mount_root.into(),
            cpus: 2,
            memory_mib: 2048,
            entry: None,
            unfirewalled: false,
            subnet: None,
            lock_dir: None,
        }
    }

    /// 이 호스트 인스턴스의 이름을 정한다(접두사도 같은 이름으로 맞춘다).
    #[must_use]
    pub fn named(mut self, instance: impl Into<String>) -> Self {
        let name = instance.into();
        self.prefix.clone_from(&name);
        self.instance = name;
        self
    }

    #[must_use]
    pub fn with_entry(mut self, entry: Entry) -> Self {
        self.entry = Some(entry);
        self
    }

    /// 시험 전용: 인스턴스 소유권 잠금 폴더를 바꾼다(위 `lock_dir`).
    #[must_use]
    pub fn with_lock_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.lock_dir = Some(dir.into());
        self
    }

    /// 망을 새로 만들 때 쓸 부분망을 고정한다(위 `subnet`).
    #[must_use]
    pub fn with_subnet(mut self, subnet: impl Into<String>) -> Self {
        self.subnet = Some(subnet.into());
        self
    }

    /// 시험 전용: 방화벽 진입 없이 시작한다.
    #[must_use]
    pub fn unfirewalled(mut self) -> Self {
        self.unfirewalled = true;
        self
    }
}

/// 명세가 안전하지 않아 컨테이너를 띄우지 않는다. 이유 문구는 진단용 고정 낱말이다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refused(pub &'static str);

/// 만든 명령: 인자들, 클라이언트 환경에만 둘 비밀 값들, 컨테이너 이름.
#[derive(Debug)]
pub struct RunCommand {
    pub args: Vec<String>,
    pub secret_env: Vec<(String, String)>,
    pub name: String,
}

pub const OWNER_LABEL: &str = "bot.owner";
pub const MANAGED_LABEL: &str = "bot.managed";
/// 이 VM을 띄운 호스트 인스턴스. 정리는 이 라벨이 정확히 같은 것만 본다.
pub const INSTANCE_LABEL: &str = "bot.instance";
/// 이 VM을 띄운 프로세스 실행(재시작마다 달라진다): 같은 인스턴스의 이전 실행이 남긴 고아를 알아본다.
pub const INCARNATION_LABEL: &str = "bot.incarnation";

/// 어느 VM인가: 모델 VM(요청 명세대로) 또는 닻 VM(고정 대기).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Model,
    Anchor,
}

/// 띄울 때 호스트가 정하는 값(모델 요청 명세가 바꾸지 못한다).
pub struct Plan<'a> {
    /// 시작 때 고정한 이미지 참조(`이름@다이제스트`)
    pub image: &'a str,
    pub incarnation: &'a str,
    pub serial: u64,
    /// 호스트가 실제로 연 릴레이(게이트웨이 IP와 포트). 모델 VM의 `ANTHROPIC_BASE_URL`은 이 주소로 고정된다.
    pub relay: Option<SocketAddr>,
}

fn token_ok(s: &str, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || extra.contains(&b)
        })
}

/// 컨테이너 안 절대 경로(`/bot`, `/work/claude`): 상위 칸·빈 칸·특수 문자 없음.
pub(crate) fn guest_ok(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 255
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != ".." && token_ok(part, &[]))
        || path == "/"
}

fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub(crate) fn secret_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"]
        .iter()
        .any(|word| upper.contains(word))
}

/// 마운트 원본: `mount_root` 아래의 실제 폴더(링크를 푼 경로)여야 한다. 허용하면 `--mount`에 쓸 경로 글을 돌려준다.
fn source_ok(root: &Path, host: &Path) -> Result<String, Refused> {
    let text = host.to_str().ok_or(Refused("mount_path"))?;
    if text
        .chars()
        .any(|c| c.is_control() || matches!(c, ',' | '"' | '\'' | '='))
    {
        return Err(Refused("mount_path"));
    }
    let real = host.canonicalize().map_err(|_| Refused("mount_missing"))?;
    let base = root.canonicalize().map_err(|_| Refused("mount_root"))?;
    if !real.is_dir() || !real.starts_with(&base) {
        return Err(Refused("mount_outside_root"));
    }
    real.to_str()
        .filter(|s| {
            !s.chars()
                .any(|c| c.is_control() || matches!(c, ',' | '"' | '\'' | '='))
        })
        .map(str::to_owned)
        .ok_or(Refused("mount_path"))
}

/// 이 시도의 소유 표지: 프로필·run·세대·claim이 모두 같은 것만 같은 소유자다.
pub fn owner_of(spec: &ModelSpec) -> String {
    let a = &spec.attempt;
    format!(
        "{}.{}.g{}.c{}",
        a.profile.0,
        a.run.0,
        a.generation.0,
        a.claim.map_or(0, |c| c.0)
    )
}

/// 컨테이너 이름: 접두사 + (소유 표지·실행)의 해시 12자 + 순번. 소문자·숫자·`-`, 63자 이내.
pub fn name_of(prefix: &str, owner: &str, incarnation: &str, serial: u64) -> String {
    let hash: String = Sha256::digest(format!("{owner}|{incarnation}").as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{prefix}-{hash}-{serial}")
}

/// 이 접두사로 만든 이름인가: `접두사-<16진 12자>-<순번>`과 정확히 같은 모양이어야 한다
/// (`botvm`과 `botvm-secondary-…`처럼 접두사끼리 포함 관계여도 다른 어댑터의 이름은 걸리지 않는다).
pub fn is_ours(prefix: &str, id: &str) -> bool {
    let Some(rest) = id
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    let mut parts = rest.split('-');
    let (Some(hash), Some(serial), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    hash.len() == 12
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !serial.is_empty()
        && serial.bytes().all(|b| b.is_ascii_digit())
}

/// 라벨 값: 소유 표지를 이름에 쓸 수 있는 글자로만 옮긴다(알아보는 데 쓰는 값이라 결정적이다).
pub fn label_value(owner: &str) -> String {
    owner
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 설정이 안전한 낱말인가(접두사·망 이름·이미지). 파일 이름·명령줄에 그대로 쓰이므로 `build`와 닻이 먼저 확인한다.
pub(super) fn valid(config: &ContainerConfig) -> Result<(), Refused> {
    if !token_ok(&config.prefix, &[])
        || !token_ok(&config.instance, &[])
        || !token_ok(&config.network, &[])
    {
        return Err(Refused("config"));
    }
    if config.subnet.as_deref().is_some_and(|s| !subnet_ok(s)) {
        return Err(Refused("config"));
    }
    if config
        .lock_dir
        .as_ref()
        .is_some_and(|dir| !dir.is_absolute())
    {
        return Err(Refused("config"));
    }
    if let Some(entry) = &config.entry {
        let caps_ok = entry.model_caps.iter().all(|cap| {
            !cap.is_empty()
                && cap.len() <= 32
                && cap != "ALL"
                && cap.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
        });
        if !guest_ok(&entry.anchor)
            || !caps_ok
            || entry.anchor_user.0 == 0
            || entry.anchor_user.1 == 0
        {
            return Err(Refused("entry"));
        }
    }
    if config.image.is_empty()
        || config.image.len() > 255
        || config.image.starts_with('-')
        || config
            .image
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(Refused("image"));
    }
    Ok(())
}

/// 닻 VM의 소유 표지(인스턴스마다 하나).
pub fn anchor_owner(config: &ContainerConfig) -> String {
    format!("anchor.{}.g0.c0", config.instance)
}

/// 두 VM이 함께 쓰는 앞부분: 보안 고정 플래그·망·자원·라벨·이름. 뒤에 역할별 인자가 온다.
fn base(config: &ContainerConfig, plan: &Plan<'_>, owner: &str, name: &str) -> Vec<String> {
    [
        "run",
        "--rm",
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--cap-drop",
        "ALL",
        "--init",
        "--no-dns",
        "--network",
        &config.network,
        "-c",
        &config.cpus.to_string(),
        "-m",
        &format!("{}M", config.memory_mib),
        "--label",
        &format!("{OWNER_LABEL}={}", label_value(owner)),
        "--label",
        &format!("{MANAGED_LABEL}=1"),
        "--label",
        &format!("{INSTANCE_LABEL}={}", label_value(&config.instance)),
        "--label",
        &format!("{INCARNATION_LABEL}={}", label_value(plan.incarnation)),
        "--name",
        name,
    ]
    .map(String::from)
    .into()
}

/// 닻 VM: 아무것도 마운트하지 않고 기다리기만 한다. 진입이 있으면 이미지의 고정 대기 진입(비root·capability 0·URL 없음)이고,
/// 없으면(시험 전용) 이미지의 `sleep`이다.
pub fn build_anchor(config: &ContainerConfig, plan: &Plan<'_>) -> Result<RunCommand, Refused> {
    valid(config)?;
    let owner = anchor_owner(config);
    let name = name_of(&config.prefix, &owner, plan.incarnation, plan.serial);
    let mut args = base(config, plan, &owner, &name);
    args.extend(["-w".into(), "/".into()]);
    if let Some(entry) = &config.entry {
        args.extend([
            "--entrypoint".into(),
            entry.anchor.clone(),
            "--user".into(),
            format!("{}:{}", entry.anchor_user.0, entry.anchor_user.1),
        ]);
    }
    args.push(plan.image.to_owned());
    if config.entry.is_none() {
        args.extend(["sleep".into(), "2147483647".into()]);
    }
    Ok(RunCommand {
        args,
        secret_env: Vec::new(),
        name,
    })
}

/// 모델 VM. 릴레이는 호스트가 실제로 연 주소로 고정한다: 명세의 환경은 그 주소를 바꾸지 못한다.
pub fn build(
    config: &ContainerConfig,
    spec: &ModelSpec,
    plan: &Plan<'_>,
) -> Result<RunCommand, Refused> {
    valid(config)?;
    if spec.program.is_empty()
        || spec.program.starts_with('-')
        || spec.program.len() > 255
        || spec.program.chars().any(char::is_control)
        // 이미지의 init은 `claude <인자…>`만 받아 고정 Claude를 exec한다: 다른 프로그램은 시작하지 않는다
        || (config.entry.is_some() && spec.program != "claude")
    {
        return Err(Refused("program"));
    }
    if spec
        .args
        .iter()
        .any(|a| a.contains('\0') || a.len() > 1 << 20)
    {
        return Err(Refused("args"));
    }
    if !guest_ok(&spec.cwd) {
        return Err(Refused("cwd"));
    }
    let relay = match plan.relay {
        Some(SocketAddr::V4(relay)) if relay.port() != 0 => relay,
        _ => return Err(Refused("relay")),
    };
    let owner = owner_of(spec);
    let name = name_of(&config.prefix, &owner, plan.incarnation, plan.serial);
    let mut args = base(config, plan, &owner, &name);
    // 진입이 있으면 이미지의 init이 시작할 때 필요한 최소 capability만 더한다(init이 곧 모두 버린다)
    if let Some(entry) = &config.entry {
        for cap in &entry.model_caps {
            args.extend(["--cap-add".into(), cap.clone()]);
        }
    }
    args.extend(["-w".into(), spec.cwd.clone()]);
    if spec.stdin.is_some() {
        args.push("-i".into());
    }
    let mut guests = Vec::new();
    for (mount, readonly) in spec
        .readonly_mounts
        .iter()
        .map(|m| (m, true))
        .chain(spec.scratch.iter().map(|m| (m, false)))
    {
        if !guest_ok(&mount.guest) || mount.guest == "/" || guests.contains(&mount.guest) {
            return Err(Refused("mount_target"));
        }
        guests.push(mount.guest.clone());
        let source = source_ok(&config.mount_root, &mount.host)?;
        args.push("--mount".into());
        args.push(format!(
            "type=bind,source={source},target={}{}",
            mount.guest,
            if readonly { ",readonly" } else { "" }
        ));
    }
    let canonical = format!("http://{relay}");
    let mut env: Vec<(String, String)> = Vec::new();
    for (key, value) in &spec.env {
        if !env_name_ok(key)
            || value.contains('\0')
            || env.iter().any(|(seen, _)| seen == key)
            // 릴레이 주소는 호스트 전용 이름이다: 명세가 정하지 못하고, 같은 이름의 URL은 호스트가 연 주소와 같아야 한다
            || matches!(key.as_str(), "BOT_RELAY_IP" | "BOT_RELAY_PORT")
            || (key == "ANTHROPIC_BASE_URL" && value.trim_end_matches('/') != canonical)
        {
            return Err(Refused(
                if key.starts_with("BOT_RELAY") || key == "ANTHROPIC_BASE_URL" {
                    "relay"
                } else {
                    "env"
                },
            ));
        }
        env.push((key.clone(), value.clone()));
    }
    env.retain(|(key, _)| key != "ANTHROPIC_BASE_URL");
    env.extend([
        ("BOT_RELAY_IP".to_owned(), relay.ip().to_string()),
        ("BOT_RELAY_PORT".to_owned(), relay.port().to_string()),
        ("ANTHROPIC_BASE_URL".to_owned(), canonical),
    ]);
    let mut secret_env = Vec::new();
    for (key, value) in &env {
        args.push("-e".into());
        if secret_like(key) {
            args.push(key.clone());
            secret_env.push((key.clone(), value.clone()));
        } else {
            args.push(format!("{key}={value}"));
        }
    }
    args.push(plan.image.to_owned());
    args.push(spec.program.clone());
    args.extend(spec.args.iter().cloned());
    Ok(RunCommand {
        args,
        secret_env,
        name,
    })
}

/// 이름이 가리키는 이미지를 다이제스트로 고정한다(`이름:태그` → `이름@sha256:…`). 이미 다이제스트면 같아야 한다.
pub(super) fn pin(image: &str, digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    if let Some((_, given)) = image.split_once('@') {
        return (given == digest).then(|| image.to_owned());
    }
    let name = match image.rfind(':') {
        Some(at) if !image[at..].contains('/') => &image[..at],
        _ => image,
    };
    Some(format!("{name}@{digest}"))
}

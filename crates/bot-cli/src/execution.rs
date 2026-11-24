//! 호스트 설정의 실행 절(`providers`·`sandbox`, docs/architecture.md «호스트 설정과 역할 조립»)과 권한 지문.
//!
//! - `providers`: 모델 별칭 → 실행 방법. `demo`(결정적 Step, 외부 키·VM 불필요) 또는 `claude-cli`(상류 `model`·HTTPS `endpoint`·프로필
//!   자격 이름 `secret`). `pi`는 격리·재개 어댑터가 준비되기 전에는 쓸 수 없다. URL에 자격·쿼리·조각을 넣을 수 없다.
//! - `sandbox.srt`: 도구를 실행하는 역할의 srt(`binary`·`work_root`, 절대 경로). `sandbox.apple`: 모델 CLI를 쓰는 워커의 VM
//!   (`image` 다이제스트 고정·`network`·선택 `relay_bind`는 그 호스트 전용 IPv4 주소 — 생략하면 닻이 확인한 주소, 전체 인터페이스·loopback 금지 — 와
//!   호스트별 선택 `work_root`·`instance`·`binary`).
//!
//! 이 절의 **모양**은 모든 명령이 검사하지만 **필요 여부**는 역할이 정한다(런타임 역할만 실제 providers·샌드박스가 필요하다).
//! 권한이 달라지는 설정(모델 허용·상류 목적지와 자격 이름·egress·서버 설치 판본·VM 이미지와 망)은 활성화 판본에 묶는다 —
//! 구판 워커가 철회된 호스트 목적지를 쓰지 못하게 한다. 자격 **값**과 호스트별 경로(`work_root`·설치 위치)·주소·인스턴스 이름은 넣지 않는다.
use bot_core::id::{Revision, SecretName};
use bot_profile::HostCatalog;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

/// 모델 별칭의 실행 방법
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    /// 외부 호출 없는 결정적 데모 모델
    Demo,
    /// 상류 서비스로 가는 Claude CLI. 서비스 키는 브로커가 상류에만 보낸다(`secret`은 프로필 자격 **이름**).
    ClaudeCli {
        model: String,
        endpoint: String,
        secret: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Srt {
    pub binary: PathBuf,
    pub work_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Apple {
    /// 다이제스트로 고정한 이미지(`…@sha256:<64자리>`)
    pub image: String,
    pub network: String,
    /// 릴레이가 듣는 그 호스트 전용 인터페이스 주소(전체 인터페이스·loopback·IPv6 금지). 생략하면 닻이 망에서 확인한 게이트웨이 IP에서 OS가 빈 포트를
    /// 정한다. 적었으면 그 IP가 확인한 게이트웨이 IP와 정확히 같을 때만 시작한다(포트 0이면 OS가 정한다).
    pub relay_bind: Option<SocketAddr>,
    /// 호출별 입력·작업 폴더를 만드는 호스트 폴더(절대 경로). 생략하면 설치 폴더 옆 `work/models`.
    pub work_root: Option<PathBuf>,
    /// 같은 기계에서 도는 워커마다 다른 이름(소유 라벨·VM 이름 접두사·소유권 잠금의 기준). 생략하면 `botvm`.
    pub instance: Option<String>,
    /// `container` 실행 파일(절대 경로). 생략하면 PATH의 `container`.
    pub binary: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sandbox {
    pub srt: Option<Srt>,
    pub apple: Option<Apple>,
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path}: 객체여야 해요"))
}

fn only(map: &Map<String, Value>, allowed: &[&str], path: &str) -> Result<(), String> {
    match map.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(unknown) => Err(format!("{path}: 알 수 없는 키예요: {unknown}")),
        None => Ok(()),
    }
}

fn text<'a>(map: &'a Map<String, Value>, key: &str, path: &str) -> Result<&'a str, String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && !value.contains('\0'))
        .ok_or_else(|| format!("{path}.{key}: 비어 있지 않은 문자열이 필요해요"))
}

fn absolute(map: &Map<String, Value>, key: &str, path: &str) -> Result<PathBuf, String> {
    let value = text(map, key, path)?;
    let path_buf = PathBuf::from(value);
    if path_buf.is_absolute() && value != "/" {
        Ok(path_buf)
    } else {
        Err(format!("{path}.{key}: `/`가 아닌 절대 경로여야 해요"))
    }
}

/// 상류 주소: `https://호스트[:포트][/경로]`. 자격(`user@`)·쿼리·조각은 금지(자격은 SecretStore로만 준다). 호스트와 포트는 실제로
/// 보내는 쪽(브로커의 egress)과 같은 규칙(`HostPort::parse`)으로 해석돼야 한다 — 포트가 범위 밖(70000)·0이거나 호스트를 정규화할
/// 수 없으면 기동 전에 거절한다. 포트를 생략하면 443이다.
fn endpoint_ok(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return false;
    };
    if !endpoint.is_ascii() || endpoint.contains(['?', '#', '@', ' ', '\\']) {
        return false;
    }
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    let target = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:443")
    };
    bot_broker::resolve::HostPort::parse(&target).is_some()
}

/// 다이제스트로 고정한 이미지: `<이름>@sha256:<소문자 16진 64자리>`
fn image_pinned(image: &str) -> bool {
    match image.rsplit_once("@sha256:") {
        Some((name, digest)) => {
            !name.is_empty()
                && !name.contains(char::is_whitespace)
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        None => false,
    }
}

/// `providers` 절. 별칭은 이름 규칙을 따르고 모르는 키·`pi`는 거절한다.
pub fn parse_providers(value: &Value) -> Result<BTreeMap<String, Provider>, String> {
    let mut out = BTreeMap::new();
    for (alias, entry) in object(value, "providers")? {
        let path = format!("providers.{alias}");
        if !bot_core::id::is_name(alias) {
            return Err(format!(
                "{path}: 별칭이 이름 규칙(소문자·숫자·하이픈)에 맞지 않아요"
            ));
        }
        let fields = object(entry, &path)?;
        let provider = match text(fields, "kind", &path)? {
            "demo" => {
                only(fields, &["kind"], &path)?;
                Provider::Demo
            }
            "claude-cli" => {
                only(fields, &["kind", "model", "endpoint", "secret"], &path)?;
                let endpoint = text(fields, "endpoint", &path)?;
                if !endpoint_ok(endpoint) {
                    return Err(format!(
                        "{path}.endpoint: `https://호스트[:포트][/경로]`여야 해요(자격·쿼리·조각 금지)"
                    ));
                }
                let secret = text(fields, "secret", &path)?;
                if SecretName::parse(secret).is_none() {
                    return Err(format!("{path}.secret: 자격 이름 규칙에 맞지 않아요"));
                }
                Provider::ClaudeCli {
                    model: text(fields, "model", &path)?.to_owned(),
                    endpoint: endpoint.to_owned(),
                    secret: secret.to_owned(),
                }
            }
            "pi" => {
                return Err(format!(
                    "{path}.kind: pi는 격리·재개 어댑터가 준비되기 전에는 쓸 수 없어요"
                ));
            }
            other => {
                return Err(format!(
                    "{path}.kind: {other}는 지원하지 않아요(demo · claude-cli)"
                ));
            }
        };
        out.insert(alias.clone(), provider);
    }
    Ok(out)
}

/// `sandbox` 절(`srt`·`apple`, 둘 다 선택). 모양만 검사하고 실제 실행 가능 여부는 그 역할이 시작할 때 확인한다.
pub fn parse_sandbox(value: &Value) -> Result<Sandbox, String> {
    let top = object(value, "sandbox")?;
    only(top, &["srt", "apple"], "sandbox")?;
    let srt = match top.get("srt") {
        Some(entry) => {
            let fields = object(entry, "sandbox.srt")?;
            only(fields, &["binary", "work_root"], "sandbox.srt")?;
            Some(Srt {
                binary: absolute(fields, "binary", "sandbox.srt")?,
                work_root: absolute(fields, "work_root", "sandbox.srt")?,
            })
        }
        None => None,
    };
    let apple = match top.get("apple") {
        Some(entry) => {
            let fields = object(entry, "sandbox.apple")?;
            only(
                fields,
                &[
                    "image",
                    "network",
                    "relay_bind",
                    "work_root",
                    "instance",
                    "binary",
                ],
                "sandbox.apple",
            )?;
            let image = text(fields, "image", "sandbox.apple")?;
            if !image_pinned(image) {
                return Err(
                    "sandbox.apple.image: `이름@sha256:<64자리 16진>`처럼 다이제스트로 고정해야 해요"
                        .to_owned(),
                );
            }
            let relay_bind = match fields.get("relay_bind") {
                None => None,
                Some(_) => {
                    let bind: SocketAddr = text(fields, "relay_bind", "sandbox.apple")?
                        .parse()
                        .map_err(|_| {
                            "sandbox.apple.relay_bind: `주소:포트` 형식이어야 해요".to_owned()
                        })?;
                    if bind.ip().is_unspecified() || bind.ip().is_loopback() || !bind.is_ipv4() {
                        return Err(
                            "sandbox.apple.relay_bind: 전체 인터페이스(0.0.0.0·::)·loopback·IPv6는 쓸 수 없어요 — 그 호스트 전용 인터페이스의 IPv4 주소를 적거나, 생략하면 닻이 확인한 주소를 써요"
                                .to_owned(),
                        );
                    }
                    Some(bind)
                }
            };
            let work_root = match fields.get("work_root") {
                None => None,
                Some(_) => Some(absolute(fields, "work_root", "sandbox.apple")?),
            };
            let binary = match fields.get("binary") {
                None => None,
                Some(_) => Some(absolute(fields, "binary", "sandbox.apple")?),
            };
            let instance = match fields.get("instance") {
                None => None,
                Some(_) => {
                    let name = text(fields, "instance", "sandbox.apple")?;
                    if !bot_gateway::identity::is_alias(name) {
                        return Err(
                            "sandbox.apple.instance: 63자 이내 소문자 kebab-case여야 해요"
                                .to_owned(),
                        );
                    }
                    Some(name.to_owned())
                }
            };
            Some(Apple {
                image: image.to_owned(),
                network: text(fields, "network", "sandbox.apple")?.to_owned(),
                relay_bind,
                work_root,
                instance,
                binary,
            })
        }
        None => None,
    };
    Ok(Sandbox { srt, apple })
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn count(hash: &mut Sha256, n: usize) {
    hash.update((n as u64).to_be_bytes());
}

/// 권한이 달라지는 호스트 설정의 지문(자격 값·호스트별 경로·주소 제외). `gateway`는 게이트웨이의 신원·입구 매핑 지문이다.
/// 프로필별 모델 허용·모델 실행 방법(상류 목적지와 자격 이름)·egress 상한·서버 별칭과 설치 판본·VM 이미지와 망 이름이 들어간다.
pub fn authority_fingerprint(
    gateway: &[u8; 32],
    catalog: &HostCatalog,
    providers: &BTreeMap<String, Provider>,
    sandbox: &Sandbox,
    revisions: &BTreeMap<String, Revision>,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-v1\0");
    hash.update(gateway);
    for (tag, map) in [("models", &catalog.models), ("egress", &catalog.egress)] {
        hash.update(tag.as_bytes());
        count(&mut hash, map.len());
        for (profile, items) in map {
            field(&mut hash, profile.as_bytes());
            count(&mut hash, items.len());
            for item in items {
                field(&mut hash, item.as_bytes());
            }
        }
    }
    hash.update(b"providers");
    count(&mut hash, providers.len());
    for (alias, provider) in providers {
        field(&mut hash, alias.as_bytes());
        match provider {
            Provider::Demo => hash.update([0]),
            Provider::ClaudeCli {
                model,
                endpoint,
                secret,
            } => {
                hash.update([1]);
                for part in [model, endpoint, secret] {
                    field(&mut hash, part.as_bytes());
                }
            }
        }
    }
    hash.update(b"servers");
    count(&mut hash, catalog.servers.len());
    for alias in &catalog.servers {
        field(&mut hash, alias.as_bytes());
        match revisions.get(alias) {
            Some(revision) => {
                hash.update([1]);
                field(&mut hash, revision.0.as_bytes());
            }
            None => hash.update([0]),
        }
    }
    hash.update(b"vm");
    match &sandbox.apple {
        Some(apple) => {
            hash.update([1]);
            field(&mut hash, apple.image.as_bytes());
            // 모델 VM이 붙는 호스트 전용 망도 격리의 일부다: 다른 망으로 바뀌면 활성화 사건이다
            field(&mut hash, apple.network.as_bytes());
        }
        None => hash.update([0]),
    }
    hash.finalize().into()
}

#[cfg(test)]
mod tests;

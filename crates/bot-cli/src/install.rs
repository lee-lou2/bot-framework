//! 도구 서버 설치본. 호스트 설정 `servers`(별칭 → 설치 폴더·실행 명령, docs/plugins.md «도구 서버 설치»)를 읽어 판본을 계산하고,
//! 도구를 실행하는 역할은 패키지를 읽기 전용 설치본으로 복사한다. 판본 계산은 `bot_core::install::server_revision` 하나뿐이다.
//!
//! - 읽기(`measure`): 승인 결정만 하는 역할(gateway·admin)과 `bot validate`. 파일을 한 번 읽어 해시만 계산한다(복사 없음).
//! - 설치(`install`): 도구를 실행하는 역할. 읽는 바이트를 그대로 읽기 전용 설치본에 쓰면서 해시를 계산하고(**쓴 바이트에서 계산**),
//!   실행은 그 설치본에서만 한다(원래 경로를 다시 열지 않는다). 설치본은 `<기준>/<판본>/`이라 같은 내용·명령이면 같은 폴더이고
//!   (여러 프로세스·재시작이 공유), 이미 있으면 설치본 바이트를 다시 해시해 판본과 같을 때만 쓴다(바뀌었으면 거절).
//! - 링크·특수 파일·UTF-8이 아닌 이름은 거절한다. 설치 위치·수정 시각은 판본에 들어가지 않는다.
use bot_broker::mcp::remote::Remote;
use bot_core::{
    id::{Revision, SecretName},
    install::{InstallEntry, ServerState, server_revision},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

mod place;
mod walk;

#[cfg(test)]
use place::remove_tree;
use place::{base_dir, install_one};
use walk::walk;

const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 4096;

/// 호스트 설정의 서버 한 항목
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    /// 코드 전용 설치 폴더(절대 경로)
    pub root: PathBuf,
    /// `command[0]`은 `root` 안의 실행 파일(상대 경로), 나머지는 셸 해석 없이 전달할 인자
    pub command: Vec<String>,
    /// 호출 사이에 남는 상태가 필요한가. `required`는 다른 설치 판본으로 자동 결합돼 승인·활성화에 묶인다(기본 `ephemeral`).
    pub state: ServerState,
}

fn relative_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// `servers` 절 전체: 로컬 설치본(stdio)과 원격(Streamable HTTP, `transport: "http"`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Servers {
    pub local: BTreeMap<String, ServerSpec>,
    pub remote: BTreeMap<String, Remote>,
}

/// `servers` 절을 읽는다: `transport`가 `http`인 항목은 원격(`url`·선택 `auth`), 그 밖(생략·`stdio`)은 로컬 설치본이다.
pub fn parse_all(value: &Value) -> Result<Servers, String> {
    let Value::Object(map) = value else {
        return parse_servers(value).map(|local| Servers {
            local,
            ..Servers::default()
        });
    };
    let (mut local, mut remote) = (serde_json::Map::new(), BTreeMap::new());
    for (alias, entry) in map {
        if entry.get("transport").and_then(Value::as_str) == Some("http") {
            if !bot_core::id::is_name(alias) {
                return Err(format!(
                    "servers.{alias}: 별칭이 이름 규칙(소문자·숫자·하이픈)에 맞지 않아요"
                ));
            }
            remote.insert(alias.clone(), parse_remote(alias, entry)?);
        } else {
            local.insert(alias.clone(), entry.clone());
        }
    }
    Ok(Servers {
        local: parse_servers(&Value::Object(local))?,
        remote,
    })
}

/// 원격 서버 한 항목: `{"transport": "http", "url": "https://…", "auth": {"kind": "bearer", "secret": "<자격 이름>"}}`(`auth`는 생략할 수 있다).
fn parse_remote(alias: &str, entry: &Value) -> Result<Remote, String> {
    let fields = entry
        .as_object()
        .ok_or_else(|| format!("servers.{alias}: 객체여야 해요"))?;
    if let Some(unknown) = fields
        .keys()
        .find(|k| !matches!(k.as_str(), "transport" | "url" | "auth"))
    {
        return Err(format!("servers.{alias}: 알 수 없는 키예요: {unknown}"));
    }
    let url = fields
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("servers.{alias}.url: 주소 문자열이 필요해요"))?;
    let bearer = match fields.get("auth") {
        None => None,
        Some(Value::Object(auth))
            if auth.get("kind").and_then(Value::as_str) == Some("bearer")
                && auth.keys().all(|k| matches!(k.as_str(), "kind" | "secret")) =>
        {
            let name = auth
                .get("secret")
                .and_then(Value::as_str)
                .and_then(SecretName::parse)
                .filter(|name| !bot_broker::private::reserved_secret(name))
                .ok_or_else(|| {
                    format!("servers.{alias}.auth.secret: 자격 이름(소문자·숫자·하이픈)이 필요해요")
                })?;
            Some(name)
        }
        Some(_) => {
            return Err(format!(
                "servers.{alias}.auth: {{\"kind\": \"bearer\", \"secret\": \"<자격 이름>\"}} 모양이어야 해요"
            ));
        }
    };
    Remote::parse(url, bearer).map_err(|why| format!("servers.{alias}.url: {why}"))
}

/// `servers` 절을 읽는다. 별칭을 키로 한 객체여야 하고, 옛 별칭 배열과 수동 판본(`revision`)은 받지 않는다.
pub fn parse_servers(value: &Value) -> Result<BTreeMap<String, ServerSpec>, String> {
    let Value::Object(map) = value else {
        return Err(
            "servers: 별칭을 키로 한 객체여야 해요 — 예: {\"agenda\": {\"root\": \"/opt/bot/servers/agenda\", \"command\": [\"bin/server\", \"--stdio\"]}} (옛 별칭 배열은 바꿔 주세요)"
                .to_owned(),
        );
    };
    let mut out = BTreeMap::new();
    for (alias, entry) in map {
        if !bot_core::id::is_name(alias) {
            return Err(format!(
                "servers.{alias}: 별칭이 이름 규칙(소문자·숫자·하이픈)에 맞지 않아요"
            ));
        }
        let Value::Object(fields) = entry else {
            return Err(format!(
                "servers.{alias}: {{\"root\": …, \"command\": […]}} 객체여야 해요"
            ));
        };
        // `transport: "http"`는 `parse_all`이 원격으로 가른다. 여기에 오는 것은 로컬 설치본이라 `stdio`(기본)만 쓸 수 있다.
        if fields.get("transport").is_some_and(|t| t != "stdio") {
            return Err(format!(
                "servers.{alias}.transport: \"stdio\"(기본) 또는 \"http\"여야 해요"
            ));
        }
        if let Some(unknown) = fields
            .keys()
            .find(|k| !matches!(k.as_str(), "root" | "command" | "state" | "transport"))
        {
            let hint = if unknown == "revision" {
                " (판본은 설치본의 내용에서 자동으로 계산해요)"
            } else {
                ""
            };
            return Err(format!(
                "servers.{alias}: 알 수 없는 키예요: {unknown}{hint}"
            ));
        }
        let root = fields
            .get("root")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("servers.{alias}.root: 절대 경로 문자열이 필요해요"))?;
        if root.contains('\0') || !Path::new(root).is_absolute() || root == "/" {
            return Err(format!(
                "servers.{alias}.root: `/`가 아닌 절대 경로여야 해요"
            ));
        }
        let command = fields
            .get("command")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| {
                format!("servers.{alias}.command: 비어 있지 않은 문자열 배열이 필요해요")
            })?;
        if command.len() > MAX_ARGS {
            return Err(format!(
                "servers.{alias}.command: 인자가 너무 많아요(최대 {MAX_ARGS})"
            ));
        }
        let command: Vec<String> = command
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|text| !text.contains('\0') && text.len() <= MAX_ARG_BYTES)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        format!("servers.{alias}.command: 문자열 배열이어야 해요(NUL 없이 {MAX_ARG_BYTES}바이트 이하)")
                    })
            })
            .collect::<Result<_, _>>()?;
        if !relative_ok(&command[0]) {
            return Err(format!(
                "servers.{alias}.command[0]: root 안의 상대 경로여야 해요(`/`로 시작하거나 `..`이 들어가면 안 돼요)"
            ));
        }
        let state = match fields.get("state") {
            None => ServerState::default(),
            Some(Value::String(text)) if text == "ephemeral" => ServerState::Ephemeral,
            Some(Value::String(text)) if text == "required" => ServerState::Required,
            Some(_) => {
                return Err(format!(
                    "servers.{alias}.state: \"ephemeral\"(기본) 또는 \"required\"여야 해요"
                ));
            }
        };
        out.insert(
            alias.clone(),
            ServerSpec {
                root: PathBuf::from(root),
                command,
                state,
            },
        );
    }
    Ok(out)
}

fn revision_of(
    alias: &str,
    spec: &ServerSpec,
    entries: &BTreeMap<String, InstallEntry>,
) -> Result<Revision, String> {
    server_revision(&spec.command, entries, spec.state).map_err(|_| {
        format!(
            "servers.{alias}: command[0]({})이 root 안의 실행 가능한 일반 파일이어야 해요",
            spec.command[0]
        )
    })
}

/// 로컬 설치본과 원격 서버의 판본 전부(원격은 주소·접속 방식·자격 이름에서 계산한다). 승인 결정과 활성화 지문이 쓴다.
pub fn measure_all(
    local: &BTreeMap<String, ServerSpec>,
    remote: &BTreeMap<String, Remote>,
) -> Result<BTreeMap<String, Revision>, String> {
    let mut out = measure(local)?;
    out.extend(
        remote
            .iter()
            .map(|(alias, remote)| (alias.clone(), Revision(remote.revision()))),
    );
    Ok(out)
}

/// 설치하지 않고 판본만 계산한다(승인 결정만 하는 역할·`bot validate`).
pub fn measure(specs: &BTreeMap<String, ServerSpec>) -> Result<BTreeMap<String, Revision>, String> {
    let mut out = BTreeMap::new();
    for (alias, spec) in specs {
        let (entries, _) =
            walk(&spec.root, None, None).map_err(|e| format!("servers.{alias}: {e}"))?;
        out.insert(alias.clone(), revision_of(alias, spec, &entries)?);
    }
    Ok(out)
}

/// 읽기 전용 설치본. 도구 실행은 `dir` 안의 `argv`로만 한다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installed {
    pub revision: Revision,
    /// 이 서버가 요구하는 상태(판본에 이미 반영돼 있다)
    pub state: ServerState,
    /// 설치본 폴더(`<기준>/<판본>`). 읽기 전용이다.
    pub dir: PathBuf,
    /// 실행 명령: 첫 값은 설치본 안 실행 파일의 절대 경로, 나머지는 설정의 인자
    pub argv: Vec<String>,
}

/// 설치한 서버들. 이 값이 살아 있는 동안 각 판본의 설치본에 공유 사용 잠금을 쥔다(어긋난 설치본을 복구하려는 다른 기동이
/// 실행 중인 도구의 파일을 지우지 못하게 한다). 값이 사라지면(프로세스 종료) 잠금도 풀린다.
#[derive(Clone, Debug, Default)]
pub struct Installs {
    items: BTreeMap<String, Installed>,
    _locks: Vec<Arc<File>>,
}

impl Installs {
    pub fn get(&self, alias: &str) -> Option<&Installed> {
        self.items.get(alias)
    }

    pub fn aliases(&self) -> impl Iterator<Item = &String> {
        self.items.keys()
    }

    /// 별칭 → 판본(승인 결정이 조회한다)
    pub fn revisions(&self) -> BTreeMap<String, Revision> {
        self.items
            .iter()
            .map(|(alias, item)| (alias.clone(), item.revision.clone()))
            .collect()
    }
}

/// 모든 서버를 읽기 전용 설치본으로 만든다. 하나라도 읽지 못하면 Err(기동 실패). 돌려준 값이 살아 있는 동안 설치본을 쓰는 중임을
/// 잠금으로 알린다.
pub fn install(
    specs: &BTreeMap<String, ServerSpec>,
    base: &Path,
    uid: u32,
) -> Result<Installs, String> {
    base_dir(base, uid)?;
    let mut items = BTreeMap::new();
    let mut locks = Vec::new();
    for (alias, spec) in specs {
        let (installed, lock) = install_one(alias, spec, base, uid)?;
        items.insert(alias.clone(), installed);
        locks.push(Arc::new(lock));
    }
    Ok(Installs {
        items,
        _locks: locks,
    })
}

/// 설치 기준 폴더: `BOT_INSTALLS`, 없으면 `$HOME/.local/state/bot/installs`.
pub fn default_base(configured: Option<&Path>, home: Option<&str>) -> Result<PathBuf, String> {
    match (configured, home) {
        (Some(path), _) if path.is_absolute() => Ok(path.to_owned()),
        (Some(_), _) => Err("BOT_INSTALLS는 절대 경로여야 해요".to_owned()),
        (None, Some(home)) if Path::new(home).is_absolute() => {
            Ok(Path::new(home).join(".local/state/bot/installs"))
        }
        _ => {
            Err("설치본을 둘 곳을 정할 수 없어요 — BOT_INSTALLS(절대 경로)를 지정하세요".to_owned())
        }
    }
}

#[cfg(test)]
mod tests;

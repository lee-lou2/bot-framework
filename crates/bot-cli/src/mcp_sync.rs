//! `bot mcp sync <스킬 폴더>`: 스킬의 `tools.yaml`이 선언한 도구의 정의(이름·설명·입력 스키마)를 서버에서 받아 같은 폴더의 `tools.lock.yaml`에 적는다.
//! 프로필은 `input`을 생략한 도구의 입력 스키마·설명·정의 해시를 이 파일에서 읽고, 실행할 때 서버가 내세우는 정의가 해시와 다르면 호출하기 전에 막는다.
//!
//! 서버는 믿을 수 없다. 그래서:
//! - **접속하기 전에** 대상 프로필을 `validate`와 같은 로더로 확정한다(루트 목록·링크 금지·구조·`profile.yaml`의 스키마와 `id`=폴더 이름, `bot_profile::locate`). 자격은 그 프로필의
//!   보관소에서만 읽는다. 못 정하면 접속 0·자격 조회 0이다.
//! - 접속 대상은 서버 주소의 `호스트:포트` 하나이고, 선언한 **모든** 도구의 egress ∩ 프로필 상한에 정확히 있어야 한다(운영과 같은 판정·같은 전송).
//! - `tools/list`의 **모든 쪽**을 받은 뒤에만 쓴다(끝나지 않는 목록은 오류). 선언한 도구만 적고, 선언했는데 서버에 없거나 이름이 둘이면 실패한다.
//!   도구는 tools.yaml의 `id`로 묶고(잠금의 키) 서버에서는 `name`(생략하면 id)으로 찾는다. 해시는 서버가 준 `{name, description, inputSchema}` 그대로다.
//! - 쓰기 전에 모든 정의를 검사한다: **자격이 응답 어디에도(선언하지 않은 도구·키·숫자 표기·커서까지) 섞여 있지 않은가**(원격이 bearer를 되풀이하면 파일과 프로필 번들에
//!   영구히 남는다), 입력 스키마가 프로필 어휘로 컴파일되는가, `x-mcp-header` 주석이 올바른가. 자격 검사가 **먼저**이고, 진단은 고정 코드와 JSON 포인터만 싣는다(서버가
//!   고른 글을 되풀이하지 않는다). 만든 잠금은 `validate`와 같은 검사(크기·YAML 한도·잠금 모양·`tools.yaml`과의 짝)를 통과해야 쓴다. 하나라도 어긋나면 아무것도 쓰지 않는다.
//! - 출력은 결정적이다(도구 id 순·고정 필드 순서). 임시 파일에 쓴 뒤 바꿔 넣는다.
use crate::{
    load::{load_host, secret_store},
    serve::remote::ReqwestWire,
    settings::Settings,
};
use bot_broker::{
    egress::EgressPolicy,
    guard::{blocks_output, contains_credential},
    mcp::{McpError, header, http::Http},
    proxy::SystemResolver,
    resolve::HostPort,
};
use bot_core::{
    mcp::{entry_description, entry_sha256},
    port::{Error, SecretStore},
};
use bot_gateway::cli::socket::current_uid;
use bot_profile::{
    Limits, Schema, SchemaProblem,
    issue::Issue,
    locate::locate_skill,
    lock::{self, Entry},
};
use serde_json::Value;
use std::{
    collections::BTreeMap, io::Write, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc,
    time::Duration,
};

/// 서버가 한 번 답하기를 기다리는 시간.
const LIMIT: Duration = Duration::from_secs(10);

/// 진단 한 줄: 고정 코드와 위치(경로·JSON 포인터)만. 설명 글은 싣지 않는다(서버가 고른 글이 섞일 수 있다).
fn fixed(issue: &Issue) -> String {
    if issue.at.is_empty() {
        format!("  [{}] {}", issue.code, issue.path)
    } else {
        format!("  [{}] {} {}", issue.code, issue.path, issue.at)
    }
}

fn fixed_lines(issues: &[Issue]) -> String {
    issues
        .iter()
        .take(12)
        .map(fixed)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 스키마 문제를 고정 코드와 JSON 포인터로만 적는다(최대 8건).
fn schema_lines(found: &[SchemaProblem]) -> String {
    let shown: Vec<String> = found
        .iter()
        .take(8)
        .map(|p| {
            format!(
                "[{}] {}",
                p.code,
                if p.pointer.is_empty() {
                    "/"
                } else {
                    &p.pointer
                }
            )
        })
        .collect();
    let more = found.len().saturating_sub(shown.len());
    format!(
        "{}{}",
        shown.join(", "),
        if more > 0 {
            format!(" 외 {more}건")
        } else {
            String::new()
        }
    )
}

/// 값 안의 모든 글(키·문자열)과 숫자 표기에 자격이 있는가. `shapes`면 토큰 모양도 자격으로 본다.
fn exposes(value: &Value, credentials: &[Vec<u8>], shapes: bool) -> bool {
    let text = |text: &str| {
        if shapes {
            blocks_output(text, credentials)
        } else {
            contains_credential(text, credentials)
        }
    };
    match value {
        Value::String(item) => text(item),
        Value::Number(number) => text(&number.to_string()),
        Value::Array(items) => items.iter().any(|item| exposes(item, credentials, shapes)),
        Value::Object(map) => map
            .iter()
            .any(|(key, item)| text(key) || exposes(item, credentials, shapes)),
        _ => false,
    }
}

/// 서버가 내세운 정의들 중 선언한 도구(`(id, 서버 이름)`)의 항목을 검사해 잠금 항목으로 만든다. 문제는 모두 모아 돌려준다.
fn entries_of(
    declared: &[(String, String)],
    listed: &[Value],
    credentials: &[Vec<u8>],
) -> Result<BTreeMap<String, Entry>, Vec<String>> {
    // 자격 검사가 먼저다: 응답 전체(선언하지 않은 도구 포함)에 자격이 섞여 있으면 어떤 진단도 만들지 않고 멈춘다(진단에 되풀이되지 않게)
    if listed.iter().any(|tool| exposes(tool, credentials, false)) {
        return Err(vec![
            "서버 응답에 자격으로 보이는 값이 섞여 있어요 — 아무것도 기록하지 않아요. 서버를 확인하세요"
                .to_owned(),
        ]);
    }
    let (mut out, mut problems) = (BTreeMap::new(), Vec::new());
    for (id, name) in declared {
        let found: Vec<&Value> = listed
            .iter()
            .filter(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))
            .collect();
        let tool = match found.as_slice() {
            [] => {
                problems.push(format!(
                    "`{id}`: 서버가 내세우는 도구에 `{name}`이(가) 없어요"
                ));
                continue;
            }
            [one] => *one,
            _ => {
                problems.push(format!(
                    "`{id}`: 서버가 같은 이름(`{name}`)의 도구를 둘 이상 내세워요"
                ));
                continue;
            }
        };
        let (Some(description), Some(schema), Some(sha256)) = (
            entry_description(tool),
            tool.get("inputSchema").filter(|s| s.is_object()),
            entry_sha256(tool),
        ) else {
            problems.push(format!(
                "`{id}`: 정의의 모양이 올바르지 않아요(설명은 글, inputSchema는 객체여야 해요)"
            ));
            continue;
        };
        match Schema::compile_input_problems(schema.clone()) {
            Ok(compiled) if compiled.doc().get("type") != Some(&Value::from("object")) => {
                problems.push(format!(
                    "`{id}`: 입력 스키마의 최상위는 `type: object`여야 해요"
                ));
                continue;
            }
            Ok(_) => {}
            Err(found) => {
                // 진단은 고정 코드와 JSON 포인터만(스키마 안의 글을 되풀이하지 않는다)
                problems.push(format!(
                    "`{id}`: 입력 스키마를 프로필이 지원하는 어휘로 읽을 수 없어요: {}",
                    schema_lines(&found)
                ));
                continue;
            }
        }
        if let Err(why) = header::check_schema(schema) {
            problems.push(format!("`{id}`: {why}"));
            continue;
        }
        if blocks_output(name, credentials)
            || blocks_output(&description, credentials)
            || exposes(schema, credentials, true)
        {
            problems.push(format!(
                "`{id}`: 정의(설명·스키마)에 자격으로 보이는 값이 섞여 있어 기록하지 않아요 — 서버를 확인하세요"
            ));
            continue;
        }
        out.insert(
            id.clone(),
            Entry {
                name: name.clone(),
                description,
                input_schema: schema.clone(),
                sha256,
            },
        );
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// 같은 폴더의 임시 파일에 쓰고 바꿔 넣는다. 기존 파일이 일반 파일이 아니면(링크 등) 쓰지 않는다.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && !meta.is_file()
    {
        return Err(format!("{}이(가) 일반 파일이 아니에요", path.display()));
    }
    let tmp = path.with_file_name(format!(".tools.lock.yaml.tmp{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}을(를) 쓰지 못했어요: {e}", path.display())
    })
}

/// 서버가 응답하지 못한 이유를 사람이 읽을 말로.
fn describe(error: &McpError) -> String {
    match error {
        McpError::Unreached => {
            "서버에 연결하지 못했어요(주소·인증서·이름 풀이 결과·egress를 확인하세요)".into()
        }
        McpError::Status {
            status: 401 | 403, ..
        } => {
            format!(
                "서버가 인증을 거절했어요(HTTP {}) — 자격을 확인하세요",
                match error {
                    McpError::Status { status, .. } => *status,
                    _ => 0,
                }
            )
        }
        McpError::Status { status, .. } => format!("서버가 HTTP {status}로 답했어요"),
        McpError::Version => {
            "서버가 지원하는 MCP 판본이 2026-07-28·2025-11-25·2025-06-18·2025-03-26이 아니에요"
                .into()
        }
        McpError::Timeout => "서버가 시간 안에 답하지 않았어요".into(),
        McpError::Exposed => {
            "서버 응답에 자격이 섞여 있어요(도구 정의·커서·알림 어디든) — 아무것도 기록하지 않아요. 서버를 확인하세요".into()
        }
        McpError::Changed => {
            "도구 목록을 모으는 동안 서버가 목록이 바뀌었다고 알렸어요 — 다시 실행하세요".into()
        }
        other => format!("서버의 응답을 해석하지 못했어요({other:?})"),
    }
}

pub async fn run(settings: &Settings, dir: &Path) -> Result<(), String> {
    run_with(settings, dir, || secret_store(settings)).await
}

/// `run`과 같되 자격 보관소를 만드는 방법을 받는다(bearer가 필요할 때만 부른다).
pub async fn run_with(
    settings: &Settings,
    dir: &Path,
    make_store: impl FnOnce() -> Result<Arc<dyn SecretStore>, String>,
) -> Result<(), String> {
    let root = settings
        .profiles
        .as_ref()
        .ok_or("BOT_PROFILES가 필요해요(프로필 폴더 경로)")?;
    // 1) 대상 프로필과 스킬을 `validate`와 같은 로더로 정한다 — 못 정하면 아무 데도 접속하지 않고 자격도 읽지 않는다
    let located = locate_skill(root, dir, &Limits::default()).map_err(|issues| {
        format!(
            "프로필을 확정하지 못해 접속하지 않아요(비밀을 읽을 프로필을 정할 수 없어요):\n{}",
            fixed_lines(&issues)
        )
    })?;
    let profile = located.profile.0.clone();
    let text = String::from_utf8(located.tools_yaml.clone())
        .map_err(|_| "tools.yaml이 UTF-8이 아니에요".to_owned())?;
    let declared = lock::read_declared("tools.yaml", &text).map_err(|issues| {
        issues
            .iter()
            .map(|i| format!("tools.yaml {} {}", i.at, i.message))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if declared.tools.is_empty() {
        return Err("tools.yaml에 선언한 도구가 없어요".into());
    }
    // 2) 서버: 호스트가 등록한 원격 서버여야 한다
    let uid = current_uid().map_err(|e| e.to_string())?;
    let host = load_host(settings, uid)?;
    let Some(remote) = host.remote_servers.get(&declared.server) else {
        return Err(if host.servers.contains_key(&declared.server) {
            format!(
                "서버 `{}`는 로컬(stdio) 서버예요 — `bot mcp sync`는 아직 원격(`transport: \"http\"`) 서버만 지원해요",
                declared.server
            )
        } else {
            format!("호스트에 등록하지 않은 서버예요: {}", declared.server)
        });
    };
    // 3) egress: 접속 대상이 선언한 모든 도구의 (선언 ∩ 프로필 상한)에 정확히 있어야 한다
    let cap: Option<Vec<HostPort>> = host
        .catalog
        .egress
        .get(&profile)
        .map(|set| set.iter().filter_map(|t| HostPort::parse(t)).collect());
    let blocked: Vec<String> = declared
        .tools
        .iter()
        .filter(|tool| {
            let wanted: Vec<HostPort> = tool
                .egress
                .iter()
                .filter_map(|t| HostPort::parse(t))
                .collect();
            !EgressPolicy::new(&wanted, cap.as_deref()).permits(&remote.host, remote.port)
        })
        .map(|tool| tool.id.clone())
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "서버 주소 `{}`가 도구 {}의 `egress`(와 운영자 상한 `egress.{}`)에 없어요 — 정확히 그 `호스트:포트`를 적으세요",
            remote.target_text(),
            blocked.join(", "),
            profile
        ));
    }
    // 4) 자격: 대상 프로필의 보관소에서 이름으로 읽는다(값은 머리에만 쓰고 정의 검사에만 더 쓴다)
    let (mut credentials, mut bearer) = (Vec::new(), None);
    if let Some(name) = &remote.bearer {
        let store = make_store()?;
        let handle = store
            .for_profile(located.profile.clone())
            .await
            .map_err(|_| format!("프로필 {profile}의 보관소를 열지 못했어요"))?;
        let value = handle.resolve(name.clone()).await.map_err(|e| match e {
            Error::Missing => {
                format!(
                    "자격 {}이(가) 프로필 {profile}의 보관소에 없어요",
                    name.as_str()
                )
            }
            _ => format!("자격 {}을(를) 읽지 못했어요(보관소 오류)", name.as_str()),
        })?;
        let text = String::from_utf8(value.0.clone())
            .ok()
            .filter(|t| !t.is_empty() && !t.contains(['\0', '\r', '\n']))
            .ok_or_else(|| {
                format!(
                    "자격 {}의 값이 bearer로 쓸 수 없는 모양이에요",
                    name.as_str()
                )
            })?;
        credentials.push(value.0);
        bearer = Some(text);
    }
    // 5) 운영과 같은 전송·판본 협상으로 도구 정의 전체를 받는다
    let wire = Arc::new(ReqwestWire::new(Arc::new(SystemResolver))?);
    let mut http = Http::new(wire, remote.clone(), bearer, false);
    let listed = async {
        http.connect(LIMIT).await?;
        http.tools(LIMIT).await
    }
    .await;
    http.close().await;
    let listed =
        listed.map_err(|e| format!("서버에서 도구 정의를 받지 못했어요: {}", describe(&e)))?;
    // 6) 선언한 도구만 검사해 잠금 항목으로 만든다. 하나라도 어긋나면 아무것도 쓰지 않는다
    let wanted: Vec<(String, String)> = declared
        .tools
        .iter()
        .map(|t| (t.id.clone(), t.name.clone()))
        .collect();
    let entries = entries_of(&wanted, &listed, &credentials)
        .map_err(|problems| format!("tools.lock.yaml을 쓰지 않았어요:\n{}", problems.join("\n")))?;
    let rendered = lock::render(&declared.server, &entries)
        .ok_or("정의를 정규화할 수 없어요(안전 범위를 넘는 수가 있어요)")?;
    // 만든 잠금을 `validate`가 프로필을 읽을 때와 같은 검사(파일 크기·YAML 한도·잠금의 모양과 해시·tools.yaml과의 짝)로 먼저 읽어 본다: 통과해야 쓴다
    located
        .verify_lock(rendered.as_bytes(), &Limits::default())
        .map_err(|issues| {
            format!(
                "tools.lock.yaml이 `validate`를 통과하지 못해 쓰지 않았어요:\n{}",
                fixed_lines(&issues)
            )
        })?;
    let path = located.profile_dir.join(&located.skill).join(lock::FILE);
    let same = std::fs::read_to_string(&path).is_ok_and(|old| old == rendered);
    if !same {
        write_atomic(&path, &rendered)?;
    }
    println!(
        "{}: 도구 {}개의 정의를 {}",
        path.display(),
        entries.len(),
        if same {
            "확인했어요(바뀐 것 없음)"
        } else {
            "기록했어요"
        }
    );
    for (id, entry) in &entries {
        println!(
            "  {id}  {}",
            entry.sha256.get(..12).unwrap_or(&entry.sha256)
        );
    }
    eprintln!(
        "잠금 기반 도구의 설명은 모델 요청에 실립니다. 배포 전에 tools.lock.yaml의 description을 검토하세요."
    );
    Ok(())
}

#[cfg(test)]
mod tests;

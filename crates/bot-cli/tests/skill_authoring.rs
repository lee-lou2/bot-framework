//! 슈퍼 관리자 전용 `skill-creator` 스킬·스킬 CRUD·MCP 추가/제거·SOPS 암호화 파일 처리·즉시 활성화 종단 시험.
//!
//! 실제 `bot serve --role gateway,worker`(실제 PostgreSQL·실제 OpenAI HTTP 입구·실제 워커·실제 브로커·실제 `LiveManager`·실제 `ReqwestWire` + `FakeMcp`·실제 `SopsStore` 경로)를
//! 프로세스 재시작 없이 돌려 다음을 끝에서 끝까지 확인한다.
//! 1. 일반 멤버는 관리 도구(`skill.*`·`mcp.*`)를 모델 목록에서도 보지 못한다.
//! 2. 슈퍼 관리자가 자연어로 스킬 생성을 요청하면 `skill-creator` 지침에 따라 `skill.inspect`로 적합한 에이전트(`main/developer`)를 찾고 `skill.create`로 생성해 재시작 없이 다음 요청에 즉시 반영한다.
//! 3. `skill.update`·`skill.delete` 변경과 비밀 유출 시 롤백이 즉시 동작한다.
//! 4. `mcp.add`가 원격 Streamable HTTP MCP 서버를 연결하면서 토큰을 SOPS 암호화 파일(`bot.<profile>.<name>`)에만 저장(평문 파일 0건)·즉시 캐시 무효화·활성화하고, 바로 다음 요청에서 새 MCP 도구를 실제로 호출한다.
//! 5. `mcp.remove(remove_secrets: true)`가 서버·스킬·스코프·SOPS 비밀을 모두 정리하고 즉시 활성화한다.
mod mcp_http_support;
mod process_support;
mod skill_authoring_support;

use bot_broker::testing::{Era, FakeMcp, McpTool};
use bot_cli::{
    cli::Role,
    secrets::{admin::SecretAdmin, sops::SopsStore},
    serve::{self, Overrides},
    settings::Settings,
};
use mcp_http_support::serve as serve_mcp;
use process_support::{Schema, bot, free_port, my_uid, text, wait_for};
use serde_json::{Value, json};
use skill_authoring_support::{
    FakeLlm, Turn, assistant_content, chat, read_all_files_under, tool_outputs, write_fake_sops,
};
use std::{fs, net::TcpStream, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const ADMIN_KEY: &str = "sk-e2e-superadmin-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const USER_KEY: &str = "sk-e2e-regularusr-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const WEATHER_TOKEN: &str = "synthetic-weather-token-e2e-987654";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn super_admin_skill_creator_and_mcp_lifecycle_work_live_without_restart() {
    let Some(schema) = Schema::create() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("skill-e2e-")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    let profile = profiles.join("personal");
    fs::create_dir_all(profile.join("agents/main/agents/developer")).unwrap();

    fs::write(
        profile.join("profile.yaml"),
        r#"version: 1
id: personal
timezone: Asia/Seoul
model: assistant
roles:
  admin: [run, skill.inspect, skill.create, skill.update, skill.delete, mcp.add, mcp.remove]
  member: [run, skill.inspect, skill.create, skill.update, skill.delete, mcp.add, mcp.remove]
members:
  - {identity: "local:owner", roles: [admin]}
  - {identity: "http:openai/admin", roles: [admin]}
  - {identity: "http:openai/regular", roles: [member]}
super_admins: ["local:owner", "http:openai/admin"]
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: assistant, adapter: http, endpoint: openai, model: assistant, agent: main}
  - {id: developer, adapter: http, endpoint: openai, model: developer, agent: main/developer}
secrets: []
policy:
  scopes: [run, skill.inspect, skill.create, skill.update, skill.delete, mcp.add, mcp.remove]
  agents: {}
  approve: []
"#,
    )
    .unwrap();
    fs::write(
        profile.join("agents/main/AGENT.md"),
        "---\ndescription: \"팀 코디네이터 — 요청을 분류하고 스킬과 MCP를 관리한다\"\n---\n슈퍼 관리자 요청에 따라 스킬과 MCP를 관리한다.\n",
    )
    .unwrap();
    fs::write(
        profile.join("agents/main/agents/developer/AGENT.md"),
        "---\ndescription: \"개발자 에이전트 — 코드 리뷰·테스트·구현 작업을 전담한다\"\n---\n코드 품질과 보안 관점에서 개발 작업을 수행한다.\n",
    )
    .unwrap();

    // 실제 `bot add skill skill-creator --agent main`으로 스킬 생성 스킬을 배치한다
    let added = bot(
        &[],
        &[
            "add",
            "skill",
            "skill-creator",
            "--agent",
            "main",
            "--dir",
            profile.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", text(&added));

    // 가짜 OpenAI LLM 상류와 원격 Streamable HTTP MCP 서버(Bearer 인증 필수)
    let llm = FakeLlm::start().await;
    let forecast_schema = json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
        "additionalProperties": false
    });
    let mut fake_mcp = FakeMcp::new(
        Era::Modern,
        vec![McpTool::new(
            "get-forecast",
            "도시의 예보 조회",
            forecast_schema,
        )],
    );
    fake_mcp.token = Some(WEATHER_TOKEN.into());
    let mcp_server = serve_mcp(fake_mcp).await;

    // 가짜 SOPS 암호화 파일(초기에는 HTTP API 키 2개만 저장)
    let sops_file = dir.path().join("secrets.sops.json");
    fs::write(
        &sops_file,
        json!({
            "bot": {
                "personal": {
                    "admin-api-key": ADMIN_KEY,
                    "regular-api-key": USER_KEY
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let sops_prog = write_fake_sops(dir.path());
    let sops_store = Arc::new(SopsStore::with_program(sops_file.clone(), sops_prog));
    let secret_admin = Arc::new(SecretAdmin::from_sops(sops_store.clone()));

    let http_port = free_port();
    let socket = dir.path().join("run/gateway.sock");
    let host_path = dir.path().join("host.json");
    fs::write(
        &host_path,
        json!({
            "version": 1,
            "models": {"personal": ["assistant"]},
            "super_admins": {"personal": ["local:owner", "http:openai/admin"]},
            "providers": {
                "assistant": {
                    "kind": "openai-api",
                    "model": "fake-llm",
                    "endpoint": llm.endpoint()
                }
            },
            "gateway": {
                "cli": {"accounts": {"owner": {"uid": my_uid()}}},
                "http": {
                    "listen": format!("127.0.0.1:{http_port}"),
                    "endpoints": {
                        "openai": {
                            "kind": "openai",
                            "wait_ms": 60000,
                            "max_active_runs": 16,
                            "keys": {
                                "admin": {"subject": "admin", "secret": "admin-api-key"},
                                "regular": {"subject": "regular", "secret": "regular-api-key"}
                            }
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let db_url = schema.url();
    let settings = Settings::read(&|name| match name {
        "BOT_PROFILES" => Some(profiles.to_str().unwrap().to_owned()),
        "BOT_HOST_CONFIG" => Some(host_path.to_str().unwrap().to_owned()),
        "BOT_DATABASE_URL" => Some(db_url.clone()),
        "BOT_SOCKET" => Some(socket.to_str().unwrap().to_owned()),
        "BOT_SECRETS" => Some(format!("sops:{}", sops_file.display())),
        _ => None,
    })
    .unwrap();

    let stop = CancellationToken::new();
    let overrides = Overrides {
        claude: None,
        secrets: None,
        secret_admin: Some(secret_admin),
        stop: Some(stop.clone()),
        slack_base: None,
        upstream: None,
        store_config: None,
    };
    let serve_task = tokio::spawn(async move {
        serve::run_with(&settings, &[Role::Gateway, Role::Worker], true, overrides).await
    });
    wait_for(
        "serve가 활성 판본과 HTTP 입구를 열지 않았어요",
        || {
            !schema.active().is_empty()
                && TcpStream::connect(format!("127.0.0.1:{http_port}")).is_ok()
        },
    );
    let rev_initial = schema.active();

    // ── 1) 일반 사용자는 스코프가 있어도 관리 도구를 보지 못한다 ──
    llm.push([Turn::Reply("권한이 없어 스킬을 만들 수 없어요.".into())]);
    let (status, body) = chat(http_port, USER_KEY, "assistant", "스킬 만들어줘").await;
    assert_eq!(status, 200, "{body}");
    let regular_seen = llm.take_seen();
    assert_eq!(regular_seen.len(), 1);
    assert!(
        !regular_seen[0]
            .tool_names
            .iter()
            .any(|t| t.starts_with("skill_") || t.starts_with("mcp_")),
        "일반 사용자에게 관리 도구가 노출됐어요: {:?}",
        regular_seen[0].tool_names
    );

    // ── 2) 슈퍼 관리자: skill-creator 발동 → skill.inspect → main/developer에 스킬 생성·즉시 활성화 ──
    llm.push([
        Turn::CallTool {
            prefix: "skill_inspect_".into(),
            args: json!({}),
        },
        Turn::CallTool {
            prefix: "skill_create_".into(),
            args: json!({
                "agent": "main/developer",
                "name": "code-review",
                "description": "PR 코드 리뷰를 보안·경계 조건 관점으로 수행한다",
                "body": "# 코드 리뷰\n- 입력 검증과 경계 조건을 우선 확인한다.\n- 비밀 값이 로그에 남지 않는지 점검한다."
            }),
        },
        Turn::Reply("main/developer 에이전트에 code-review 스킬을 만들고 바로 활성화했어요.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "개발자용 코드 리뷰 체크리스트 스킬 만들어줘",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(assistant_content(&body).contains("code-review"), "{body}");
    let create_seen = llm.take_seen();
    assert_eq!(create_seen.len(), 3);
    assert!(
        create_seen[0].system.contains("### skill-creator"),
        "skill-creator 지침이 시스템 프롬프트에 있어야 해요"
    );
    let inspect_out = tool_outputs(&create_seen[1]);
    assert!(
        inspect_out.contains("main/developer"),
        "skill.inspect 결과에 main/developer가 포함돼야 해요: {inspect_out}"
    );
    let create_out = tool_outputs(&create_seen[2]);
    assert!(
        create_out.contains(r#""active":true"#),
        "skill.create 결과가 즉시 활성화 성공이어야 해요: {create_out}"
    );
    let rev_after_create = schema.active();
    assert_ne!(
        rev_initial, rev_after_create,
        "DB 활성 판본이 즉시 갱신돼야 해요"
    );
    assert!(
        profile
            .join("agents/main/agents/developer/skills/code-review/SKILL.md")
            .is_file()
    );

    // 재시작 없이 바로 다음 developer 요청에서 새 스킬이 시스템 프롬프트에 반영된다!
    llm.push([Turn::Reply("리뷰 완료".into())]);
    let (status, body) = chat(http_port, ADMIN_KEY, "developer", "이 PR 검토해줘").await;
    assert_eq!(status, 200, "{body}");
    let dev_seen = llm.take_seen();
    assert!(
        dev_seen[0].system.contains("### code-review")
            && dev_seen[0].system.contains("입력 검증과 경계 조건"),
        "새 스킬이 재시작 없이 즉시 반영돼야 해요:\n{}",
        dev_seen[0].system
    );

    // ── 3) 스킬 수정(skill.update) → 즉시 반영, 그 뒤 스킬 삭제(skill.delete) → 즉시 제거 ──
    llm.push([
        Turn::CallTool {
            prefix: "skill_update_".into(),
            args: json!({
                "name": "code-review",
                "body": "# 코드 리뷰 v2\n- 트랜잭션 원자성과 롤백을 우선 확인한다."
            }),
        },
        Turn::Reply("code-review 스킬을 수정했어요.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "code-review 스킬 수정해줘",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let _ = llm.take_seen();

    llm.push([Turn::Reply("v2 리뷰".into())]);
    let (status, _) = chat(http_port, ADMIN_KEY, "developer", "다시 검토해줘").await;
    assert_eq!(status, 200);
    let updated_seen = llm.take_seen();
    assert!(
        updated_seen[0].system.contains("코드 리뷰 v2"),
        "수정된 스킬이 즉시 반영돼야 해요"
    );

    llm.push([
        Turn::CallTool {
            prefix: "skill_delete_".into(),
            args: json!({"name": "code-review"}),
        },
        Turn::Reply("code-review 스킬을 삭제했어요.".into()),
    ]);
    let (status, body) = chat(http_port, ADMIN_KEY, "assistant", "code-review 스킬 지워줘").await;
    assert_eq!(status, 200, "{body}");
    let _ = llm.take_seen();
    assert!(
        !profile
            .join("agents/main/agents/developer/skills/code-review")
            .exists()
    );

    llm.push([Turn::Reply("기본 검토".into())]);
    let (status, _) = chat(http_port, ADMIN_KEY, "developer", "검토해줘").await;
    assert_eq!(status, 200);
    let deleted_seen = llm.take_seen();
    assert!(
        !deleted_seen[0].system.contains("### code-review"),
        "삭제된 스킬이 시스템 프롬프트에서 즉시 사라져야 해요"
    );

    // ── 4) 검증 실패(스킬 본문에 비밀 토큰 포함) 시 거부·롤백, 활성 판본 유지 ──
    let rev_before_bad = schema.active();
    llm.push([
        Turn::CallTool {
            prefix: "skill_create_".into(),
            args: json!({
                "agent": "main",
                "name": "bad-secret",
                "description": "잘못된 스킬",
                "body": "토큰: ghp_1234567890abcdefghijklmnopqrstuvwxyz"
            }),
        },
        Turn::Reply("비밀이 섞여 거절됐어요.".into()),
    ]);
    let (status, _) = chat(http_port, ADMIN_KEY, "assistant", "토큰 넣어서 만들어줘").await;
    assert_eq!(status, 200);
    let bad_seen = llm.take_seen();
    let bad_out = tool_outputs(&bad_seen[1]);
    assert!(
        bad_out.contains("secret_in_skill"),
        "비밀 포함 스킬은 secret_in_skill로 거절돼야 해요: {bad_out}"
    );
    assert!(!profile.join("agents/main/skills/bad-secret").exists());
    assert_eq!(schema.active(), rev_before_bad);

    // ── 5) MCP 추가(SOPS 암호화 저장 + 원격 동기화 + 즉시 활성화) → 바로 도구 실행 → MCP·비밀 제거 ──
    llm.push([
        Turn::CallTool {
            prefix: "mcp_add_".into(),
            args: json!({
                "alias": "weather",
                "transport": "http",
                "url": mcp_server.url(),
                "auth_secret": "weather-token",
                "secret_value": WEATHER_TOKEN,
                "agent": "main",
                "default_class": "read"
            }),
        },
        Turn::Reply("weather MCP를 연결하고 활성화했어요.".into()),
    ]);
    let (status, body) = chat(http_port, ADMIN_KEY, "assistant", "weather MCP 추가해줘").await;
    assert_eq!(status, 200, "{body}");
    let add_seen = llm.take_seen();
    let add_out = tool_outputs(&add_seen[1]);
    assert!(
        add_out.contains("weather.get-forecast") && add_out.contains(r#""active":true"#),
        "mcp.add 결과가 도구 목록과 즉시 활성화를 알려야 해요: {add_out}"
    );
    // SOPS 파일에만 토큰이 저장됐고 프로필·host.json에는 평문 토큰이 단 한 곳도 없다
    let sops_json: Value = serde_json::from_str(&fs::read_to_string(&sops_file).unwrap()).unwrap();
    assert_eq!(sops_json["bot"]["personal"]["weather-token"], WEATHER_TOKEN);
    let host_text = fs::read_to_string(&host_path).unwrap();
    assert!(
        !host_text.contains(WEATHER_TOKEN),
        "host.json에 평문 비밀이 남았어요"
    );
    for (p, content) in read_all_files_under(&profiles) {
        assert!(
            !content.contains(WEATHER_TOKEN),
            "{}에 평문 비밀이 남았어요",
            p.display()
        );
    }

    // 재시작 없이 바로 다음 요청에서 새로 추가된 weather.get-forecast 도구를 실제로 실행한다!
    llm.push([
        Turn::CallTool {
            prefix: "weather_get-forecast_".into(),
            args: json!({"city": "Seoul"}),
        },
        Turn::Reply("서울 날씨 조회를 마쳤어요.".into()),
    ]);
    let (status, body) = chat(http_port, ADMIN_KEY, "assistant", "서울 날씨 알려줘").await;
    assert_eq!(status, 200, "{body}");
    let weather_seen = llm.take_seen();
    let weather_out = tool_outputs(&weather_seen[1]);
    assert!(
        weather_out.contains("ok:get-forecast"),
        "새 MCP 도구가 실제 HTTP MCP 서버를 호출해 응답해야 해요: {weather_out}"
    );
    assert_eq!(mcp_server.mcp.count("tools/call"), 1);

    // MCP 제거 + 미사용 비밀 정리(remove_secrets: true) → SOPS에서 제거되고 다음 요청에서 도구 사라짐
    llm.push([
        Turn::CallTool {
            prefix: "mcp_remove_".into(),
            args: json!({"alias": "weather", "remove_secrets": true}),
        },
        Turn::Reply("weather MCP와 비밀을 제거했어요.".into()),
    ]);
    let (status, body) = chat(http_port, ADMIN_KEY, "assistant", "weather MCP 제거해줘").await;
    assert_eq!(status, 200, "{body}");
    let _ = llm.take_seen();
    assert!(!profile.join("agents/main/skills/weather").exists());
    let host_after: Value = serde_json::from_str(&fs::read_to_string(&host_path).unwrap()).unwrap();
    assert!(host_after["servers"].get("weather").is_none());
    let sops_after: Value = serde_json::from_str(&fs::read_to_string(&sops_file).unwrap()).unwrap();
    assert!(sops_after["bot"]["personal"].get("weather-token").is_none());

    llm.push([Turn::Reply("이제 weather 도구가 없어요.".into())]);
    let (status, _) = chat(http_port, ADMIN_KEY, "assistant", "서울 날씨 또 알려줘").await;
    assert_eq!(status, 200);
    let gone_seen = llm.take_seen();
    assert!(
        !gone_seen[0]
            .tool_names
            .iter()
            .any(|t| t.starts_with("weather_")),
        "제거된 MCP 도구가 남아 있어요: {:?}",
        gone_seen[0].tool_names
    );

    stop.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(30), serve_task)
        .await
        .expect("serve가 제때 종료되지 않았어요")
        .unwrap();
    assert_eq!(ended, Ok(()), "serve가 오류로 종료됐어요");
}

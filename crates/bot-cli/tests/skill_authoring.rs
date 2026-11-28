//! End-to-end test for super-admin live skill CRUD, `skill-creator`, remote MCP add/remove, and SOPS secret handling.
//! Runs `bot serve --role gateway,worker` against PG, OpenAI HTTP ingress, `LiveManager`, `ReqwestWire` + `FakeMcp`,
//! and `SopsStore` without restarts, checking regular-user isolation, rollback on secret leaks, and live activation.
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
        "---\ndescription: \"Team coordinator — routes requests and manages skills and MCP servers\"\n---\nManage skills and MCP servers for super-admin requests.\n",
    )
    .unwrap();
    fs::write(
        profile.join("agents/main/agents/developer/AGENT.md"),
        "---\ndescription: \"Developer agent — handles code review, testing, and implementation\"\n---\nPerform development work with focus on code quality and security.\n",
    )
    .unwrap();

    // Install the built-in `skill-creator` skill via the real CLI command.
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
            "Look up city weather forecast",
            forecast_schema,
        )],
    );
    fake_mcp.token = Some(WEATHER_TOKEN.into());
    let mcp_server = serve_mcp(fake_mcp).await;

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
        telegram_base: None,
        upstream: None,
        store_config: None,
    };
    let serve_task = tokio::spawn(async move {
        serve::run_with(&settings, &[Role::Gateway, Role::Worker], true, overrides).await
    });
    wait_for(
        "serve did not publish active revision and open HTTP listener",
        || {
            !schema.active().is_empty()
                && TcpStream::connect(format!("127.0.0.1:{http_port}")).is_ok()
        },
    );
    let rev_initial = schema.active();

    // 1) Regular members never see management tools even when their role includes the scopes.
    llm.push([Turn::Reply(
        "You do not have permission to create skills.".into(),
    )]);
    let (status, body) = chat(http_port, USER_KEY, "assistant", "Create a skill").await;
    assert_eq!(status, 200, "{body}");
    let regular_seen = llm.take_seen();
    assert_eq!(regular_seen.len(), 1);
    assert!(
        !regular_seen[0]
            .tool_names
            .iter()
            .any(|t| t.starts_with("skill_") || t.starts_with("mcp_")),
        "management tools leaked to regular user: {:?}",
        regular_seen[0].tool_names
    );

    // 2) Super-admin: inspect agents, create a skill on `main/developer`, and activate live.
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
                "description": "Review PR code for security and boundary conditions",
                "body": "# Code Review\n- Check input validation and boundary conditions first.\n- Verify secrets are never written to logs."
            }),
        },
        Turn::Reply(
            "Created and immediately activated the code-review skill on main/developer.".into(),
        ),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Create a code review checklist skill for the developer agent",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(assistant_content(&body).contains("code-review"), "{body}");
    let create_seen = llm.take_seen();
    assert_eq!(create_seen.len(), 3);
    assert!(
        create_seen[0].system.contains("### skill-creator"),
        "skill-creator instructions must appear in system prompt"
    );
    let inspect_out = tool_outputs(&create_seen[1]);
    assert!(
        inspect_out.contains("main/developer"),
        "skill.inspect output must include main/developer: {inspect_out}"
    );
    let create_out = tool_outputs(&create_seen[2]);
    assert!(
        create_out.contains(r#""active":true"#),
        "skill.create must report live activation: {create_out}"
    );
    let rev_after_create = schema.active();
    assert_ne!(
        rev_initial, rev_after_create,
        "DB active revision must update immediately"
    );
    assert!(
        profile
            .join("agents/main/agents/developer/skills/code-review/SKILL.md")
            .is_file()
    );

    // The next request to `developer` sees the new skill immediately without restart.
    llm.push([Turn::Reply("Review complete".into())]);
    let (status, body) = chat(http_port, ADMIN_KEY, "developer", "Review this PR").await;
    assert_eq!(status, 200, "{body}");
    let dev_seen = llm.take_seen();
    assert!(
        dev_seen[0].system.contains("### code-review")
            && dev_seen[0]
                .system
                .contains("input validation and boundary conditions"),
        "new skill must be live immediately:\n{}",
        dev_seen[0].system
    );

    // 3) Live update and delete reflect immediately in subsequent prompts.
    llm.push([
        Turn::CallTool {
            prefix: "skill_update_".into(),
            args: json!({
                "name": "code-review",
                "body": "# Code Review v2\n- Check transaction atomicity and rollback first."
            }),
        },
        Turn::Reply("Updated the code-review skill.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Update the code-review skill",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let _ = llm.take_seen();

    llm.push([Turn::Reply("v2 review".into())]);
    let (status, _) = chat(http_port, ADMIN_KEY, "developer", "Review again").await;
    assert_eq!(status, 200);
    let updated_seen = llm.take_seen();
    assert!(
        updated_seen[0].system.contains("Code Review v2"),
        "updated skill must be live immediately"
    );

    llm.push([
        Turn::CallTool {
            prefix: "skill_delete_".into(),
            args: json!({"name": "code-review"}),
        },
        Turn::Reply("Deleted the code-review skill.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Delete the code-review skill",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let _ = llm.take_seen();
    assert!(
        !profile
            .join("agents/main/agents/developer/skills/code-review")
            .exists()
    );

    llm.push([Turn::Reply("Standard review".into())]);
    let (status, _) = chat(http_port, ADMIN_KEY, "developer", "Review this").await;
    assert_eq!(status, 200);
    let deleted_seen = llm.take_seen();
    assert!(
        !deleted_seen[0].system.contains("### code-review"),
        "deleted skill must disappear from system prompt immediately"
    );

    // 4) Secret embedded in skill body triggers rollback and leaves active revision untouched.
    let rev_before_bad = schema.active();
    llm.push([
        Turn::CallTool {
            prefix: "skill_create_".into(),
            args: json!({
                "agent": "main",
                "name": "bad-secret",
                "description": "Invalid skill",
                "body": "Token: ghp_1234567890abcdefghijklmnopqrstuvwxyz"
            }),
        },
        Turn::Reply("Rejected because the skill contained a secret.".into()),
    ]);
    let (status, _) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Create a skill with a token",
    )
    .await;
    assert_eq!(status, 200);
    let bad_seen = llm.take_seen();
    let bad_out = tool_outputs(&bad_seen[1]);
    assert!(
        bad_out.contains("secret_in_skill"),
        "skill containing a secret must fail with secret_in_skill: {bad_out}"
    );
    assert!(!profile.join("agents/main/skills/bad-secret").exists());
    assert_eq!(schema.active(), rev_before_bad);

    // 5) Remote MCP add (SOPS secret only, no plaintext on disk) -> live tool call -> remove with secret cleanup.
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
        Turn::Reply("Connected and activated the weather MCP server.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Add the weather MCP server",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let add_seen = llm.take_seen();
    let add_out = tool_outputs(&add_seen[1]);
    assert!(
        add_out.contains("weather.get-forecast") && add_out.contains(r#""active":true"#),
        "mcp.add must report synced tools and live activation: {add_out}"
    );
    let sops_json: Value = serde_json::from_str(&fs::read_to_string(&sops_file).unwrap()).unwrap();
    assert_eq!(sops_json["bot"]["personal"]["weather-token"], WEATHER_TOKEN);
    let host_text = fs::read_to_string(&host_path).unwrap();
    assert!(
        !host_text.contains(WEATHER_TOKEN),
        "plaintext secret leaked into host.json"
    );
    for (p, content) in read_all_files_under(&profiles) {
        assert!(
            !content.contains(WEATHER_TOKEN),
            "plaintext secret leaked into {}",
            p.display()
        );
    }

    llm.push([
        Turn::CallTool {
            prefix: "weather_get-forecast_".into(),
            args: json!({"city": "Seoul"}),
        },
        Turn::Reply("Fetched the weather forecast for Seoul.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Show the weather in Seoul",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let weather_seen = llm.take_seen();
    let weather_out = tool_outputs(&weather_seen[1]);
    assert!(
        weather_out.contains("ok:get-forecast"),
        "new MCP tool must invoke remote HTTP MCP server: {weather_out}"
    );
    assert_eq!(mcp_server.mcp.count("tools/call"), 1);

    llm.push([
        Turn::CallTool {
            prefix: "mcp_remove_".into(),
            args: json!({"alias": "weather", "remove_secrets": true}),
        },
        Turn::Reply("Removed the weather MCP server and its secrets.".into()),
    ]);
    let (status, body) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Remove the weather MCP server",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let _ = llm.take_seen();
    assert!(!profile.join("agents/main/skills/weather").exists());
    let host_after: Value = serde_json::from_str(&fs::read_to_string(&host_path).unwrap()).unwrap();
    assert!(host_after["servers"].get("weather").is_none());
    let sops_after: Value = serde_json::from_str(&fs::read_to_string(&sops_file).unwrap()).unwrap();
    assert!(sops_after["bot"]["personal"].get("weather-token").is_none());

    llm.push([Turn::Reply(
        "The weather tool is no longer available.".into(),
    )]);
    let (status, _) = chat(
        http_port,
        ADMIN_KEY,
        "assistant",
        "Show the weather in Seoul again",
    )
    .await;
    assert_eq!(status, 200);
    let gone_seen = llm.take_seen();
    assert!(
        !gone_seen[0]
            .tool_names
            .iter()
            .any(|t| t.starts_with("weather_")),
        "removed MCP tool remained exposed: {:?}",
        gone_seen[0].tool_names
    );

    stop.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(30), serve_task)
        .await
        .expect("serve did not shut down in time")
        .unwrap();
    assert_eq!(ended, Ok(()), "serve exited with error");
}

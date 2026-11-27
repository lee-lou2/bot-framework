//! 통합 시험 묶음(`bot-cli`, 비카나리).

mod chat_support;
mod claude_support;
mod mcp_http_support;
mod process_support;

#[path = "suite/add_tool.rs"]
mod add_tool;
#[path = "suite/add_tool_safety.rs"]
mod add_tool_safety;
#[path = "suite/chat_output.rs"]
mod chat_output;
#[path = "suite/chat_socket.rs"]
mod chat_socket;
#[path = "suite/doctor.rs"]
mod doctor;
#[path = "suite/doctor_db.rs"]
mod doctor_db;
#[path = "suite/driver_logs.rs"]
mod driver_logs;
#[path = "suite/gateway_process.rs"]
mod gateway_process;
#[path = "suite/http_listen.rs"]
mod http_listen;
#[path = "suite/legacy_skill_pg.rs"]
mod legacy_skill_pg;
#[path = "suite/legacy_skill_prompt.rs"]
mod legacy_skill_prompt;
#[path = "suite/log_content.rs"]
mod log_content;
#[path = "suite/mcp_http_long.rs"]
mod mcp_http_long;
#[path = "suite/model_keys.rs"]
mod model_keys;
#[path = "suite/observability_e2e.rs"]
mod observability_e2e;
#[path = "suite/openai_keys.rs"]
mod openai_keys;
#[path = "suite/outbox_routing.rs"]
mod outbox_routing;
#[path = "suite/scaffold.rs"]
mod scaffold;
#[path = "suite/secret_reread.rs"]
mod secret_reread;
#[path = "suite/serve_activation.rs"]
mod serve_activation;
#[path = "suite/serve_admin.rs"]
mod serve_admin;
#[path = "suite/serve_schedule.rs"]
mod serve_schedule;
#[path = "suite/serve_token.rs"]
mod serve_token;
#[path = "suite/servers.rs"]
mod servers;
#[path = "suite/slack_approval.rs"]
mod slack_approval;
#[path = "suite/slack_card_helper.rs"]
mod slack_card_helper;
#[path = "suite/slack_receive.rs"]
mod slack_receive;
#[path = "suite/trace_remote.rs"]
mod trace_remote;

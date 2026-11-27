//! Single-binary integration test entry point for `bot-profile`.

mod support;

#[path = "suite/acceptance.rs"]
mod acceptance;
#[path = "suite/assembly.rs"]
mod assembly;
#[path = "suite/demo_server.rs"]
mod demo_server;
#[path = "suite/front_matter.rs"]
mod front_matter;
#[path = "suite/legacy_bytes.rs"]
mod legacy_bytes;
#[path = "suite/legacy_skill_restore.rs"]
mod legacy_skill_restore;
#[path = "suite/locate.rs"]
mod locate;
#[path = "suite/messages.rs"]
mod messages;
#[path = "suite/openai.rs"]
mod openai;
#[path = "suite/openai_routes.rs"]
mod openai_routes;
#[path = "suite/preapproval.rs"]
mod preapproval;
#[path = "suite/rejects.rs"]
mod rejects;
#[path = "suite/rejects_triggers.rs"]
mod rejects_triggers;
#[path = "suite/remote_lock.rs"]
mod remote_lock;
#[path = "suite/restore.rs"]
mod restore;
#[path = "suite/revisions.rs"]
mod revisions;
#[path = "suite/slack_events.rs"]
mod slack_events;
#[path = "suite/source.rs"]
mod source;
#[path = "suite/team_example.rs"]
mod team_example;
#[path = "suite/tool_env.rs"]
mod tool_env;
#[path = "suite/tool_timeout.rs"]
mod tool_timeout;
#[path = "suite/triggers_schema.rs"]
mod triggers_schema;

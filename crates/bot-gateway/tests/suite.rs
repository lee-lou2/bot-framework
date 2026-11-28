//! Integration test suite for `bot-gateway`.

mod delivery_support;
mod http_support;
mod openai_support;
mod support;

#[path = "suite/card_click.rs"]
mod card_click;
#[path = "suite/card_deliver.rs"]
mod card_deliver;
#[path = "suite/card_dm.rs"]
mod card_dm;
#[path = "suite/card_origin.rs"]
mod card_origin;
#[path = "suite/cli.rs"]
mod cli;
#[path = "suite/cli_hints.rs"]
mod cli_hints;
#[path = "suite/delivery.rs"]
mod delivery;
#[path = "suite/delivery_route.rs"]
mod delivery_route;
#[path = "suite/delivery_slack.rs"]
mod delivery_slack;
#[path = "suite/http.rs"]
mod http;
#[path = "suite/http_active.rs"]
mod http_active;
#[path = "suite/http_security.rs"]
mod http_security;
#[path = "suite/openai_accept.rs"]
mod openai_accept;
#[path = "suite/openai_auth_limit.rs"]
mod openai_auth_limit;
#[path = "suite/openai_chat.rs"]
mod openai_chat;
#[path = "suite/openai_deadline.rs"]
mod openai_deadline;
#[path = "suite/openai_http.rs"]
mod openai_http;
#[path = "suite/openai_limits.rs"]
mod openai_limits;
#[path = "suite/openai_revoke.rs"]
mod openai_revoke;
#[path = "suite/openai_stream.rs"]
mod openai_stream;
#[path = "suite/openai_wait.rs"]
mod openai_wait;
#[path = "suite/retry.rs"]
mod retry;
#[path = "suite/slack.rs"]
mod slack;
#[path = "suite/slack_approval.rs"]
mod slack_approval;
#[path = "suite/slack_events.rs"]
mod slack_events;
#[path = "suite/slack_evidence_seam.rs"]
mod slack_evidence_seam;
#[path = "suite/slack_reactions.rs"]
mod slack_reactions;
#[path = "suite/slack_ux.rs"]
mod slack_ux;
#[path = "suite/slack_ux_stream.rs"]
mod slack_ux_stream;
#[path = "suite/telegram.rs"]
mod telegram;
#[path = "suite/telegram_ext.rs"]
mod telegram_ext;
#[path = "suite/token_rejected.rs"]
mod token_rejected;
#[path = "suite/trace.rs"]
mod trace;

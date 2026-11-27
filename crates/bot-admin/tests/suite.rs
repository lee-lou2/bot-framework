//! Single-binary integration test entrypoint for `bot-admin`.

mod support;

#[path = "suite/approvals.rs"]
mod approvals;
#[path = "suite/assets.rs"]
mod assets;
#[path = "suite/auth.rs"]
mod auth;
#[path = "suite/decisions.rs"]
mod decisions;
#[path = "suite/delivery_card.rs"]
mod delivery_card;
#[path = "suite/fence.rs"]
mod fence;
#[path = "suite/hold_ids.rs"]
mod hold_ids;
#[path = "suite/hold_pairs.rs"]
mod hold_pairs;
#[path = "suite/hold_profiles.rs"]
mod hold_profiles;
#[path = "suite/holds.rs"]
mod holds;
#[path = "suite/lineage.rs"]
mod lineage;
#[path = "suite/map.rs"]
mod map;
#[path = "suite/observability.rs"]
mod observability;
#[path = "suite/observability_tls.rs"]
mod observability_tls;
#[path = "suite/preapprovals.rs"]
mod preapprovals;
#[path = "suite/private_body.rs"]
mod private_body;
#[path = "suite/races.rs"]
mod races;
#[path = "suite/serve.rs"]
mod serve;
#[path = "suite/settings.rs"]
mod settings;
#[path = "suite/trace.rs"]
mod trace;
#[path = "suite/trace_admin.rs"]
mod trace_admin;
#[path = "suite/trace_config.rs"]
mod trace_config;
#[path = "suite/trace_gate.rs"]
mod trace_gate;
#[path = "suite/ui.rs"]
mod ui;
#[path = "suite/ui_logic.rs"]
mod ui_logic;

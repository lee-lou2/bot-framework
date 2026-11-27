//! Integration test entry point for `bot-store` (single binary).

mod support;

#[path = "suite/accept.rs"]
mod accept;
#[path = "suite/activation_fence.rs"]
mod activation_fence;
#[path = "suite/active.rs"]
mod active;
#[path = "suite/active_seeds.rs"]
mod active_seeds;
#[path = "suite/admin.rs"]
mod admin;
#[path = "suite/admin_hold_http.rs"]
mod admin_hold_http;
#[path = "suite/admin_listing.rs"]
mod admin_listing;
#[path = "suite/admin_run_http.rs"]
mod admin_run_http;
#[path = "suite/admin_views.rs"]
mod admin_views;
#[path = "suite/api_accept.rs"]
mod api_accept;
#[path = "suite/api_approval_cards.rs"]
mod api_approval_cards;
#[path = "suite/api_broker_approval.rs"]
mod api_broker_approval;
#[path = "suite/api_card_set.rs"]
mod api_card_set;
#[path = "suite/api_status.rs"]
mod api_status;
#[path = "suite/approval_expiry_locks.rs"]
mod approval_expiry_locks;
#[path = "suite/audit.rs"]
mod audit;
#[path = "suite/broker_flow.rs"]
mod broker_flow;
#[path = "suite/card_failure.rs"]
mod card_failure;
#[path = "suite/card_jobs.rs"]
mod card_jobs;
#[path = "suite/cli_checkpoint.rs"]
mod cli_checkpoint;
#[path = "suite/create_card_fingerprint.rs"]
mod create_card_fingerprint;
#[path = "suite/create_card_set.rs"]
mod create_card_set;
#[path = "suite/create_route_race.rs"]
mod create_route_race;
#[path = "suite/delegate.rs"]
mod delegate;
#[path = "suite/delegate_result.rs"]
mod delegate_result;
#[path = "suite/delivery.rs"]
mod delivery;
#[path = "suite/delivery_card_race.rs"]
mod delivery_card_race;
#[path = "suite/delivery_card_set.rs"]
mod delivery_card_set;
#[path = "suite/delivery_ids.rs"]
mod delivery_ids;
#[path = "suite/delivery_race.rs"]
mod delivery_race;
#[path = "suite/delivery_release.rs"]
mod delivery_release;
#[path = "suite/delivery_unrouted.rs"]
mod delivery_unrouted;
#[path = "suite/delivery_unsent.rs"]
mod delivery_unsent;
#[path = "suite/delivery_unsent_precedence.rs"]
mod delivery_unsent_precedence;
#[path = "suite/delivery_views.rs"]
mod delivery_views;
#[path = "suite/effect_lookup.rs"]
mod effect_lookup;
#[path = "suite/effect_secret.rs"]
mod effect_secret;
#[path = "suite/effects.rs"]
mod effects;
#[path = "suite/handover_set.rs"]
mod handover_set;
#[path = "suite/history_scope.rs"]
mod history_scope;
#[path = "suite/hold_ids.rs"]
mod hold_ids;
#[path = "suite/hold_targets.rs"]
mod hold_targets;
#[path = "suite/http_accept_fence.rs"]
mod http_accept_fence;
#[path = "suite/http_intake.rs"]
mod http_intake;
#[path = "suite/human_approval.rs"]
mod human_approval;
#[path = "suite/ledger_approvals.rs"]
mod ledger_approvals;
#[path = "suite/ledger_calls.rs"]
mod ledger_calls;
#[path = "suite/ledger_claims.rs"]
mod ledger_claims;
#[path = "suite/ledger_fence_profiles.rs"]
mod ledger_fence_profiles;
#[path = "suite/ledger_lookup.rs"]
mod ledger_lookup;
#[path = "suite/memory.rs"]
mod memory;
#[path = "suite/memory_hybrid.rs"]
mod memory_hybrid;
#[path = "suite/memory_scope.rs"]
mod memory_scope;
#[path = "suite/memory_share.rs"]
mod memory_share;
#[path = "suite/migrations.rs"]
mod migrations;
#[path = "suite/no_select_star.rs"]
mod no_select_star;
#[path = "suite/outbox.rs"]
mod outbox;
#[path = "suite/pass_chain.rs"]
mod pass_chain;
#[path = "suite/pass_recovery.rs"]
mod pass_recovery;
#[path = "suite/plan_stability.rs"]
mod plan_stability;
#[path = "suite/preapproval.rs"]
mod preapproval;
#[path = "suite/preapproval_effects.rs"]
mod preapproval_effects;
#[path = "suite/preapproval_pause.rs"]
mod preapproval_pause;
#[path = "suite/private_expiry.rs"]
mod private_expiry;
#[path = "suite/private_flow.rs"]
mod private_flow;
#[path = "suite/private_ledger.rs"]
mod private_ledger;
#[path = "suite/profile_bundles.rs"]
mod profile_bundles;
#[path = "suite/quarantine.rs"]
mod quarantine;
#[path = "suite/queue.rs"]
mod queue;
#[path = "suite/races.rs"]
mod races;
#[path = "suite/run_origin.rs"]
mod run_origin;
#[path = "suite/schedule.rs"]
mod schedule;
#[path = "suite/schedule_create_approval.rs"]
mod schedule_create_approval;
#[path = "suite/schedule_destination.rs"]
mod schedule_destination;
#[path = "suite/schedule_seal.rs"]
mod schedule_seal;
#[path = "suite/share_flow.rs"]
mod share_flow;
#[path = "suite/shutdown_release.rs"]
mod shutdown_release;
#[path = "suite/slack_dedupe.rs"]
mod slack_dedupe;
#[path = "suite/stale_release.rs"]
mod stale_release;
#[path = "suite/static_publication.rs"]
mod static_publication;
#[path = "suite/trigger_origin.rs"]
mod trigger_origin;
#[path = "suite/user_close_notice.rs"]
mod user_close_notice;
#[path = "suite/user_close_notice_race.rs"]
mod user_close_notice_race;
#[path = "suite/user_close_reasons.rs"]
mod user_close_reasons;
#[path = "suite/user_due_pages.rs"]
mod user_due_pages;
#[path = "suite/user_schedule_due.rs"]
mod user_schedule_due;
#[path = "suite/user_schedules.rs"]
mod user_schedules;
#[path = "suite/user_unreadable_due.rs"]
mod user_unreadable_due;
#[path = "suite/worker_fence.rs"]
mod worker_fence;

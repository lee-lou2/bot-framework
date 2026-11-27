//! Explicit column lists per table to avoid `select *` prepared-statement plan invalidation (`0A000`) across migrations.
//! Enforced by `tests/no_select_star.rs`.

pub(crate) fn prefixed(columns: &str, alias: &str) -> String {
    columns
        .split(", ")
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) const RUNS: &str = concat!(
    "profile, id, root, parent, parent_step, depth, agent, principal, approver, scopes, ",
    "revision, trigger, binding, conv_kind, conv_id, thread, attachments, target, input, ",
    "state, wait_kind, wait_child, generation, claim_epoch, attempts, lease_owner, ",
    "lease_until_ms, not_before_ms, cancel_requested, serial_key, private_memory_read, ",
    "private, keep_generation, result, error, created_ms, updated_ms, finished_ms, ",
    "private_started_ms, quiet, wake_depth, run_origin, claim_set, pass_pos"
);

pub(crate) const CALLS: &str = concat!(
    "profile, run, call, tool, args_sha256, state, approval, started_generation, owner, ",
    "result, reason, resolution, receipt, risk, tool_revision, created_ms, updated_ms, ",
    "started_ms, finished_ms"
);

pub(crate) const APPROVALS: &str = concat!(
    "nonce, profile, run, generation, call, requester, approver, risk, policy_revision, ",
    "tool, tool_revision, args_sha256, expires_ms, reasons, card, state, decided_by, ",
    "decided_ms, created_ms"
);

pub(crate) const EFFECTS: &str = concat!(
    "profile, id, run, call, step, args_sha256, state, generation, receipt, reason, ",
    "resolution, created_ms, updated_ms"
);

pub(crate) const DELIVERIES: &str = concat!(
    "profile, run, dest_binding, dest_kind, dest_id, dest_thread, state, activation, ",
    "result, answers, sha256, drafted_ms, created_ms, updated_ms, unsent, ",
    "unsent_receipt, unsent_activation, unsent_nonce, unsent_ms"
);

pub(crate) const MEMORIES: &str = concat!(
    "profile, id, agent, owner, visibility, body, seed_path, seed_sha256, created_ms, ",
    "updated_ms, deleted_ms"
);

pub(crate) const USER_SCHEDULES: &str = concat!(
    "profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, ",
    "origin_id, origin_thread, origin_private_read, approver, ceiling, dest_binding, ",
    "dest_kind, dest_id, dest_thread, at_ms, cron, timezone, instruction, quiet, ",
    "wake_depth, state, close_reason, cursor_ms, next_ms, created_ms, updated_ms, ",
    "fire_origin"
);

pub(crate) const SEGMENTS: &str = concat!(
    "profile, agent, trigger, from_ms, until_ms, revision, cron, timezone, beat, ",
    "principal, approver, scopes, target"
);

pub(crate) const AUDIT: &str =
    "seq, profile, at_ms, actor, action, target, args_sha256, verdict, prev_hash, hash";

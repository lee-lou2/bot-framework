//! 테이블별 열 목록. `select *`·`x.*`는 쓰지 않는다: 준비된 문은 연결마다 캐시되는데 그 결과 형식은 열 순서까지 포함하므로, 마이그레이션이 열을 더하거나
//! (새 판본과 옛 프로세스가 같은 DB를 쓰는 동안) 열을 지웠다 다시 만들면 PostgreSQL이 `cached plan must not change result type`(0A000)으로 거절한다.
//! 열을 명시하면 결과 형식이 이 목록으로 고정된다. 새 열을 읽어야 하면 마이그레이션과 함께 여기도 고친다(읽지 않는 열은 더하지 않아도 된다).
//! `tests/no_select_star.rs`가 `select *`가 다시 생기는 것을 막는다.

/// `alias.열, alias.열, …` — 별칭이 있는 질의나 같은 표를 두 번 쓰는 질의용.
pub(crate) fn prefixed(columns: &str, alias: &str) -> String {
    columns
        .split(", ")
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `bot_runs`의 공통 조회에 쓰는 열 목록.
pub(crate) const RUNS: &str = concat!(
    "profile, id, root, parent, parent_step, depth, agent, principal, approver, scopes, ",
    "revision, trigger, binding, conv_kind, conv_id, thread, attachments, target, input, ",
    "state, wait_kind, wait_child, generation, claim_epoch, attempts, lease_owner, ",
    "lease_until_ms, not_before_ms, cancel_requested, serial_key, private_memory_read, ",
    "private, keep_generation, result, error, created_ms, updated_ms, finished_ms, ",
    "private_started_ms, quiet, wake_depth, run_origin, claim_set, pass_pos"
);

/// `bot_calls`의 공통 조회에 쓰는 열 목록.
pub(crate) const CALLS: &str = concat!(
    "profile, run, call, tool, args_sha256, state, approval, started_generation, owner, ",
    "result, reason, resolution, receipt, risk, tool_revision, created_ms, updated_ms, ",
    "started_ms, finished_ms"
);

/// `bot_approvals`의 공통 조회에 쓰는 열 목록.
pub(crate) const APPROVALS: &str = concat!(
    "nonce, profile, run, generation, call, requester, approver, risk, policy_revision, ",
    "tool, tool_revision, args_sha256, expires_ms, reasons, card, state, decided_by, ",
    "decided_ms, created_ms"
);

/// `bot_effects`의 공통 조회에 쓰는 열 목록.
pub(crate) const EFFECTS: &str = concat!(
    "profile, id, run, call, step, args_sha256, state, generation, receipt, reason, ",
    "resolution, created_ms, updated_ms"
);

/// `bot_deliveries`의 공통 조회에 쓰는 열 목록.
pub(crate) const DELIVERIES: &str = concat!(
    "profile, run, dest_binding, dest_kind, dest_id, dest_thread, state, activation, ",
    "result, answers, sha256, drafted_ms, created_ms, updated_ms, unsent, ",
    "unsent_receipt, unsent_activation, unsent_nonce, unsent_ms"
);

/// `bot_memories`의 공통 조회에 쓰는 열 목록.
pub(crate) const MEMORIES: &str = concat!(
    "profile, id, agent, owner, visibility, body, seed_path, seed_sha256, created_ms, ",
    "updated_ms, deleted_ms"
);

/// `bot_user_schedules`의 공통 조회에 쓰는 열 목록.
pub(crate) const USER_SCHEDULES: &str = concat!(
    "profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, ",
    "origin_id, origin_thread, origin_private_read, approver, ceiling, dest_binding, ",
    "dest_kind, dest_id, dest_thread, at_ms, cron, timezone, instruction, quiet, ",
    "wake_depth, state, close_reason, cursor_ms, next_ms, created_ms, updated_ms, ",
    "fire_origin"
);

/// `bot_schedule_segments`의 공통 조회에 쓰는 열 목록.
pub(crate) const SEGMENTS: &str = concat!(
    "profile, agent, trigger, from_ms, until_ms, revision, cron, timezone, beat, ",
    "principal, approver, scopes, target"
);

/// `bot_audit`의 공통 조회에 쓰는 열 목록.
pub(crate) const AUDIT: &str =
    "seq, profile, at_ms, actor, action, target, args_sha256, verdict, prev_hash, hash";

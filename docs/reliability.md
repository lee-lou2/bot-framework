# Reliability and Crash Recovery

English | [한국어](ko/reliability.md)

**Accepted means committed to PostgreSQL.** Execution attempts may repeat after network or process faults. Internal DB state applies once via unique keys and transactions; external writes recover only to what the receiver can prove.

## Unique Keys and Atomic Boundaries

| Unit | Unique key & atomic transaction boundary |
|---|---|
| Profile activation | Commits new active set, memory seeds, static trigger interval transitions, and audit together only when the expected active-set revision matches current. Any failure rolls back all; restarting with the identical set joins without writing. |
| Standard intake (CLI, Slack, webhook) | `(profile, binding, issuer/principal scope, stable event/request ID)`. Input hash covers authenticated conversation, attachments, body, and (for HTTP) actual method and sub-path. Same key + input returns the existing run; differing method or path is a conflict. ACK only after intake + run + queue + audit commit. |
| OpenAI-compatible intake | `(profile, HTTP entrance, authenticated principal, Idempotency-Key)` across bindings and models. Key reservation, per-entrance in-flight cap, run, and audit commit together. Same key + input + model returns the existing run without consuming a slot; differing input or model is a conflict. |
| Schedules | `(profile, agent, trigger ID, scheduled UTC instant)`. Schedule edits split intervals at the effective timestamp. DB-clock cursor, occurrence row, and queue advance in one commit; leader failover and concurrent schedulers converge on the same key. |
| Delegation | `(parent run, saved step ID)`. Child run and parent wait commit together; parent crash reuses the same child result. Model steps, tool calls, and results checkpoint for resume. |
| Replies | `(run, origin destination, answer seq)`. Finish result and outbox commit together; delivery follows the same unknown-outcome hold and receipt policy as tool effects. |

**Cron, DST, and `beat` rules:**
- Cron evaluates in profile local time: DST-skipped minutes are skipped; repeated fall-back minutes fire twice as distinct UTC instants.
- Missed occurrences during downtime catch up oldest-first in bounded batches.
- Skipping/coalescing is permitted only for status `beat` triggers, which distinguish one pending occurrence from a signal arriving mid-run so the latest update is never lost.

## Worker Handover

### Queue Order, `0024`, and Corrupt Targets

- Unlocked normal runs queue in one line ordered by `ready_at`, retry time, or lease expiry (earliest first; ties break by `created_at`, then `run_id`). Unblocking approval, releasing, or resuming resets wait time to current DB time so earlier waiters stay ahead; explicit future ready times remain intact. One `beat` key belongs to its `leased` predecessor until lease expiry, after which that run is reclaimed or terminated per attempt limit.
- **Migration `0024` (`crates/bot-store/migrations/0024_queue_order.sql`):** backfills null `ready_at` on legacy ready rows from `created_at` and creates `bot_runs_eligible`. Altering the column default holds a lock on `bot_runs` through index creation and commit (blocking reads/writes); benchmark on a production-sized clone and schedule a maintenance window for large databases. Drop `bot_runs_leased` in a separate migration after all legacy workers retire, and drop `bot_runs_ready` only after checking Admin list query plans against the new index.
- **Unreadable targets:** if a candidate's stored target cannot decode, the store preserves the raw payload, dead-letters it as `invalid_target`, records child failure on any waiting parent (skipping the child if the parent is locked), and continues. Each claim inspects at most 8 candidates (remaining wait for the next claim); DB errors return transient failure. Operator resume requires a decodable target. Before rolling out a new target format, swap the active set or drain legacy workers first.

### VM CLI Waiting on Approvals or Children

1. Before tool execution, the broker seals the complete tool-bearing upstream response received by the host and matches every `/mcp` call against that plan (preserving all slots when a turn plans multiple tools; neither the first MCP request nor a single session line proves turn completion).
2. Logical call identity is `(profile, run, saved assistant turn, tool slot)`. The host's `CallBook` checks the wire call ID (`_meta["claudecode/toolUseId"]` for `claude-cli`, `_meta["callId"]` for `codex-cli`, or FIFO `_meta["progressToken"]` correlation for `agy-cli`), tool name, and canonical args hash against that slot. Redelivery at the same slot returns the same call; two distinct slots sending identical args to the same recipient are two distinct calls.
3. On approval wait or direct child delegation, the parent drops its CLI connection and worker lease. Child results persist under `(parent run, call ID)`, so a single worker can execute children. Resuming workers check current permissions under a new claim and continue saved calls or read existing child results.
4. Each VM CLI adapter restores settled ledger results into a fresh claim workspace before resuming:
   - **`claude-cli`:** restores its session JSONL with verified ledger `tool_result` records and resumes with `--resume <UUID>`.
   - **`codex-cli`:** restores its rollout JSONL (using Codex's own `thread_id`), rewrites VM-authored `function_call_output` lines from the trusted ledger while preserving rollout `ordinal`, and resumes with `codex exec resume --all <thread_id>`.
   - **`agy-cli`:** runs ephemerally with no native session resume; reconstructs all settled `cli-group-*` / `cli-use-*` exchanges from the trusted ledger into the fresh ephemeral stdin prompt.
   Existing results are always verified against the ledger; CLI abort errors are never adopted as completion, and unproven call-to-result links remain held.

Checkpoints hold only conversation records required for restore. Each claim gets a fresh relay token, MCP config, and workspace so stale processes cannot overwrite new session files; each runtime adapter owns its CLI session format.

- **Response sealing:** the broker records complete tool-bearing responses to `cli-resp-<id>` (provider use IDs, full tool names, JSON args in order) before sending the final JSON/SSE event to the CLI. Truncated responses, missing IDs, or duplicate use IDs cannot start tools; post-commit disconnect retries reuse the existing record when key and content match.
- **Turn commit:** `claude-cli` and `codex-cli` aggregate CLI session/rollout records, verify all use IDs, order, names, and args, and write session records (`cli-rec-<seq>`), tool slots (`cli-use-<tool_use_id>`), and the turn group (`cli-group-<turn>`, where `turn` is the first assistant record number) in one transaction (`agy-cli` commits `cli-group-*` and `cli-use-*` directly from the broker). The relay reads the use ID and group under the current claim; first-seen groups verify every slot (for example, MCP `agenda_delete` must match its sealed call plus canonical JSON args). Redelivery resolves to the same `CallId::cli(turn, slot)` (delegation uses this ID as well).
- **Resume replay:** relay tools absent from the ledger re-check current permissions and approvals at their verified slot; completed calls reuse stored results (preserving MCP `content` and `isError`, wrapping plain JSON in one text block, and using fixed error words for rejection/expiry). Unverifiable VM built-in tool results are never substituted or marked succeeded by the host.

### Pre-Execution Refusal Sealing

Both the VM CLI front (`claude-cli`, `codex-cli`, `agy-cli`) and direct API worker seal confirmed pre-execution refusals at a verified planned slot under the current claim in `cli-refusal-<call_id>` and replay that exact word on resume: CLI attaches `isError: true`, Anthropic API attaches `is_error: true`, and OpenAI-compatible API places the word in the matching tool message.

- The stepped API worker writes the turn's full plan to `turn-N` before calling tools. If an earlier call is refused and a later call awaits approval, the next worker replays sealed refusals without calling the broker and continues to the approval slot; once all calls complete, it writes bundled results to `end-N`. Incomplete-turn success receipts and existing child results are re-verified against current permissions on resume.
- CLI serializes requests per slot so concurrent redeliveries cannot end one slot as both refused and executed. If the session is revoked while waiting on plan verification, slot lock, or seal read (even racing with completion), no execution, delegation, or seal starts and `stale` is returned. Sealed normal refusals consume zero run attempts; fixing credentials or routes lets a new model slot re-evaluate. Refusals before slot verification (invalid tool name, arg shape, `toolUseId`, plan mismatch, or unverified plan) return the word without sealing. Ledger entries already started, held, completed, or refused override seals; pre-start open records may be closed by a seal.

| Pre-execution verdict | Sealed word shown to model |
|---|---|
| Missing tool / invalid input | `unknown_tool` / `invalid` |
| Missing permission or scope, missing tool credential, disallowed schedule destination | `denied` |
| Call content conflict | `conflict` |
| Revision mismatch on VM CLI call | `stale` |
| Rate or concurrency limit reached | `unavailable` |

- Rate/concurrency limits seal only when confirmed rejected alongside that call's own result (per-call marker prevents prior rejections or unread flags from leaking onto another error at the same slot). Store outages/unreadable ledger (`unavailable`) and lost-claim `stale` are never sealed.
- On broker `Stale`, the direct API worker checks its lease: if still valid, it re-evaluates the same call against the new revision; if lost, it steps aside.
- When the store reports a missing schedule ID or invalid timing/timezone, the stepped worker returns that error to the model; refusal seals are written only when confirmed from ledger and pre-evaluation, while store `Unavailable` retries the turn.
- If the ledger cannot be read or the seal cannot be written, the slot stays unsettled: stepped workers retry the turn (broker verdict audit and seal write are separate, so a fault between them may append another audit on re-evaluation); CLI returns `unavailable` and, if still unsettled on exit, retries as `session_unsaved` (consuming an attempt); lost-lease workers back off with `stale`; already-started or uncertain calls follow ledger recovery.
- A corrupt seal blocks execution (`unavailable` on live CLI; `corrupt_checkpoint` on CLI restore and API worker). Malformed `delegate` args on the CLI front return `invalid` immediately; CLI resume validates stored args identically.

### Redeploy and Restoration Acceptance Criteria

Before opening roles on `--activate` or restart, a new process restores prior revisions from stored profile bundles and refuses startup if restoration fails. Real-PostgreSQL and multi-OS-process tests verify each case below, including the prepare-to-publish race (re-check rejects with `Stale`, verified alongside retry-decision tests) and two-host concurrent publication of the same set ([Revision restoration contract](profile-spec.md)).

| Pre-redeploy state | Expected result after `--activate` |
|---|---|
| Tool awaiting approval; only instructions changed (approval rules unchanged) | Approving via the existing card executes the same call exactly once. |
| Static `cron` scheduled time passed between processes | Fires once under that interval's revision; still-allowed tools succeed; 0 dead letters. |
| Old revision's tool revoked in new revision | Direct tool call returns `denied`; old approval button returns `stale`; 0 tool executions. Tools absent from the pinned revision return `unknown_tool`. |
| Destination removed from registry after delivery card issued | Verified by `claude_route_gone`: occurrence finishes as `delivery_unrouted` with 1 origin notice and 0 destination sends; re-clicking the old button yields the same result. |
| Keep profile A, remove profile B (B still has unbundled dead letters / unconsumed intervals) | Swapping A and joining new processes succeed; 0 bundle restores, model calls, tool executions, approval consumptions, or sends for B. |
| Re-register B with the same ID while an old revision reference is still needed | Fails before publish if source is missing, preserving A's active set; backfilling the matching ID + hash source enables restore. |
| Swapping A (missing old bundle) while an old worker admits an old-revision run after prepare lookup | Pre-publish re-check requires that old revision and blocks swap until backfilled; control run with no old references succeeds. |
| Two processes publish the same new set; an old worker admits an old run after prepare, and one process restores and publishes first | The second process sees a write conflict, restores that old revision before joining, and refuses startup if it cannot. Once published, new old-revision admissions return `Stale`. If no new pinned references appear, it joins directly without re-preparing. |

**Bundle backfill and format versions:**
- Backfilling the same directory twice in PostgreSQL leaves one bundle. Another valid directory cannot satisfy a missing key, and corrupted stored bytes for the same key are never overwritten. Backfilling alone never mutates the active set, run attempts, or pending cards.
- Legacy `BOTPROFILE1` bundles restore verbatim per header version even after `SKILL.md` front matter and 64 KiB rules; new profiles parsing `SKILL.md` front matter use `BOTPROFILE2`, preventing silent cross-version joins into the same active set.

### Leases, Waits, Cancellation, and Operator Run Control

Lifecycle: `ready → leased → succeeded | waiting | retry | dead`.

- **Cancellation:** locks target subtree rows parent-first so committed delegations join the cancellation and post-cancel delegations are rejected. Running work records the cancel request and checks before its next external effect; already-started effects track to conclusion. If the current owner finishes after cancel committed, the transaction closes as `Cancelled` and wakes any waiting parent with no new answers or replies (usage and truncation record only on successful completion). If completion committed first, its result and replies stand. Mid-run cancel checks surface `Failure::Permanent("cancelled")`, which the store confirms and closes as `Cancelled`.
- **Leases & epochs:** claims use short transactions (`SKIP LOCKED`, lease expiry, increasing `generation`). Heartbeat (checked against DB clock under row lock), completion, child creation, and tool calls verify owner, generation, and expiry without holding DB locks across external calls. Every claim (including normal resume) issues a fresh `claim_epoch`; released tokens cannot renew, checkpoint, finish, or invoke tools.
- **Retries & waits:** transient errors use bounded exponential backoff + jitter; permanent errors and attempt exhaustion dead-letter. Attempt count increments on consumed transient failures and lease expirations, not on normal completion, permanent errors, user cancellation, approval waits, or effect holds. Approval requests, child delegations, and effect holds move the run to `waiting` and drop the lease atomically; decision, superseding, expiry, child result, or operator verdict wakes the run under the same generation. Lease-expiry reclaim bumps `generation` and marks old approvals `Superseded`. Approvals stay valid across claims within `(profile, run, generation)` and re-verify approver, tool, arg digest, and expiry on consumption. Approver-only context views never expose execution tokens.
- **Operator resume & cancel:** submits observed `state`, `generation`, and `claim_epoch` for a normal root; the store verifies them under lock and checks for unsettled subtree holds. Resume applies only to dead-lettered runs (keeping logical run and effect keys while resetting attempts to `0`); cancel applies subtree cancellation to non-terminal roots. Successful commands commit transition + audit atomically: replaying the same command ID and inputs returns the prior success even if a later generation failed; same ID with different preconditions returns conflict; new ID with stale preconditions returns `stale`. Authenticated operator and fixed reason are audited; unsettled effects resolve in the hold-resolution UI ([Admin API](admin.md)).
- **Checkpoint & finish errors:** stepped-model checkpoints exceeding `MAX_DOC_BYTES` (1 MiB) or returning `Invalid` terminate immediately as `invalid_checkpoint` (preserving already-committed per-call receipts within limit); `Unavailable` retries as `checkpoint_unavailable` reusing saved plan and receipts. Finish `Invalid` / `Conflict` terminate as `invalid_result` / `finish_conflict`; `Unavailable` retries as `finish_unavailable`.

### Worker Shutdown and Lease Reclaim

1. On `SIGTERM`/`SIGINT`, `bot serve` stops claiming new runs and gives in-flight runs an **8 s grace window** with heartbeats active to finish or enter approval wait.
2. After grace, it verifies model/tool process exit for that claim, revokes its session token, and calls `RunStore::release_for_shutdown` (default **10 s cleanup budget**). Shutdown also interrupts model VM startup: preflight skips launch, and an already-starting VM stops its client first and deletes the container repeatedly until confirmed absent (preventing late create calls from leaving `stopped` VMs behind). If exit cannot be confirmed, lease expiry hands the run to the next worker.
3. `release_for_shutdown` checks non-expired lease, generation, `claim_epoch`, and cancel state under lock; returns the run to `ready` without spending an attempt (even when the active set is unchanged), bumps generation once, and supersedes old approvals in the same commit. Terminal runs and other owners are untouched; unknown-outcome `Started` writes follow hold/recovery.
4. On `SIGKILL`, crash, or failed cleanup/release, the reclaiming worker compares the prior claim's recorded **host-wide active set** against the current active set inside the claim transaction. If both are known and differ, reclaim consumes **0 attempts** and records the new set; if identical or absent (legacy row), it consumes 1 attempt (`dead(lease_expired)` on the last attempt). Active-set lookup failure leaves state unchanged and retries. Cancel, generation bump, and approval superseding apply identically.
5. **Audit outcomes:** graceful shutdown release logs `run.release` (`shutdown`); active-set replacement reclaim without attempt consumption logs `run.reclaim` (`set_replaced`).

## Walkthrough — Worker Crash Mid-Event Deletion

The broker pins the event ID and args, obtains approval, commits approval consumption + call start + audit, and launches the MCP server. When the server returns, the call and receipt complete atomically under the same call ID across restarts.

- **Unknown external outcome:** if the process dies after launching the server with no result, actual transmission is unknown. Recover only via a receiver-guaranteed idempotency key or receipt lookup; if expired or unproven, stay `held`.
- **Multi-write step ledger:** optional step receipts preserve confirmed steps (e.g., file upload) while adjudicating only the uncertain step (e.g., notification). If ≥1 step confirmed and the final tool response is lost, the entire call is held (same step key returns its prior receipt, but a restarted tool might pick a different step name for the same content). Once an operator confirms old-tool exit and receiver receipt for the whole call, the broker returns that receipt without re-running the tool ([Tool callbacks](plugins.md)).
- **Gateway replies & resume preconditions:** replies use the same rules. Operators record verdicts with an external receipt or clear proof of non-execution. Before resuming the same call ID under current auth/approval, the host closes prior-generation IPC/egress and confirms the old tool process and children exited *and* receiver non-execution is proven; host disconnect or uncertain state stays `held`.
- **Durability prerequisites:** requires PG durability, verified backups, and deduplication retention ≥ the support window. Zero intake loss across failover requires synchronous replication and fencing of the old primary. Lock outbound sends during backup restore and reconcile with receivers. Receivers lacking both receipts and idempotency keys cannot simultaneously guarantee automatic completion and zero duplicates.

## Hold Verdict Idempotency

The store assigns a `hold_id` to every hold occurrence: per call for replies, single tools, and sensitive DMs; per unsettled step for multi-step tools; and a separate call-level ID when all steps are settled yet the final tool result is unknown (legacy step-only holds without a call-level verdict target also appear here).

The Admin UI submits the `hold_id` selected during initial inspection. The store checks target, occurrence, operator, verdict, and evidence digest and commits transition + audit atomically. Replaying the same verdict on an earlier `hold_id` (e.g., after a resend also enters hold under a new `hold_id`) returns the already-applied result without touching the second hold. Reusing an old `hold_id` with a different operator, target, verdict, or evidence returns conflict. Sensitive DM verdicts never persist raw evidence or receipts.

## Gateway Handover Contract

- **Active-set fencing:** active-set revisions include both profiles and effective gateway identity/entrance mappings (changing even a CLI OS user produces a new set). Intake, approval decisions, reply `begin`, and admin verdicts verify the active set under the same DB lock as activation swaps: once a swap commits, stale entrances fail with `Stale`, while receipts for already-started sends remain accepted.
- **Status lookup:** keyed by `(profile, binding, authenticated principal, request)`. Redelivered identical input on the current entrance reuses the original run; new intake also checks target profile revision.
- **Sensitive DMs & `private_pass: true`:**
  - Acquires a lease in the dedicated private ledger, verifies tool, input HMAC, lease, and active revision immediately before call, records start once, discards raw tool output/errors, and keeps only completed/passed/denied/held status. Tools requiring approvals or step callbacks cannot be wired here.
  - A valid empty PASS allowed by `private_pass: true` commits alongside private-ledger closure and audit. If the process crashes before standard intake, recovery requires redelivery of the identical event with unchanged revision, trigger, and per-profile digest secret: matching per-profile HMAC over those plus mapped args, body, attachments, and event ID reuses the completed call and admits under `<request_id>:pass`. Rolling back to a legacy gateway holds pre-intake `passed_v2` until the new gateway is restored and the event is redelivered; legacy completions without a pass record remain private-consumed.
  - Pre-start failure or proven non-send + exit releases the lease and withholds ACK so redelivery resumes on a new claim; completion must match start; unconfirmed sends or expired leases become `Held` even without redelivery; hold verdicts store no raw receipts or notes.
- **Reply outbox & transport scoping:**
  - Replies use a separate send generation: `outbox.claim` → post-eligibility check → `begin` commit → single send. Pre-`begin` expiry reclaims; post-`begin` uncertainty holds; `settle` commits receipt + audit. Send key `(profile, run, seq)` persists across worker restarts without re-querying the worker effect ledger.
  - CLI and HTTP deliver via authenticated status lookup (no pull outbox rows; live CLI connections read the same committed result). Push intake pins the host-selected adapter ID (`reply_transport`) on the run (inherited by redeliveries and delegations), and reply rows carry destination `transport`. PostgreSQL filters `outbox.claim` by adapter `transport` and optional `(profile, binding)` before returning bodies; DM vs channel selects posting method within that adapter. Earlier `pending`/`sending` replies on the same `run` block later sequences regardless of transport. Gateway verifies binding, principal, and destination permissions under the current revision and passes that revision to `begin` (`Stale` on mismatch).
- **Migration `0026` (`crates/bot-store/migrations/0026_reply_transport.sql`) & empty `transport` rules:**
  - **Rollout & locking:** adds columns to `bot_runs` and `bot_outbox`, backfills rows, and rebuilds `bot_outbox_pending` non-concurrently (`ACCESS EXCLUSIVE` / write lock — benchmark on a production-sized clone; stop legacy writers/claimers, settle in-flight sends, migrate, then resume with the new gateway).
  - **Legacy backfill & audit queries:** official legacy Slack origin conversations and approved explicit destinations backfill to `slack`; rows lacking proof remain `transport = ''` (excluded from auto-claim). Inspect remaining rows and affected schedules with:
    - `SELECT profile, run, seq, state, reason FROM bot_outbox WHERE transport = '' AND state IN ('pending', 'sending', 'held')`
    - `SELECT s.profile, s.id, s.origin_run, s.origin_kind FROM bot_user_schedules s JOIN bot_runs r ON (r.profile, r.id) = (s.profile, s.origin_run) WHERE s.state = 'active' AND s.origin_kind IN ('direct', 'channel') AND r.reply_transport IS NULL`
    Custom fork adapters must be backfilled separately. Legacy empty-transport holds (`owner_lost`, etc.) cannot resend until `bot_outbox.transport` is filled; active user schedules from null-transport runs resume firing on the next sweep once `bot_runs.reply_transport` is populated.
  - **Missing-transport guard:** legacy/unmapped push runs (`reply_transport IS NULL`) emitting replies or schedule-close notices do not fail completion/closure; instead they immediately open the reply as `state = 'held'`, `reason = 'missing_transport'` in `bot_holds` + audit (`outbox.held`). Firing a new user schedule (`Fire`) from a push origin run with `reply_transport IS NULL` returns `Invalid`. `not_executed` resend on `transport = ''` returns `Conflict` — populate `bot_outbox.transport` before resolving as not executed, or inspect `bot_outbox.body`, deliver manually, and close as handled.
  - **Explicit-destination scope:** origin-bound schedule replies and close notices inherit the creating run's `transport`; explicit destinations are currently fixed to Slack channels (`slack`).
- **HTTP response mode:** initial intake persists method, path, and response mode; `/_requests/{id}` renders in the original mode. Response mode is excluded from the retry hash; legacy HTTP requests without stored mode render as `accepted`.
- **Reply payload & card rules:**
  - Reply body is a JSON object with ≥1 of `text` (≤12,000 chars), `attachments` (≤8 files, ≤8 MiB total decoded, each `{name, type, data}`), or `approval` (`nonce`, `title`, `lines` ≤20).
  - On push conversations (`direct`/`channel`), line-leading `<attachment name="attachment.md">...</attachment>` blocks in root agent answers split into `text` plus following `text/plain` `attachments` replies in the same commit (recording `truncated` if char/byte/reply caps are exceeded).
  - `approval` is permitted only on broker-created cards committed with the approval ledger (rejected in `Finish.answers`). Oversized approval cards that would truncate details are withheld and link to the authenticated approval detail view instead. Clicking an approval button runs `Broker::decide` then refreshes the card from ledger state (refresh failure never re-executes the decision).
- **Send failures & retries:** after `begin`, pre-write connection failures or receiver-guaranteed non-execution rejections (rate limits, auth-layer bot token rejection while rotating cached tokens) settle as `NotSent`. Token rejection evicts the cached token so the next attempt reloads credentials (repeated rejections obey the retry limit). Hidden gateway/HTTP retries are disabled. The store commits retry scheduling + audit only from matching send generation `sending`; next claim waits `max(backoff, Retry-After)` under a new generation with the same `(call, seq)` and marks terminal failure for operators once exhausted. Response loss, timeout, or partial attachment upload after writing the request is `held`.
- **Approvals, revocations, & revision fences:**
  - Condition changes on click mark only the current call's nonce `Superseded` with audit + wake in one commit (re-clicking wakes nothing). Post-approval revocation and `start` serialize in one ledger: revoke first → `Revoked`; consume first → revoke returns `Conflict`.
  - Schedules and delegated children resolve their current approver from the root's recorded agent and trigger ID (schedules sharing a service principal follow their own approvers); missing cron, changed principal, or non-human approver denies approval-gated calls.
  - Broker passes `StartCall.checked_revision` (active profile revision); the store serializes profile swaps against call start (`Stale` without consuming approval if profile changed/deactivated first; otherwise consumes approval + starts + audits atomically). `Outbox::begin` enforces the same fence.
  - `memory.share` approval hashes canonical JSON `{"id": ..., "sha256": ...}` (`sha256` = lowercase hex SHA-256 of preview body UTF-8 bytes). The store locks that row, verifies owner and current digest (`Conflict` without consuming approval if modified), then commits consumption + profile visibility + completion + audit together. Replaying a completed call never re-publishes later edits; legacy ID-only pending approvals need a new card.
  - `finish` accepts only the current owner and generation. A new owner seeing an old generation's `Started` recovers only with receiver proof + confirmed old runner exit (otherwise `Held`, preserving original owner, generation, and effect receipts). Operator notes alone never prove non-execution; result blocking cannot overwrite an uncertain write as denied.
  - Audit entries append to one host-wide chain in the same commit as every state change and are verified sequentially by operators.

### OpenAI-Compatible Intake and Lookup

`ApiRuns::accept` verifies the active set and checks for an existing intake. For a new key, it acquires the entrance lock, re-checks, verifies profile revision and active-run capacity, and commits intake, run, and audit atomically.

- **Key:** `(profile, entrance, authenticated principal, Idempotency-Key)` across bindings and models. Matching key + input digest + model attaches to the existing run (in-flight or terminal) with 0 new slots, runs, or audits; differing input or model returns `Conflict` without writing.
- **Capacity:** active runs are root runs in ready, leased, waiting (approval, child, hold), or retry (delegated children share the root's slot). New intakes per `(profile, entrance)` serialize via `pg_advisory_xact_lock` to enforce `max_active_runs` (`1`–`256`). Full returns `Capacity`, stale revision returns `Stale` (nothing written). Already-committed retries attach without waiting on the entrance lock.
- **Uncertain commit:** if the commit reply is lost, the server re-queries by key; if still unknown, it returns HTTP 422 `request_outcome_unknown` with the applied idempotency key so the caller can retry with the same key and body. SQLSTATEs classified as definite rollback: integrity violations (`23xxx`), `40001`, `40002`, and `40P01`.
- **Lookup:** matches all four tuple fields and returns pinned model, binding, agent, revision, input digest, and state: in progress (`"Request is still in progress. Query again with the same Idempotency-Key and request."`), awaiting approval (`"Awaiting approval. Query again with the same Idempotency-Key and request."` — only when an actionable human approval exists on the root or a waiting descendant, and for Slack cards only after a confirmed `posted` receipt), held, approval blocked (`no_approver`, `approver_revoked`, `route_changed`, `approval_dm_unavailable` — read strictly from stored verdicts), final text, failed, or cancelled.
- **Usage & truncation:** final text is the worker's own run text; host-truncated (>12,000 chars) replies record `truncated` in the finish commit. Token usage is saved in the same commit only on the first claim, with no delegated child created, and when every model request reported tokens (`claude-cli`: all requests report positive input+output summing to the CLI total; `codex-cli`: non-resumed `turn.completed` with positive input+output; `agy-cli`: non-resumed `result` with positive input+output matching per-step sums when present; direct API `anthropic-api`/`openai-api`: every turn's upstream response reports summable usage) — otherwise left empty, never zero-filled. Direct API output limits fail with `output_limit` without partial text or truncated tool calls; VM CLI continuations store the final text and report `length` only when host-truncated. JSON returns HTTP 422 `run_failed`; open SSE ends with a `run_failed` error event ([OpenAI-compatible API](gateway.md#openai-compatible-api)).
- **Isolation:** standard webhook/CLI intake and status cannot read or adopt OpenAI-entrance intake rows; colliding `(profile, binding, principal, request_id)` never creates a new run.

### Completed Call Redelivery

After checking current permissions, the broker looks up completed calls via `CallLedger::lookup`: normal tools match name + canonical args hash; completed `memory.share` matches name + stored receipt memory ID (later edits/deletions keep the receipt intact, while pre-completion edits abort approval and sharing). Both initial and redelivered results verify PG's active profile revision and current claim via `active_revision(attempt)` immediately before returning; on swap, cancel, lease expiry, or lookup error, the result is withheld while the confirmed receipt stays in the ledger.

### Private Memory Writes, Embeddings, and Auto-Capture

- **Atomic `memory.save` and `memory.forget`:** Both acquire a per-owner advisory lock and commit memory mutation, optional approval consumption, call completion, and audit in one transaction. `memory.save` enforces the 500 active non-seed owner quota, deduplicates exact and `cos >= 0.94` near-matches, and allows non-destructive `supersedes` strictly on the owner's `private` entries. `memory.forget` wipes `body` and embedding columns when setting `deleted_ms`.
- **Lazy embedding backfill:** Before turn-start hybrid recall, the worker backfills up to 16 unembedded rows per turn outside DB locks, verifying `body_sha256` and active-set fencing on write. Provider outages fall back cleanly to lexical recall; `Error::Stale` aborts the attempt as `Lost`.
- **Post-answer `memory_judge` auto-capture:** Runs best-effort before `finish` on eligible non-private 1:1 human DM/CLI root `message` runs with double active-revision fencing, local + `contains_secret >= 0.50` filtering, and key-leak scanning. The store re-verifies active revision, leased attempt, and non-cancel state, and skips when an explicit `memory.save`/`memory.forget` already completed in the same run; transient errors never fail or retry the run.

## Chat-Created Schedules

User schedules persist separately from static profile triggers. The host pins the authenticated human, agent, origin binding/conversation/thread, permission ceiling, and `private` flag (delegations and scheduled runs never escalate to service-account or approver privileges). Each firing checks current member, agent, and binding and admits with `ceiling ∩ current permissions`; policy edits also apply immediately before tool calls and replies. Explicit destinations require approval on creation (recipient + instruction) and separate approval on delivery (recipient + rendered content). Private memory visibility and approval routing follow the origin conversation.

- **Commit & sweep:** create/cancel commit atomically with call completion. The scheduler evaluates one-shot and cron occurrences by DB clock, committing `(profile, schedule_id, scheduled_utc)` run, cursor, and next due time together (cancel-first prevents new occurrences; admit-first stays trackable; concurrent schedulers converge to one run). Late occurrences catch up with static-cron DST and batch rules. Profile reactivation never deletes user schedules.
- **Limits & `<quiet>`:** max **32** active schedules per `(profile, owner)`; max **8 KiB** UTF-8 instruction; automatic wake depth caps at **8** (delegation cannot reset depth); tool follow-up wakes commit atomically with parent tool receipts (redelivery creates once). A `quiet: true` schedule succeeding with no attachments and final text equal to `<quiet>` records the result and suppresses reply (normal reports, approvals, failures, and holds follow normal paths).
- **Creation seal & redelivery:** creation cards and PG verify SHA-256 of canonical JSON (JCS) from `ScheduleSpec::seal_create()` covering raw instruction, `quiet` (default `false`), explicit destination, one-shot UTC milliseconds matching stored precision (e.g., `.250000001Z` → `.250Z`), or 5-field cron + resolved IANA timezone (defaulting to the pinned profile revision's timezone). Redelivery checks completed receipts before future-time checks: legacy completed calls return their receipt on matching permissions and digest, while incomplete calls with a mismatched legacy card require fresh approval.

### Delegation Results and Schedule Publication

Worker and MCP relay share `delegation_scopes` to check direct lineage, requester role, and agent ceiling across both revisions (intersection with run scope must contain `run`). The store creates the child and moves the parent to `waiting` after verifying active revision and current claim. Returning child completion re-checks original child, instruction, and that every granted scope remains allowed.

Regardless of which role publishes, active set, memory seeds, and static trigger changes commit together (closing removed profiles' static schedules at one DB timestamp while preserving prior interval cursors). Joining processes never rewrite static schedules.

### Explicit-Destination Scheduled Replies

`ScheduleRoutes::resolve` pins host-registry-verified Slack team and channel into `conversation.id`, preserving `team_id/channel_id` and thread from creation card through delivery outbox. Checked at fire (scheduler), approval open/consume (broker), and send (gateway); missing or rebound routes prevent new runs and drafts. Both creation and delivery cards verify the host active set used to resolve the target inside the card-creation transaction (a committed swap prevents opening stale-route cards).

- **Draft sealing:** the worker freezes resolved destination and final answers once; the creator must hold `schedule.create` at request time and currently. Explicit-destination sends always require owner approval (noting private-memory taint on the card if present), showing binding, conversation kind, recipient ID, thread, and all answers in send order. `DeliveryDraft::approval_json()` encodes `activation`, `target` (`null` thread if absent), and `answers` (`text/plain` attachments only with name, type, and full UTF-8 text — e.g., `data: "aGk="` renders as `text: "hi"`; unreadable or oversized bundles are rejected whole).
- **Approval & enqueue:** the card's **`Delivery draft`** line carries this full JSON; `DeliveryDraft::sha256()` hashes those UTF-8 bytes. Worker-only tool `schedule.deliver` uses revision `builtin-1` and per-run call ID `schedule-deliver`. PG verifies active set and stored draft digest against the approval and commits consumption, destination outbox enqueue, and run completion atomically (0 sends on body change, rejection, expiry, or route drift). `DeliveryDraft::queued_receipt()` returns queued count (e.g. `{"queued":2}`, attachments included); on lost reply, the latest claim reads the original receipt bytes under the same active set while the gateway tracks actual delivery separately.

| Pre-enqueue verdict | Occurrence outcome |
|---|---|
| Owner rejects or revokes before consumption | `delivery_rejected` (recorded owner decision wins even if target vanished or registry lookup fails; 0 origin notices). |
| Current registry rejects target or binding points to another team/channel | `delivery_unrouted` (completes occurrence with 0 destination sends; posts once to origin conversation: `"Scheduled delivery route changed; this reply was not sent."`). |
| Registry temporarily unavailable | Retries same occurrence (preserves approval and draft; 0 sends, 0 notices). |

Unsent settlement verifies the current claim or approval card + owner, re-reads any owner decision in the same transaction, and commits final reason + receipt (redelivery returns the committed receipt; races with enqueue keep the first commit). Origin notices never include the draft or destination; restoring a route never resends a settled occurrence, and recurring schedules re-check on the next firing.

### Owner-DM Approval Card Delivery (Slack)

All Slack channel-origin approvals (general tool calls, delegated child runs, and explicit-destination schedules) deliver full approval cards via the owner's 1:1 DM so tool names, parameters, and drafts never appear in public channels. Gateway and store enable buttons only after confirming card posting on the same Slack installation; the origin channel/thread gets a short notice only. Local operator tokens carry no Slack-owner approval authority.

1. **Prepare:** store records one delivery job binding `nonce`, raw card + hash, owner, and origin profile/binding/installation (card is not clickable or decidable yet). Outside the DB transaction, the gateway confirms authenticated requester == designated approver Slack user, opens a 1:1 DM on that installation, and posts the full card (channel thread `ts` is not copied to DM).
2. **Post confirmation:** Slack's confirmed DM channel + message `ts` form the receipt. The store re-verifies receipt, active set, generation, and expiry, then atomically opens the card and enqueues one origin channel/thread line: `"Approval request sent via DM"` (no tool, args, draft, recipient, hash, buttons, or approval ID). Lost commit responses re-read by `nonce`.
3. **Click:** verifies authenticated Socket Mode session (or request signature + timestamp on signed entrances). The broker re-checks approver, args, and current policy; only a click matching stored installation, team, DM, `ts`, `nonce`, and clicker on a live card can decide (must work even when only the channel binding is active and general DM ingress is disabled). Copied messages or pre-receipt clicks never consume approval.
4. **Delivery failure:** inability to open the DM or post the full card closes the request as `approval_dm_unavailable`; mid-post disconnects or restarts (uncertain outcome) keep buttons disabled and never auto-repost. The store atomically wakes the run and commits result + audit once (lost settlement responses query the existing commit and retry settling the same job). Creation approvals create no schedule; delivery approvals complete that occurrence unsent with `{"status":"approval_dm_unavailable"}` (recurring schedules retain future occurrences). Both emit 0 success notices and 0 destination sends, posting one fixed origin channel/thread line in the same commit:
   - Confirmed undeliverable: `"Approval request could not be sent via DM; request closed."`
   - Uncertain post outcome: `"Could not confirm approval DM delivery; request closed."`
   Neither carries draft, recipient, hash, or approval ID, and replay emits the notice once. If a card actually posted in the uncertain case, clicking it cannot decide and shows `"This request is already closed."`

- **Timeouts & retries:** claimed jobs unstarted after **90 s** reclaim; `sending` jobs unsettled after **120 s** close as uncertain. Confirmed transient failures (pre-send connect error, rate limit, bot token rejection) wait `max(Retry-After, 5 s)` and close as `approval_dm_unavailable` if the **6th** claim still fails. If the card expired, decided, invalidated, or the active set swapped mid-flight, it stays closed with no public notice. System closures record system rejection (not owner decision) and one `approval.dm_unavailable` audit.
- **Attempt fencing:** `begin`, `settle`, and `block` match `(nonce, attempt)` against the locked job row so stale retries cannot disturb active sends; cleanup verifies attempt, state, and deadline under lock; human clicks still use `nonce` + post receipt (applies when all participating gateways enforce attempt matching). Already-opened cards and destination revocations still follow `delivery_rejected` / `delivery_unrouted`, and DM confirmation still requires a separate valid delivery approval before enqueueing to the destination outbox.

### OpenAI-Compatible General Tool Approvals

Approvals for intake-backed API roots (and delegated descendants) open under the host active-set lock: the broker's checked set must still be current at commit, or neither local card nor Slack delivery job is created (`Stale`); unpinned handles are rejected. The requester must match the intake-authenticated principal, and Slack DM cards require the delivery installation (`team`) recorded on the intake row (never broker-supplied).

The delivery job pins origin binding, requester, revision, and `team` (HTTP origins queue no channel notice). Missing or mismatched receipt `team` yields no receipt, decision target, or awaiting-approval state and closes as `route_changed` (even the designated user's click cannot consume it). Gateways detecting route drift before posting call `block` with `no_approver`, `approver_revoked`, or `route_changed` (returned verbatim on lookup); late verdicts never close an already-open card.

### User Schedule Closure Reasons and Sweep

The scheduler freezes sweep upper bound `end` on the first page and paginates (`after < pos <= end`). `end` is a key bound rather than a row snapshot: newly due rows past `end` wait for the next sweep, while rows between cursor and `end` may appear in the current sweep. Corrupt rows (unparseable cron or destination kind) count toward page size and are returned separately with ID, cursor, next-check time, and close reason (`due` is read-only).

- Corrupt rows are closed individually with the store's reason (`invalid_timing` or `invalid_destination`, using read cursor + current revision if active, or `profile_inactive` if inactive), interleaved in store order `(at, profile, id)` so later corrupt locks never delay earlier valid schedules. Store outages are never treated as corruption.
- Each schedule gets at most one turn per sweep: recurring schedules whose next due time still falls inside the sweep wait for the next sweep, while a per-sweep "more due" flag carries across pages so the next sweep starts immediately without sleeping the default interval. Mid-page store errors skip only already-processed IDs and retry the failed and remaining schedules; sweep completion clears the seen set.
- Closure commits one `schedule.close` audit atomically; stale cursors return `Stale` (no audit or notice); replaying the same closure preserves the initial reason.

| Reason | Condition |
|---|---|
| `profile_inactive` | Store confirmed the profile is inactive. |
| `member_revoked` | Owner is no longer a member of the current profile. |
| `binding_revoked` | Origin conversation binding was revoked. |
| `agent_revoked` | Target agent was removed. |
| `permission_revoked` | `ceiling ∩ current permissions` no longer contains `run`. |
| `invalid_timing` | Stored cron expression or timezone cannot be parsed. |
| `invalid_destination` | Stored explicit destination kind cannot be parsed (no origin notice). |
| `finished` | No further occurrences (closes in the same commit that admits the final occurrence's run). |
| `unrouted` | Explicit destination vanished from the host registry or maps to a different target. |

Only `unrouted` posts to the origin conversation: `"Scheduled message destination is no longer registered; schedule stopped."` Closure, audit, and single notice commit together (deduplicated across competing schedulers and lost replies); transient registry outages keep the cursor and retry.

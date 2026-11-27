# Roadmap & Public Acceptance Criteria

English | [한국어](ko/roadmap.md)

Acceptance gates for the local demo and every runtime role. PostgreSQL tests read a dedicated test database URL from `BOT_TEST_DATABASE_URL`. ([Release Checklist](release.md))

| Stage | Owner Crates | Acceptance Criteria |
|---|---|---|
| 1. Profiles | `bot-profile`, `bot-core` | Standalone build from public checkout. Add a second profile and grandchild agent on the same binary. Reject broken links, duplicate bindings, and missing servers before activation. |
| 2. Queue & Schedules | `bot-store`, `bot-runtime` | With two workers and two schedulers: idempotent request/cron intake, lease takeover, DST & outage catch-up, dead-letter resume, and duplicate-free parent wakeup. |
| 3. Tools | `bot-broker` | Models have zero secrets and no network; tools receive only declared secrets and egress hosts. Reject sibling-secret leaks, proxy bypasses, cross-principal approvals, argument tampering, replays, and revoked scopes. |
| 4. Ingress & Replies | `bot-gateway`, `bot-runtime`, `bot-cli` | Verified intake and origin replies across CLI, Slack, and HTTP. Reject unregistered users and rate-limit breaches. Delegation and memory enforce profile/owner boundaries. |
| 5. Operations | `bot-broker`, `bot-store`, `bot-admin` | Interrupt single-step delete and multi-step tools before and after send: verify original receipt or `held`, zero automatic duplicate sends. Verify Admin UI holds, audit chain, OTLP failure tolerance, and backup restore. |
| 6. Public Demo | `bot-cli` | Reproduce the README local demo on a clean host: CLI chat, direct cron call (0 model calls), and self-approved single delete behave identically in `--role all` and split-role modes. |

## Security & Recovery Acceptance Matrix

| Scenario | Required Outcome |
|---|---|
| Same-named synthetic secret with different values in two profiles | Each tool receives only its own value and rejects cross-namespace paths. Connections outside operator egress caps or disallowed model providers are blocked. |
| Sending a message to a different recipient on the same service | Zero sends before a separate approval displaying recipient and body; zero sends if body, attachment, or recipient changes after approval. |
| Risk escalation or worker generation change while awaiting approval | Old buttons/channels rejected; new approval consumed at most once. |
| Overlapping events, private DM, repeated HTTP query | Only the earliest tool runs with valid args. Private DMs stay private by default; only a valid empty PASS from a selected `private_pass: true` trigger creates one regular run with the original question and attachment refs. HTTP returns consistent sync status/body or `202` followed by the identical result. |
| Worker crash immediately before external write | Zero re-executions until the old tool exits and non-execution is verified; resumes with the same `_meta` call ID afterward. |
| Final call response lost after step confirmation | Hold queue preserves committed step receipts and resumes only via a call-level receipt; whole-call `not_executed` is rejected, and cancel closes open tool holds. |
| Race between delegation and cancel, or late completion | Cancellation covers already-committed children and rejects late delegations. If cancel commits first, late completions emit zero replies and wake the parent once. |
| Retry budget exhausted by transient errors and lease expiries | Final attempt count recorded on the `dead` row; operator resume restarts from attempt 0. |
| Retry limit 1 in PG; shutdown handover times out on active-set swap lock | After swap commits, a new worker claims the expired lease and succeeds at attempt 0. Re-expiring on the same set or lacking set metadata consumes one attempt. |
| Expired `beat` vs incoming events | Expired beat keeps serialization-key ownership. Due retries, expired leases, and new runs are claimed by ready timestamp while skipping locked rows. |
| Corrupted target on claimed run | Quarantines only that run with raw payload and fixed reason; unrelated runs and waiting parents proceed. DB errors roll back the claim. |
| Checkpoint cap & finish conflict | Plans/results exceeding the 1 MiB store cap and repeated finish conflicts terminate in one attempt; transient store errors resume from saved plans/receipts. |
| `SIGTERM` to a worker on the same active set (retry limit 1) | After 8 s grace, exits and revokes sessions, returning the run to `ready` at attempt 0; another worker completes it. Zero writes from the stale claim overwrite the new owner. |
| Reply route revoked before recurring schedule fires | PG records per-reason audit codes; only `unrouted` posts one fixed notice to the origin chat, even under dual-scheduler races/retries. |
| Tool aborts during step-receipt pre-lookup | Re-lookup on the same call returns `none` with zero steps, holds, or audits. Different args on an existing step return `conflict`; confirmed steps return the original receipt. |
| Admin resolves held step as `not_executed` and worker resumes | Lookup returns `held`, `resolution:"not_executed"`, `send:false`. Only the first re-claim on that step gets `send:true` and consumes the resolution. Stale same-gen claims get `stale`, unregistered calls `denied`, missing steps `none`, and cross-profile IDs stay isolated. |
| Operator resumes/cancels from run detail in PG | Root normal runs only. Retrying a command whose response was lost transitions and audits once. Earlier claims cannot cancel a newer claim. Cancel cascades to subtrees; unresolved holds direct to the hold screen. |
| Per-agent built-in tool gating | Omitted, empty, or undeclared children get zero built-ins; only explicitly listed roles get `read`. Shell, write, and web declarations fail before activation. |
| Real Claude CLI + mock model streaming -> MCP tool -> model round-trip | Exactly 1 tool execution; provider keys and session tokens stay within their boundaries. Model VM network/FS isolation is verified in separate host tests. |
| Real Claude CLI + mock upstream + PG worker swap during write approval | Resumes same session in a fresh `HOME`: 1 approval consumption, 1 tool execution, 1 receipt; follow-up model turn keeps the original tool-use ID and real result. |
| Same setup with parent delegating via MCP to direct child | Single worker completes child; restarting parent leaves 1 child run and 1 execution, delivering the child result back to the original delegation. |
| Same tool & args planned twice plus duplicate request delivery | Distinct positions each run once; redelivery at the same position returns its cached result. |
| Hold decision response lost, then same reply/tool/step held again | Retrying the first occurrence ID returns cached success with 1 audit; the new occurrence stays open until decided separately. |
| Memory search across own DM/CLI, other users, channels, and delegated children | Only own DM/CLI reads `private` items; channels never leak private memory into parent or child model prompts or replies. |
| Search tool called after reading `private` memory | Zero external requests before approval of outbound arguments, even across restarts or delegations. |
| Private DM intake and redelivery | Runs only the designated tool once; default private mode keeps raw input/output out of ledgers, models, and traces. Crashing between a valid empty PASS (`private_pass: true`) and regular intake redelivers once under `:pass` on the same revision/trigger/secret. Pre-start failures reclaim with identical input; uncertain/expired calls hold without redelivery. |
| Attempt by another user to set/edit/delete memory owner | `owner` stays bound to the authenticated principal; zero mutations to other users' items. |
| Sharing memory to `profile` scope | Only the owner inspects full content; editing after card creation leaves it unconsumed/unshared; retrying after a lost response emits 1 share audit. |

Public CI enforces `rustfmt`, Clippy, tests, schema/example validation, a 600-line file limit (excluding generated `Cargo.lock`), allowed licenses (`MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Unicode-3.0`, `Zlib`, `BSL-1.0`), and secret scanning (including pre-release Git history). Real profiles and private identifier dictionaries run only in local release checks; production cutovers additionally pass dry-run and rollback rehearsals.

## Additional Pre-Release Verification

- CLI explicit-destination refusals exiting cleanly at retry limit 1 are tracked via `claude_attempts`; recording delegation-arg errors under the same marker remains open. Pinned-revision restoration on redeploy and isolated termination of corrupted schedules are verified against real PostgreSQL. ([Reliability](reliability.md))
- **Conversational schedules:** One-shot and recurring CLI schedules processed by two schedulers — verify 1 creation per redelivery, 1 run per due tick, cancel-vs-intake races, DST, member revocation, cross-user query/cancel denial, and explicit `<quiet>` completion.
- **Slack explicit-destination schedules:** `schedule.create` targeting a registered channel in the same profile requires owner approval to create the schedule and a second approval per firing before emitting one outbox reply (0 rows before each approval). Removing the channel stops future firings/deliveries; CLI/HTTP entrances reject explicit destinations.
- **Owner-DM approval cards when general DM ingress is off:** Creating a schedule from a public channel opens each of the two cards once in the owner's DM and posts one status line (posted / failed / uncertain) in the origin channel. Clicks from other users/DMs or missing DM receipts produce 0 consumptions and 0 deliveries; redeliveries do not duplicate cards, notices, or sends. ([Reliability](reliability.md))
- **Tool follow-up checks:** Interrupting immediately before/after result persistence preserves the ledger receipt alongside one one-shot schedule and halts at the chain cap.
- **Tool config precedence:** Verify parent settings, child overrides, and sibling isolation on real tools; reject secret env collisions and missing required keys before launch.
- **Persistent tool files:** Validate behind an opt-in adapter (owner-only read across restarts, cross-server/model block, symlink-escape prevention, concurrent writes, hard quota) before migrating profiles that require it.

## Post-v2.0 Model Settings

- `providers.<alias>` supports `effort` across `claude-cli` (default `medium`, 8,192 max output tokens), `anthropic-api`, and `openai-api`, plus `max_tokens` (and `max_completion_tokens` on `openai-api`). Future work adds per-agent `effort` and output-token overrides bounded by host caps.
- Pi CLI (`kind: "pi"`) is unsupported in v2.0: unused runner code was removed (preserved in Git history) and `kind: "pi"` remains rejected in host config until isolation, resumption, and session-token boundaries are validated.

## Time Library Migration

Before the next maintenance release, migrate cron iteration and backward catch-up to `jiff`, drop SQLx's `chrono` feature in favor of SQLx's `time` types, and diff DST gaps, repeated wall-clock times, half-hour offsets, persisted UTC cursor resumptions, and dual-scheduler schedules against the current implementation. Once `chrono` and `chrono-tz` leave the dependency tree, remove both maintenance exceptions and re-run all four `cargo-deny` checks. ([RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543.html) · [RUSTSEC-2026-0555](https://rustsec.org/advisories/RUSTSEC-2026-0555.html))

## Deferred Decisions

Items deliberately left out of current scope, with explicit revisit triggers so partial implementations are not added ad hoc.

| Decision | Why Deferred | Revisit Trigger |
|---|---|---|
| DB-clock approval expiry | Broker (`crates/bot-broker/src/service/gate.rs`, `crates/bot-broker/src/service/decide.rs`) checks expiry on local clocks while store enforces DB clocks; unifying requires changing `OpenApproval`, `CallLedger`, cards, and test doubles. Assumes NTP sync across hosts. | Clock-skew expiry anomalies on multi-host brokers, or when changing `CallLedger::open_approval`. |
| Batched approval expiry sweeps | `crates/bot-store/src/queue.rs` sweeps all expired approvals before claiming and waits on per-run `FOR UPDATE` (`crates/bot-store/src/approvals.rs`). Bounded batches with `SKIP LOCKED` need revisit cadence and partial-index tuning. | Claim latency from mass expiries or lock contention shows up in DB profiling. |
| Exhausting empty closed cron intervals | Closed windows with no firing before `until_ms` keep `until_ms > cursor_ms`, pinning old bundles (`crates/bot-store/src/schedule.rs`, `crates/bot-store/src/profile_bundles.rs`). Advancing cursors unsafely risks permanent missed ticks; needs `ScheduleStore` extension and boundary tests. | Uncollected old bundles from edited schedules become an operational issue, or when extending `ScheduleStore`. |
| Cron DST wall-clock "once" policy | A 5-field cron expression cannot distinguish every-minute jobs from once-per-local-day reports; altering `crates/bot-runtime/src/cron.rs` defaults breaks existing cursors. Needs an opt-in field, interval semantics, approval sealing, and migration. | Explicit demand to avoid twice-yearly DST skips/duplicates for daily reports. |
| Explicit schedule destination transport column | General replies and outbox store `reply_transport`/`transport`, whereas explicit schedule destinations only allow Slack channel bindings and hardcode `slack` in `crates/bot-store/src/delivery.rs`. Adding a column prematurely complicates seals and re-validation. | Adding a second push transport for explicit scheduled destinations (`ScheduleRoutes` + approval seals). |
| Enforced token budgets | Currently bounded by turn/time limits and output-token caps; no cross-path store budgeter covers non-reporting upstreams or both CLI proxy and direct API paths, and per-model pricing tables are high-maintenance. | Need to hard-cap token spend per run/profile across deep delegations with a price-free token limit and unreported-usage policy. |
| Automated post-turn memory consolidation | Explicit `memory.search`, `memory.save`, and `memory.forget` with hybrid embeddings, quotas, deduplication, and `supersedes` are shipped; background LLM fact extraction from user-only turns adds extra model latency/spend and prompt tuning. | Confirmed demand for zero-tool passive fact extraction beyond model-initiated `memory.save`. |
| `bot eval` (offline profile evaluation) | Intersects the boundary excluding `testing` doubles from production binaries (optional feature vs separate crate). Today `bot validate` and `bot map --json` diffs catch scope/tool regressions. | Recurring CI demand to test scenario auth/approval/denial trajectories without DB or external APIs. |
| `bot validate --json` | Profile checks emit stable `Issue.code` values, but host config errors remain free-form strings; needs a versioned JSON schema and host error-code contract first. | Editors or CI pipelines need machine-readable validation output. |
| `host.schema.json` | Host validation is mostly semantic/path/fingerprint checks across 9 modules, and `.github/scripts/docs_contract.py` already guards documented keys. Rewriting parsing schema-first costs more than it returns today. | External tools demand autocomplete/schema validation for `host.json`, or during a duplicate-key parser consolidation. |
| Port conformance testkit | Broker doubles hit circular-dependency constraints; extracting `bot-core` fakes and shared PG conformance macros requires restructuring tests across multiple crates. | Repeated drift between fake stores and PostgreSQL, or adding a second persistence backend. |
| Clippy function-length thresholds | `.github/scripts/check_contracts.py` already enforces the 600-line file cap. Enabling `clippy.toml` length/complexity thresholds requires a phased ratcheting plan for existing functions. | Oversized single functions inside 600-line files become a recurring review bottleneck. |
| Model VMs on Linux | Linux hosts can run zero-VM direct APIs (`anthropic-api`, `openai-api`); a Linux `claude-cli` VM adapter needs dedicated isolation, network-block, and resume verification. | Confirmed operational need to run local `claude-cli` on Linux instead of direct APIs. |
| External MCP client library | The in-house client (`crates/bot-broker/src/mcp.rs`, `crates/bot-broker/src/mcp/`) passes size, header, and schema-hash-pinning hardening tests; no spike yet proves an external crate preserves these guarantees. | Tracking new MCP transports/auth/versions costs more than maintaining the internal client (gated on passing existing hardening tests). |
| Data retention / pruning policy | Prematurely purging audit chains, open holds, idempotency keys, or referenced revisions risks duplicate runs or broken audits; requires per-table boundaries and redelivery-safety proofs. | Completed run, checkpoint, or outbox growth measurably impacts DB storage or query latency (start with dry-run counts of unreferenced terminal rows). |
| Progressive skill body loading | Loading skill bodies lazily changes read boundaries, scan limits, and revision hashing. Keeping `SKILL.md` front matter `description` in structs and injecting bodies within the 64 KiB per-agent cap (`crates/bot-runtime/src/context.rs`) is currently sufficient. | Agents routinely hit the 64 KiB skill body cap and measurements justify summary-first lazy loading. |

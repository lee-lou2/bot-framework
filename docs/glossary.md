# Glossary

English | [한국어](ko/glossary.md)

Maps documentation terms to code identifiers and flags overloaded names across crates. Items marked **Watch out** are frequent sources of misreading. Pair with the "Where Things Live" table in [AGENTS.md](../AGENTS.md).

## Execution

| Term | Code | Meaning |
|---|---|---|
| run | `RunId`, table `bot_runs`, `RunState` | Unit of work created by one accepted request. Root runs have `root = id`; delegated children carry `parent` and `parent_step`. States: `Ready`, `Leased`, `Waiting`, `Retry`, `Succeeded`, `Dead`, `Cancelled`. |
| claim | `RunStore::claim`, `RunContext` | Action (and context) of a worker acquiring a ready run. Every claim increments `claim_epoch`. |
| lease | `lease_until_ms`, `RunStore::renew` | Ownership deadline held by the claiming worker, extended by heartbeats via `renew` and taken over on expiry. |
| generation | `Generation`, `bot_runs.generation` | Fencing token that invalidates approvals opened by older generations. Increments on claim unless `keep_generation` is set — kept only on wakeups from approval/child/hold decisions and the first claim after graceful shutdown handover (which already bumped it). Incremented on first claim (`0 -> 1`), transient retry, lease-expiry takeover, and handoff to the next candidate after `bot/pass`. |
| `claim_epoch` | `ClaimEpoch`, `bot_runs.claim_epoch` | Per-run monotonic counter incremented on **every** claim (including normal resumptions) to block stale writers within the same generation. |
| attempt | `Attempt` | Identity of one claim `(profile, run, generation, claim_epoch)` used for ownership checks. When `claim` is `None`, it is an identity-only handle (approvals, UI, traces) and cannot authorize worker writes. **Watch out:** `bot_runs.attempts` counts consumed failure/lease-expiry retries, not this struct. |

## Ingress

| Term | Code | Meaning |
|---|---|---|
| request | `RequestId`, `RequestKey` | Caller-supplied idempotency key; duplicate deliveries resolve to the existing run. |
| entrance / adapter | `Adapter`, `Entrance` | CLI, Slack channel, Slack DM, HTTP webhook, or OpenAI-compatible HTTP. `Adapter` (`bot_core::profile`) is the declared binding kind; `Entrance` (`bot_gateway::entrance`) is the gateway's alias-level entry point. |
| binding | `BindingId` | Profile declaration connecting one entrance to one agent. |
| normalized input | `Inbound` | Transport-agnostic payload produced after adapter authentication (`bot_gateway::inbound`). |
| ingress | `bot_gateway::ingress` | Gateway pipeline executing `verify -> normalize -> durable accept -> ack`. |
| accept | `Accept`, `Envelope`, `RunStore::accept` | Committing a verified envelope to PostgreSQL to create a run; acceptance equals DB commit. |
| intake | `bot_core::intake` | **Watch out:** Not the accept step itself — the module defining intake status queries, private-input handoff, and reply-ledger ports. |

## Egress (Replies)

| Term | Code | Meaning |
|---|---|---|
| answer | `Finish.answers` | Reply payloads committed in the same transaction as run completion. |
| outbox | `Outbox`, `Outgoing` | Durable reply-dispatch ledger populated atomically with completion. |
| reply | `ReplyBody` | Response content delivered back to the originating conversation (`bot_gateway::reply`). |
| delivery | `Deliverer`, `Sender` | Claiming an outbox row, checking the active revision, pushing, and settling (`bot_gateway::delivery`). `Sender` is a push adapter (currently Slack); CLI and HTTP read replies via status queries rather than outbox. |
| `Reply<'a, T>` | `bot_core::port::Reply` | **Watch out:** Not a chat reply — the boxed async return type alias (`Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>`) used by port traits. |

## Overloaded Names

| Name | Distinct Meanings |
|---|---|
| `cli` | `bot_core::cli` defines **VM model CLI tool-call positions** (`claude-cli`, `codex-cli`, `agy-cli` checkpoint keys) and pre-execution refusal records (`CliRefusal`, also shared by direct API workers); `bot_gateway::cli` is the Unix-socket CLI entrance; `bot-cli` is the `bot` binary crate. |
| `Sandbox` | `bot_core::sandbox::Sandbox<ModelSpec>` isolates model VMs (`ContainerSandbox`); `bot_broker::sandbox::Sandbox` isolates MCP tool servers (`SrtSandbox`). No shared trait links them. |
| `Delivery` | Outbox dispatch outcomes in `bot_gateway::delivery`; scheduled-reply port bundle in `bot_runtime::worker`; operator-secret output target (terminal vs file) in `bot-cli`'s `serve::admin`. |

## F1–F4 in Migration Comments

Internal identifiers for four features shipped in v2.2. Removed from Rust code and tests, they remain only in comments of immutable deployed SQL migrations (`crates/bot-store/migrations/0020_pass_and_threads.sql` through `crates/bot-store/migrations/0022_slack_events.sql`):

| Tag | Meaning | Primary Locations |
|---|---|---|
| F1 | Pre-approval grants issued at activation for static triggers | `crates/bot-core/src/grant.rs`, `crates/bot-store/src/grants.rs` |
| F2 | Slack channel event selection (`events`: mention, thread reply, reaction) and bot-thread participation proofs | `crates/bot-core/src/slack.rs`, `crates/bot-gateway/src/slack/events.rs` |
| F3 | Pass (`bot/pass`) chain and cursor handing unhandled direct-tool events to the next candidate | `crates/bot-core/src/pass.rs`, `crates/bot-store/src/pass.rs` |
| F4 | Designated-approver evidence allowing a specific user's message or reaction to satisfy an approval step | `crates/bot-store/src/evidence.rs` |

## Other Commonly Confused Terms

| Term | Code | Meaning |
|---|---|---|
| effect | `EffectId`, `EffectState` | Single external write step performed by a tool. An effect with uncertain outcome pauses as `Held` until an operator resolves the hold (`HoldId`). |
| active set | `ActiveRevisions`, `Revision` | Currently published profile set and its revision. `ActiveRevisions` reads and atomically swaps (CAS) the published `Revision`, which hashes profile revisions together with security-relevant host settings. Changes require `bot serve --activate`; processes running older revisions become stale ([Gateway](gateway.md)). |

# Admin UI and Observability

English | [한국어](ko/admin.md)

`bot serve --role admin` exposes the axum Admin API and embedded UI. `bot-admin` never accesses PostgreSQL directly: reads and mutations go strictly through `bot_core::admin` ports (`AdminRead`, `AdminAct`) and `Broker::decide`, with audit entries committed atomically alongside state changes. The UI and API share one authentication pipeline.

## Access Scope

Authentication uses a single secret—HTTP Basic in browsers (username ignored) or `Authorization: Bearer` in tools—bound to `Viewer{principal, operator}`. If no credentials are configured, no routes are mounted (closed by default). Non-loopback bind addresses require explicit `remote: true`.

- **Operator** (one per host): views structure, runs, holds, pre-approvals, audit logs, and status across all profiles; resolves holds; resumes/cancels runs; pauses/resumes pre-approvals.
- **Other principals**: view only their own runs and approvals they requested or are designated to approve. Other principals' runs return `404` (existence hidden).

## Authentication Pipeline

Every request passes host, rate-limit, and credential checks; `POST` mutations also pass CSRF validation:

1. **`Host` check**: rejects `Host` headers outside the allowlist (bind address, loopback names, configured `hosts`) with `421` to block DNS rebinding.
2. **Per-IP failure limit**: exceeding `max_failures` (default `5`) failed attempts in 60s blocks that peer IP for 60s (`429`) without affecting other clients. Uses only the socket peer address—never reads `X-Forwarded-For` or `Forwarded` (locked by tests; behind a proxy, the proxy IP shares one bucket). Requests without credentials (initial browser prompt) are not counted; only invalid or malformed credentials count.
3. **Secret match**: hashes the secret and compares in constant time across all entries (`401` on failure).
4. **CSRF protection (`POST`)**: requires viewer-bound `X-Bot-Csrf` (issued by authenticated `GET /v1/admin/me`), `Content-Type: application/json`, and same-origin headers (`Origin` / `Sec-Fetch-Site`), returning `403` on failure.

Error bodies contain a single fixed vocabulary word (`invalid`, `denied`, `stale`, `changed`, `csrf`, `not_found`, `conflict`, `unavailable`, `no_body`, etc.) and never echo input. Responses are capped at 512 KiB. When `leak_values` / `leak_redactor` is configured, decoded JSON keys and string values are scanned before emission so responses containing known secrets—even with quotes, backslashes, or newlines—are blocked.

## Admin API

| Route | Access | Behavior |
|---|---|---|
| `GET /v1/admin/me` | All | Current `principal`, `operator` flag, and `csrf` token. |
| `GET /v1/admin/runs` | All (scoped) | Paginated run list (`profile`, `state`, `root`, `limit`, `after`). |
| `GET /v1/admin/runs/{profile}/{run}` | All (scoped) | Lineage (root to current), children, tool calls (error codes: `denied`, `timeout`, `invalid_result`, `outcome_unknown`, `execution_failed`; no raw payloads), effects, per-attempt trace ID and link. |
| `POST /v1/admin/runs/{profile}/{run}/resume` | Operator | Resumes a `dead` standard root run under the same run ID. |
| `POST /v1/admin/runs/{profile}/{run}/cancel` | Operator | Requests cancellation of a non-terminal standard root run and its child tree. |
| `GET /v1/admin/approvals` | All (scoped) | Approvals (`?state=pending` default, `decided`, `all`), broker-rendered target/changes, and `args_sha256`. |
| `POST /v1/admin/approvals/{nonce}/decide` | Designated approver | Approves or denies a pending card. |
| `GET /v1/admin/holds` | Operator | Held calls and steps (with preserved receipts). |
| `POST /v1/admin/holds/{profile}/{run}/{call}/resolve` | Operator | Resolves a hold (`hold_id` required in body). |
| `GET /v1/admin/preapprovals` | Operator | Active pre-approvals `{"items":[{profile, agent, trigger, grant_id, epoch, paused, used_today, max_per_day}]}` (no query params; UI filters by profile). |
| `POST /v1/admin/preapprovals/{profile}/{agent}/{trigger}/pause` | Operator | Emergency pause for a pre-approved trigger; exact body `{"grant_id","epoch"}`. |
| `POST /v1/admin/preapprovals/{profile}/{agent}/{trigger}/resume` | Operator | Resumes a paused trigger; exact body `{"grant_id","epoch"}`. |
| `GET /v1/admin/audit` | Operator | Hash-chained audit log (newest first; reads one extra row so `next` is set only when another page exists; store max 201 rows). |
| `GET /v1/admin/audit/verify` | Operator | Recomputes the full audit hash chain from genesis. |
| `GET /v1/admin/status` | Operator | Per-profile run counts, queue age, pending approvals, holds, and trace/log/issue exporter status. |
| `GET /v1/admin/map` | Operator | Structure map and access summary. |
| `GET /ui` | All | Embedded UI (same-origin scripts/styles only; no inline code). |

### Run Resume and Cancel

Operator run-detail responses for standard root runs include `control` alongside `run.state` and `run.generation`: `claim_epoch` (increments on re-claim even within the same generation), `held` (unresolved hold in subtree), and `cancel_requested`. When `cancel_requested` is `true`, active-run cancellation is blocked, while a `dead` run may still be resumed (clearing `cancel_requested` on commit).

Both endpoints enforce auth, CSRF, same-origin, and the active-set fence. Send observed fields plus a fresh `command_id` (e.g., `{"command_id":"<id>","state":"dead","generation":2,"claim_epoch":4}`). Success returns `200 {"ok":true}`. Retrying the identical body after a lost response still returns `200 {"ok":true}` even after later generations. Each committed action writes one audit row with operator, target, command digest, and fixed reason `operator_requested`.

- `400 invalid`: malformed body or identifier.
- `403 denied` / `403 csrf`: non-operator or CSRF/origin failure.
- `404 not_found`: missing run or private run.
- `409 stale`: observed `state`, `generation`, or `claim_epoch` changed.
- `409 conflict`: `command_id` reused with a different payload, or invalid state transition.
- `409 held`: unresolved hold in subtree (resolve under **Approvals & Holds** first).
- `503 unavailable`: store or active-set check failed.

### Pre-Approval Management

`GET /v1/admin/preapprovals` returns one row per active pre-approval (`epoch`, `used_today`, `max_per_day` JSON integers; `paused` boolean; `used_today` is current UTC day). `pause` and `resume` enforce auth, operator scope, JSON, `X-Bot-Csrf`, same-origin, active-set fence, and `{grant_id, epoch}` match, returning `200` with the unwrapped updated row.

- Re-sending the same command when already in that state on the same `(grant_id, epoch)`—or retrying after a lost response with no intervening transition—returns `200` with zero new audit rows or epoch increments.
- Replaying an earlier pause after `pause → resume → pause` or after grant replacement returns `409 stale`.
- Agent IDs containing `/` (e.g., `main/developer`) must be single-segment percent-encoded (`main%2Fdeveloper`); the server decodes once after routing.
- Errors: `400 invalid`, `403 denied`, `404 not_found`, `409 stale`, `503 unavailable`.

After a pause commits, no new occurrences, tool starts, or write grants execute. Occurrences scheduled before resume are skipped rather than replayed (each skipped occurrence records an audit entry; execution resumes from the next occurrence after resume). Result recording and hold recovery for already-authorized writes continue normally.

## Active-Set Fence

When `AdminConfig::fence(hook)` is configured (`bot-cli` compares this process's active set against the store's published revision), every `/v1/admin/*` response runs the hook **immediately before returning**. If the set drifted or the check exceeds `fence_timeout` (default 3s), the handler response is discarded and replaced with `503 {"error":"unavailable"}` so stale processes never serve outdated maps, cards, or runs.

- Mutating endpoints also run the fence **before** the handler so stale processes never reach the broker or store. If the post-handler fence fails, the commit may already have succeeded; retrying the identical decision returns `repeated: true` without double-applying (store commits enforce their own fence).
- Authentication (`Host`, failure limit, secret, CSRF) runs first—rejected requests return normal `401`/`403`/`421`/`429` without invoking the hook. Static UI assets (`/ui`) skip the fence. Without a hook, the layer is a no-op. Reads invoke the hook once; mutations twice.

## Approvals

Approval and run listings apply filters before cursor pagination and paginate by the sort key (never row offsets), preventing duplicates or skips when rows change between pages.

| `state` | Scope | Sort Order |
|---|---|---|
| `pending` (default) | Awaiting decision | Soonest expiry ascending, then `nonce` ascending. |
| `decided` | All non-pending (including expired/superseded) | Decided timestamp descending (nulls last), then `nonce` ascending. |
| `all` | All approvals | `decided` rows first in `decided` order, then `pending` rows. |

To decide via API, send `POST /v1/admin/approvals/{nonce}/decide` with `{"allow":true,"seen":"<args_sha256>"}` (`allow: false` to deny), `Content-Type: application/json`, and `X-Bot-Csrf` from `GET /v1/admin/me`:

- Expired or superseded cards return `409 stale`; `seen` digest mismatches return `409 changed`.
- Only the designated approver may decide (`403` otherwise—operators cannot approve on another user's behalf; models and service principals are rejected by the broker).
- Repeating the same verdict with the same `seen` by the same approver returns `200` with `repeated: true` without re-invoking the broker; flipping the verdict returns `409`.
- Recording a decision wakes the worker; the broker re-verifies current permissions and the store atomically consumes the approval and starts the call (or enqueues outbox rows for scheduled delivery).

## Owner-Only Approval Cards

Default `bot serve` signs into Admin UI as the local operator. In v2.0, Slack owners approve channel-origin schedules via their Slack 1:1 DM; Admin UI does not support Slack login ([Reliability](reliability.md)).

Memory share cards (`memory.share`) contain private memory text, and scheduled delivery cards (`schedule.deliver`) contain full outbound drafts. Their body is visible **only when viewer == requester == designated approver** (including operators). All other viewers receive metadata with `render: []` and `render_hidden: true`. Run details, audit, status, and map never include card bodies.

- **Memory share (`memory.share`)**: requires exact single `Memory ID` and `Body` rows (continuation lines joined; raw text or JSON string; other rows like title/scope are free). Approval (`allow: true`) is permitted only when the body is visible and `memory_share_hash(memory_id, sha256(body))` matches `args_sha256`. Missing, hidden, truncated, or mismatched content sets `approve_blocked: "no_body"`, disables **Approve** in the UI, and returns `403 no_body` on `allow: true` before reaching the broker or audit. **Denial (`allow: false`) is always allowed** (non-designated viewers are rejected first as unauthorized).
- **Scheduled delivery (`schedule.deliver`, `bot_core::schedule::DELIVERY_CARD_LABEL`, risk `irreversible`)**: requires exactly one `Delivery draft` row whose joined value is the exact canonical JSON from `DeliveryDraft::approval_json()` (raw document, never JSON-string-wrapped) and whose SHA-256 equals `args_sha256` (`DeliveryDraft::sha256()`). Any truncation, edit, or mismatch yields `approve_blocked: "no_body"` and `403 no_body` on `allow: true` (denial remains allowed). The UI displays this row verbatim without modifying its text.

## Holds and Audit

Holds occur when an external write's outcome cannot be confirmed. Multi-step tools preserve receipts for confirmed steps and hold only unresolved steps. During rolling `0025` upgrades, older admin processes roll back whole-call `not_executed` on calls with confirmed or unresolved steps; resolve per-step and final call receipts on the updated Admin UI.

- **Verdicts**: `confirmed` (destination `receipt` required) or `not_executed` (`evidence` required)—each a single line of 1–512 non-control characters.
- **Multi-step calls and final outcome**: resolve each open step individually. If every step resolves `not_executed`, the call resumes through current permissions and approval. If any step is `confirmed` without a final call response, resolving the last open step opens a **Final call outcome** card (individual step receipts remain visible). After confirming the old tool and child processes have exited, enter the destination-verified overall call receipt; the broker returns it on next claim. Sending `verdict: "not_executed"` for a call with confirmed steps returns `400 invalid`. Cancelling a run closes its open tool holds atomically (`run.cancelled` audit + closed timestamp); operator verdicts emit `hold.resolve`. Steps marked not executed display `Marked not executed` (`resolution: "not_executed"`, unselectable) until restarted (`null`).
- **Post-`not_executed` behavior by `HoldView.kind`** (set by the store from the backing row; API never inspects tool names or call IDs):
  - `tool`: requires verifying **both** prior process exit and non-execution at the destination. Waits if open steps remain, opens **Final call outcome** if confirmed steps exist, or resumes under the same call ID.
  - `reply`: resends the reply from scratch. Exception: legacy/unconfigured `missing_transport` replies reject `not_executed` with `409 conflict` (UI hides the resend form). Either populate `bot_outbox.transport` in DB before marking `not_executed`, or deliver `bot_outbox.body` manually and close as `confirmed`.
  - `private`: message body is never stored, so retry is impossible—closes as not executed and requires a new user request (`confirmed` closes as completed).
- **`hold_id` pinning**: the store mints a new `hold_id` on every hold entry (`HoldView.hold_id` for whole-call/final/reply/private; `EffectView.hold_id` for open steps, while call-level `hold_id` is empty). `hold_id` is mandatory in the resolve payload (`400` if absent/malformed) and matched atomically with `(profile, run, call, effect)`: unknown → `404`, wrong target → `409`, identical replay on a closed `hold_id` by the same operator with the same verdict and receipt/evidence → `200 repeated: true` (no extra audit or span, even if a later hold opened), differing operator/values → `409`. Targets missing a `hold_id` disable resolution buttons (and Enter submission) and prompt **Reload**.
- **Kind and profile isolation**: the UI keys cards and forms by `(profile, run, call, kind)` (`crates/bot-admin/ui/holdpin.js` `keyOf`), keeping legacy dual `tool` + `reply` holds on the same call ID separate. The store keys all holds, steps, approvals, and runs by `(profile, run, call[, effect])` (`crates/bot-admin/tests/suite/hold_pairs.rs`, `crates/bot-admin/tests/suite/hold_profiles.rs`, `crates/bot-admin/tests/browser/hold_pair.mjs`). Test doubles store verdict + digest only, matching production.
- **Two-step UI pinning**: irreversible hold verdicts require two presses (`crates/bot-admin/ui/holdpin.js`). The first press pins target step, `hold_id`, verdict, and input; the second sends strictly the pinned values. Concurrent list updates, `404`/`409` responses, or step selector changes clear both confirmation and input (`crates/bot-admin/tests/js/holdpin.test.mjs`, `crates/bot-admin/tests/browser/hold_pin.mjs`, `crates/bot-admin/tests/browser/hold_stale_click.mjs`).
- **Audit chain**: links sequence number, previous hash, principal, action, target, args hash, and verdict. `GET /v1/admin/audit/verify` recomputes from genesis and identifies the first broken sequence (does not protect against full-DB rewrites).

## Embedded UI

`/ui` serves buildless static HTML/CSS/JS embedded at compile time from `crates/bot-admin/ui/` via `build.rs` (manifest locked by `crates/bot-admin/tests/suite/assets.rs`). Strict same-origin CSP—tests forbid external URLs, HTML injection, and inline scripts/styles.

| Tab | Audience | Contents |
|---|---|---|
| Structure | Operator | Interactive card map (pan, zoom, search, kind filter) with dependency and shared-resource (`needs`) edges. |
| Runs | All | Scoped run list and detail pane (lineage, children, tool calls, effects, trace links), plus operator resume/cancel. |
| Approvals & Holds | All (holds: operator) | Approval cards (designated approver only, verified by `seen` hash) and two-step hold resolution forms. |
| Pre-approvals | Operator | Status, UTC daily usage meter, and emergency pause/resume controls ([Pre-Approvals UI](#pre-approvals-ui)). |
| Access | Operator | Roles, members, policy caps, tool risk/approval/scopes/secret names, and external-egress tools. |
| Audit | Operator | Hash-chained log and **Verify chain** action. |
| Status | Operator | Per-profile run counts, queue wait age, pending approvals, holds, and observability exporter health. |

Respects light/dark mode and `prefers-reduced-motion`; 5s auto-refresh pauses during typing or armed confirmations. Operator tabs are hidden from non-operators (and blocked server-side with `403`). Vocabulary labels live in `crates/bot-admin/ui/labels.js` (unknown codes display as `Other`).

In **Runs** detail, **Resume** on a `dead` root run requires two presses within 12s (600ms guard) pinning observed state; **Cancel run** uses one confirmation prompt. Subtree holds show **View in Approvals & Holds**. Lost responses pause auto-refresh and offer **Resend same command** or **Reload latest state**.

### Structure Map

Built from the active `ProfileCatalog` with 8 card kinds (profile, agent, skill, tool, trigger, binding, scope, secret) and 6 edge kinds (delegates, uses, owns, handles, wakes, needs) under a deterministic revision (`rev`). Exposes names, classifications, and counts only—never instructions, knowledge bodies, env vars, or secret values. Inherited skills appear once on the defining agent. Agent summaries show the first line of description (≤ 140 chars, fallback to model) and non-inherited `builtin` read-only tools (schema allows `read`/`grep`/`glob`; `claude-cli` VM supports `read` only).

### Pre-Approvals UI

The operator-only **Pre-approvals** tab lists each active `preapproved` static trigger with status (`Active`/`Paused`), UTC daily write usage (`used_today / max_per_day`), grant ID prefix (12 chars), pause `epoch`, and **Emergency pause** / **Resume** buttons (profile selector filters client-side).

- **One-click pause, two-click resume**: **Emergency pause** fires immediately. **Resume** pins target, `grant_id`, and `epoch` on first press, requires confirmation within 12s (ignoring presses within 600ms), sends strictly pinned values, and disarms on list drift, Esc, Cancel, tab switch, or timeout.
- **Server-authoritative state**: never mutates optimistically; allows one in-flight request per row (15s timeout); accepts only responses matching target, `grant_id`, and expected `paused` state; always reloads the list afterward.
- **Malformed/duplicate rows**: rows failing required-field checks (4 strings, 3 integers, 1 boolean) or duplicating `(profile, agent, trigger)` are skipped without buttons and counted in a warning; malformed list responses preserve prior rows with a warning.
- **Boundary and tests**: server enforces operator role, CSRF, same-origin, active-set fence, and `{grant_id, epoch}` match. Tested by `crates/bot-admin/tests/js/preapproval.test.mjs`, `crates/bot-admin/tests/js/preapprovals.view.test.mjs` (`crates/bot-admin/tests/js/support/contract.mjs`), and manual browser scripts `crates/bot-admin/tests/browser/preapproval_stub.mjs` and `crates/bot-admin/tests/browser/preapproval_stop.mjs`.

## Observability

### OTLP Tracing (`admin.trace`)

Worker, broker, and gateway submit completed spans to `bot_core::trace::Tracer` synchronously, without blocking or failing.

- **Deterministic Trace IDs**: derived from `(profile, run, generation)` so split roles correlate without IPC propagation; new generations or different profiles produce distinct traces.
- **Fixed vocabulary**: exports only `gen_ai.*`, `bot.*`, `user.id`, and `session.id` (never nonces, raw args, receipts, or private DM text). `worker.model` (`Client`) records `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, and token counts; `worker.run` records `user.id`, `session.id`, and `gen_ai.conversation.id` (pseudonymized by default). HTTP intake spans record matched **registered trigger** `http.request.method` / `http.route` (unmatched → `_OTHER` / `unmatched`), never caller-supplied paths.
- **Redaction and content policy**: unknown keys drop at export. Content attributes are per-profile opt-in (`content: false` default), routed only to that profile's collector, redacted for known secrets/token patterns, and capped at 8 KiB. The live `Redactor` records every secret read from `SecretStore` before use and refreshes all declared secret names (including host observability secrets) every `BOT_SECRETS_REREAD_SECS` (default 60s, `0` disables; 5s per-read timeout, paced with up to 10x backoff on unresponsive stores). Rotated secrets—and secrets used only by other roles once read—are redacted across traces, logs, issues, and Admin API responses; prior values stay redacted for process lifetime, and batch bodies serialize *after* reading exporter auth headers. `bot.private=true` suppresses content even when `content: true` (the flag is not exported). Identifiers default to per-profile hashes (`ids: "hash"`; set `ids: "plain"` to opt out).
- **Revocation and activation fence**: `admin.trace`, `admin.logs`, and `admin.issues` enter the activation fingerprint (disabled sections are omitted so unused hosts keep unchanged fingerprints; display `name` and `link` are excluded). Changing or revoking observability requires `--activate`. Stale processes drop queued items (`dropped`, `last_error: stale`); store outages/3s timeouts fail closed. The fence is checked twice per batch—before reading exporter credentials and right before HTTP send (in-flight requests already transmitted cannot be recalled).
- **Isolation, resilience, and sampling**: each profile has its own bounded queue, worker, circuit breaker (1s–60s exponential backoff), and destination. Spans > 256 KiB or full queues drop cleanly without affecting runs. Header secrets are read per send from that profile's `SecretStore` (never sent unauthenticated on error). Dropping all `Exporter` handles flushes queued spans and releases store refs. Deterministic sampling on trace ID ensures split roles agree.
- **Transport**: OTLP/HTTP protobuf (`application/x-protobuf`).
  - `https://` (Langfuse, LangSmith, remote OTLP): uses OS root certificates without `remote: true`, disables redirects/retries/connection reuse, marks custom headers sensitive, and pins `localhost`/`*.localhost` to loopback without DNS.
  - Plaintext `http://`: allowed without `remote: true` only for loopback (`127.0.0.0/8`, `::1`), private (`10/8`, `172.16/12`, `192.168/16`, `fc00::/7`), link-local (`169.254/16`, `fe80::/10`), and `localhost`/`*.localhost` (pinned to loopback with original `Host`). Any other plaintext host or DNS name requires explicit `remote: true`.

### OTLP Logs (`admin.logs`) and Issue Tracking (`admin.issues`)

Both share the same transport, circuit breaker, activation fence, and `Redactor`:

- **Structured stderr and OTLP logs (`admin.logs`)**: `BOT_LOG_FORMAT=json` writes line-delimited `{ts_ms, level, target, message, fields}` to stderr (default `text`), filtered by `BOT_LOG` (workspace crates only; external crates capped at `warn`; `sqlx` off). Enabling `admin.logs` batches the same events over OTLP/HTTP(S) protobuf (`ExportLogsServiceRequest`, default path `/v1/logs`) at minimum `level` (`debug`, `info`, `warn` default, `error`, bounded by `BOT_LOG`). Bodies and fields are redacted and capped (2 KiB body, 256-char values, 32 fields).
- **Issue tracker (`admin.issues`, `kind: "sentry"`)**: implements `bot_core::issue::IssueReporter` via Sentry Envelope v7 (`POST <endpoint>`, `Content-Type: application/x-sentry-envelope`, `X-Sentry-Auth`) for Sentry, GlitchTip, Bugsink, and compatible backends. Auto-reports: (1) `tracing::error!`, (2) panics (`fatal`), and (3) abnormal failed spans (excluding expected rejections/cancellations: `cancelled`, `denied`, `invalid`, `rate_limited`, `forbidden`, `unregistered`, `no_route`, `too_large`, `parked`, `lost`). Links failed spans via `contexts.trace.trace_id` and omits extra tags on `bot.private` spans. Loop/flood guards: exporter failures update counters/`last_error` without logging, observability/HTTP transport targets are skipped, and `(kind, target)` deduplicates with a 5s cooldown.
- **Host-scoped secret refs**: `admin.logs.headers` secret objects and `admin.issues.key` accept `{"secret": "<name>"}` (resolves under `bot/host/<name>`) or `{"profile": "<profile>", "secret": "<name>"}` (checked by `bot validate`/`bot doctor` and preloaded into the redactor by `bot serve`). `admin.issues.key` may be omitted only for loopback plaintext `http://` mocks.
- **Status**: `GET /v1/admin/status` exposes `process.logs` and `process.issues` with `{enabled, queued, sent, dropped, failed, circuit, last_error, backends}` (shows `origin` only—never paths, headers, or keys).

## Host Configuration (`admin` Section)

`BOT_HOST_CONFIG` passes its `admin` object to `AdminSettings::from_json`. Unknown keys and invalid values fail before startup; `{}` uses local defaults; never include secret values.

| Key | Default | Contract |
|---|---|---|
| `bind` | `127.0.0.1:8787` | Admin listener `host:port`. Non-loopback requires `remote: true`. |
| `remote` | `false` | Explicit operator opt-in for non-loopback `bind` (and under `trace`/`logs`/`issues`, opt-in for non-private plaintext `http://` export; unnecessary for `https://`). |
| `hosts` | `[]` | Additional allowed `Host` values (`host:port`) behind a reverse proxy. |
| `max_failures` | `5` | Per-peer-IP auth failures allowed in 60s before a 60s block (range `1..=100`). |
| `trace.<profile>` | off | Per-profile OTLP trace exporter (`null` disables): required `endpoint`, optional `name` (default `default`), `link` (`http(s)` template with `{trace_id}`), `headers` (string or `{"secret": "<name>"}` from that profile's `SecretStore`), `content` (`false`), `ids` (`hash` or `plain`), `sample` (`0.0..=1.0`, default `1.0`), `remote` (`false`). |
| `logs` | off | Host-wide OTLP log exporter: `endpoint`, `name`, `headers` (string or `{"secret": "<name>"}` / `{"profile": "<p>", "secret": "<name>"}`), `level` (`warn`), `remote` (`false`). |
| `issues` | off | Host-wide issue tracker: `kind` (`sentry`), `name`, `endpoint`, `key` (`{"secret": "<name>"}` or with `profile`; optional on loopback `http://` only), `environment`, `release`, `sample` (`1.0`), `remote` (`false`). |

Example local collector config (`admin` object in `host.json`):

```json
{
  "bind": "127.0.0.1:8787",
  "remote": false,
  "hosts": [],
  "max_failures": 5,
  "trace": {
    "personal": {
      "endpoint": "http://127.0.0.1:4318/v1/traces",
      "name": "local-collector",
      "headers": {"Authorization": {"secret": "collector-token"}, "X-Env": "dev"},
      "link": "https://traces.example/t/{trace_id}",
      "content": false,
      "ids": "hash",
      "sample": 1.0
    }
  }
}
```

Example combining Langfuse, LangSmith, OTLP logs, and Sentry:

```json
{
  "bind": "127.0.0.1:8787",
  "trace": {
    "personal": {
      "name": "langfuse",
      "endpoint": "https://cloud.langfuse.com/api/public/otel/v1/traces",
      "headers": {
        "Authorization": {"secret": "langfuse-auth"},
        "x-langfuse-ingestion-version": "4"
      },
      "link": "https://cloud.langfuse.com/trace/{trace_id}",
      "ids": "hash",
      "content": false,
      "sample": 1.0
    },
    "work": {
      "name": "langsmith",
      "endpoint": "https://api.smith.langchain.com/otel/v1/traces",
      "headers": {
        "x-api-key": {"secret": "langsmith-key"},
        "Langsmith-Project": "bot-work"
      }
    }
  },
  "logs": {
    "name": "otel-logs",
    "endpoint": "https://otlp.example.com/v1/logs",
    "headers": {"Authorization": {"secret": "otel-logs-token"}},
    "level": "warn"
  },
  "issues": {
    "kind": "sentry",
    "name": "sentry",
    "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/",
    "key": {"secret": "sentry-key"},
    "environment": "production",
    "release": "bot@0.1.0",
    "sample": 1.0
  }
}
```

## Startup Sequence

1. Start `bot serve --role admin`. The operator identity is resolved from the CLI account mapped to the current OS user (alphabetically first if multiple exist).
2. Startup prints `Admin UI: http://<addr>/ui` and displays the one-time operator secret once on the terminal. Open `/ui` and enter any username with that secret in the HTTP Basic prompt. For headless starts, set `BOT_ADMIN_TOKEN_FILE` to an absolute path inside a directory not writable by other users; the secret is written with mode `0600` and removed on clean shutdown.
3. Verify identity and role at `GET /v1/admin/me`. Approval buttons act only when the signed-in principal is the designated approver.

Token-file publication acquires a per-path lock, writes a recovery record, and then publishes the secret. If active-set publication fails, the file rolls back to its prior state. After an unclean exit, the next startup reconciles the recovery record against the file identity; if an operator replaced the file and prior state cannot be verified, startup preserves the file and aborts.

To embed the Admin UI in another host binary, use `bot_admin::bind`, `router`, and `serve` (`serve` attaches peer `ConnectInfo` required for per-IP login rate limiting).

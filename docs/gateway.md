# Gateway Operations

English | [한국어](ko/gateway.md)

The gateway authenticates CLI, Slack, and HTTP requests and validates registered entrances, members, and triggers. Principal and reply conversation come strictly from authenticated identity—request bodies cannot override them. A request ID is returned only after the PostgreSQL intake transaction commits; redeliveries with the same ID converge on the same run ([Reliability](reliability.md)).

Push entrances pin the reply delivery adapter (`transport`) from the authenticated binding (Slack channels and DMs share `slack`). Outbox workers claim only replies matching their adapter—and their assigned profile/binding scope when split—and re-verify current binding and destination permissions even after claiming. CLI and HTTP receive answers by polling rather than through the outbox ([Reliability](reliability.md)).

## Host Registry (`BOT_HOST_CONFIG`, JSON)

`host.json` maps profile aliases to OS accounts, services, and executables. Omitting `BOT_HOST_CONFIG` uses built-in demo defaults for example profiles. Custom profiles must register their models and entrances ([Host configuration for a new profile](profile-spec.md#host-configuration-for-a-new-profile)).

Top-level keys: `version` (`1`), `gateway`, `servers`, `models`, `providers`, `egress`, `private_egress`, `super_admins`, `sandbox`, `limits`, `embeddings`, `memory_judge`, `admin`.

- `models` / `providers`: per-profile allowed model aliases bound to `demo`, `claude-cli`, `codex-cli`, `agy-cli`, `anthropic-api`, or `openai-api`. Upstream endpoints require `https://` URLs with no credentials, query, or fragment (direct API providers also accept `http://` on loopback IP literals, the only case where `secret` may be omitted).
- `egress` / `private_egress`: per-profile allowed external `host:port` caps and pinned private/local destination mappings.
- `super_admins`: host-level upper bound on super-admin principals per profile (included in the activation fingerprint when non-empty).
- `servers`: registers local MCP packages (`root` and command) or remote HTTP servers (`transport: "http"`, `url`). `bot validate` computes server revisions from server files and commands. Tool runners use read-only installs under `BOT_INSTALLS`. Default `state` is `ephemeral`; `required` servers cannot start on the current `srt` host ([Plugins](plugins.md)).
- `sandbox` / `limits` / `embeddings` / `memory_judge` / `admin`: model VM and sandbox settings (`instance`, `image`/`images`, `network` — see [Architecture](architecture.md)), tool limits, optional hybrid memory `/embeddings` and System One `memory_judge` endpoints, and Admin UI/observability settings (`bind`, `hosts`, `max_failures`, `trace`, `logs`, `issues` — see [Admin UI](admin.md)).

The active set binds profile revisions together with authority-affecting host settings (CLI user mappings, Slack teams/channels, HTTP auth, allowed models/upstreams/secret names, server revisions, `egress`, `private_egress`, `super_admins`, VM `image`/`images`/network, non-default `limits.tool_call_max_secs`, configured `embeddings`/`memory_judge`, and enabled `admin.trace`/`logs`/`issues`). Secret values are never included. Apply host mapping changes with `bot serve --activate`.

### `gateway` Keys

| Key | Contract |
|---|---|
| `cli.accounts.<alias>` | Exactly one of `{"os_user": "name"}` or `{"uid": 501}`. `cli.socket` must be an absolute path ≤ 100 bytes. |
| `slack.workspaces.<alias>` | `team_id`, optional `app_id` (checked on Socket Mode hello), `channels` (alias → channel ID; `D…` DM IDs forbidden here), `dm` (default `false`), `receive`, and secret **names** `bot_token`, `app_token`, `signing_secret`. |
| `http.listen`, `http.remote` | Default `127.0.0.1:8788` (distinct from Admin UI's `127.0.0.1:8787`). Plaintext HTTP: non-loopback binds require explicit `http.remote: true` (default `false`) behind a TLS proxy, else `bot validate` and `bot serve` reject `gateway.http.listen` and bind nothing. |
| `http.endpoints.<alias>` (webhook) | `kind: "webhook"` (or omitted): `issuer`, `audience`, `key` (HS256 secret name), `sync_wait_ms` (default 10s, 1–60000 ms), `max_token_age_secs` (≥ 1). |
| `http.endpoints.<alias>` (OpenAI) | `kind: "openai"`: `keys` (1–16 slots mapping slot → `{secret, subject}`), `wait_ms` (default 120s, 1–300000 ms), `max_active_runs` (default 32, 1–256). Must use a separate alias from webhooks (`v1` is reserved when any OpenAI endpoint is configured). |
| `limits` | `body_bytes` (default 256 KiB OpenAI, 64 KiB other entrances; max 1 MiB), `attachments` (default `8`), `per_entrance_per_min` (default `120`), `per_principal_per_min` (default `30`). |
| `admin_url` | Optional Admin UI URL linked on standard approval cards (unused for Slack owner scheduled-approval DMs). |

Slack workspace receive rules:

- `receive` accepts `socket` (default) only; `http` is rejected on `slack.workspaces.<alias>.receive` by `bot validate` and `bot serve` even if `signing_secret` is set.
- `app_token` is required for `socket`.
- `signing_secret` is format-checked only and unused at runtime, but enters the active set (changing it requires `--activate`).

Never put secret values in `host.json`. Names resolve exclusively under `bot/<profile>/<name>` for the binding-owning profile (no global fallback; identical token values never merge permissions).

## Principals and Entrances

- **Principal format**: `local:<account>`, `slack:<workspace-alias>/<Slack-user-ID>`, `http:<endpoint-alias>/<sub-or-subject>`. Embedded separators cannot spoof another principal.
- **Entrance uniqueness**: no entrance may be bound twice across active profiles (at either alias or resolved ID level). CLI selects an entrance by `(OS user, profile)`.
- **Membership and scopes**: non-members are rejected (Slack silently acks, HTTP `403`, CLI `forbidden`). Requester scopes equal `(union of role scopes) ∩ policy ceiling`; starting an agent run requires `run`. Approvers default to human principals (CLI/Slack); HTTP service principals are never approvers.
- **Triggers**: candidates for the same binding and event (or HTTP method/path) evaluate in `order`; first match wins. Envelope matchers (`body.…`, `conversation.…`) support dot-path lookup and literal equality only.
- **Private triggers**: route exclusively through `PrivateIntake` before standard intake/queueing and never fall back to normal chat on failure. A selected `private_pass: true` trigger returning a valid empty PASS submits the raw text and attachment references to the binding's standard DM intake. Only permanent rejections (format/permission) are acked; in-progress, pre-start retry, conflict, and outage signals stay unacked for redelivery.

## CLI Entrance

- **Mutual peer verification**: uses a Unix domain socket (`0700` dir, `0600` socket) and authenticates the client via **peer UID**. Before sending text, `bot chat` verifies the socket directory is owned by the current user with `0700` permissions (not a symlink), the socket file is owned by the current user, and the peer process UID equals its own UID. Any mismatch fails with exit code `1` (not retry code `3`), blocking socket spoofing in shared directories.
- **Socket path precedence**: `BOT_SOCKET` → `gateway.cli.socket` → `<XDG_RUNTIME_DIR or TMPDIR>/bot-<uid>/gateway.sock` → `$HOME/.local/state/bot/run/gateway.sock` (never shared `/tmp`). If `$HOME` is not an absolute path, startup/client fails naming `BOT_SOCKET`.
- **Wire protocol**: one JSON request line per connection (`{"v":1,"op":"chat","request":ID,"text":…,"profile"?,"thread"?,"wait"?}` or `op:"result"` to re-query/attach) and multiple JSON response lines (`accepted` → optional `state` → `result`, or `error` with fixed `code` and message). Reuse the same request ID across retries (`bot chat` generates one if omitted).
- **Disconnects and auth**: closing the socket does not cancel the run. Current membership is re-checked before emitting any status or result. CLI replies are fetched via polling, not the outbox.

## HTTP Entrance

This section covers JWT webhook endpoints (`kind: "webhook"` or omitted). SDK routes follow [OpenAI-Compatible API](#openai-compatible-api).

Because this entrance serves plaintext HTTP, it binds loopback by default; external traffic requires a TLS reverse proxy and `gateway.http.remote: true`.

- **Route**: `POST|PUT|PATCH|DELETE|GET /{endpoint-alias}{trigger-path}`.
- **Required headers**: `Authorization: Bearer <HS256 JWT>` (verifies `iss`, `aud`, `exp`, `nbf`; `alg: HS256` only; key ≥ 32 bytes) and `Idempotency-Key` (reused on retries).
- **Routing and auth order**: unregistered aliases return `404` before auth; registered endpoints authenticate before path lookup (`401` with no reason leaked). Auth and intake share one routing table; key cache is scoped by `(profile, binding, revision, secret name)`.

| Response | Condition |
|---|---|
| `202` + `Location` | Default (`accepted`) or `sync` timeout; body `{request_id, state}`. |
| Raw tool `{status, body}` | `sync` mode when tool returns `{status, body}` with `status` ∈ `{200, 201, 204, 400, 403, 404, 409, 422}`. |
| `400`, `401`, `403`, `404`, `405`, `409`, `413`, `415`, `429` (+`Retry-After`), `503` | Bad request, auth, permission, not found, method, same-key payload conflict, body limit, media type, rate limit, or store outage. |
| `502` | `sync` tool output does not match `{status, body}`. |

Status polling (`GET /{endpoint}/_requests/{id}`) returns results only to the originating principal and never starts new tool calls. Response mode (`sync` vs `accepted`) is stored at first intake and preserved across redeliveries and polls even if the trigger changes later (legacy rows without a recorded mode return `accepted`). Current membership and active revision are re-checked before returning any status or result.

## OpenAI-Compatible API

Configure `kind: "openai"` under `gateway.http.endpoints.<alias>` and bind public model names to agents in the profile ([OpenAI model bindings](profile-spec.md#openai-model-bindings)). Discover available model names via `models.list()`.

```python
import os
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8788/v1", api_key=os.environ["BOT_API_KEY"])
model = client.models.list().data[0].id
reply = client.chat.completions.create(
    model=model,
    messages=[{"role": "user", "content": "Show today's agenda"}],
    extra_headers={"Idempotency-Key": "agenda-request-1"},
)
print(reply.choices[0].message.content)
```

- **Idempotency**: reuse the original `messages`, `model`, and `Idempotency-Key` to poll an existing request; use a new key for a new task. If omitted, the server generates a random key and returns it in the `Idempotency-Key` response header (unrecoverable if the first response is lost—always supply a key for requests that may perform external writes). SDK retries sent without a key fail with `400 idempotency_key_required`. Distinct requests with identical bodies are never coalesced.
- **Auth and limits**: every route requires `Authorization: Bearer <API key>` (32–256 ASCII graphic bytes). Key slots map `secret` names to HTTP `subject`s; rotate keys by adding a new slot for the same `subject`, then revoking the old key. Because the gateway reads `SecretStore` per request, rotation takes effect immediately. Current membership applies to model lists, completions, and SSE. Loopback-only by default (`gateway.http.remote: true` + TLS proxy for remote access; no default CORS). Request body cap defaults to 256 KiB (overridable via `gateway.limits.body_bytes` up to 1 MiB; webhooks default to 64 KiB) with a max 10s read timeout (bounded by remaining request deadline).

| Method and Route | Response |
|---|---|
| `GET /v1/models` | Models the authenticated member may run on this entrance (`created` = Unix seconds when the current profile revision was first stored). |
| `GET /v1/models/{id}` | Single authorized model; missing and unauthorized models both return `404`. |
| `POST /v1/chat/completions` | Final answer in `choices[0].message.content` (`finish_reason: "length"` if truncated by output limit; `422 run_failed` if the runner hit the limit without saving a final answer). `stream: true` returns SSE. |

### Conversations and Approvals

- Each request is a standalone conversation: clients resend prior turns in `messages`. The last `user` turn is the prompt; earlier turns pass once in order as context. Client-supplied `system`, `developer`, and `assistant` turns are treated as untrusted user data—the server enforces profile instructions and permissions.
- Approval-gated tools run only after approval by the binding's designated approver (inherited by delegated children). The server verifies approver and delivery route match between intake and the current profile. Slack approvers decide in 1:1 DM; local approvers use the authenticated Admin UI; HTTP keys cannot approve. If the approver is missing, revoked, route changed, or DM delivery fails, the HTTP response reports that the action was not executed.

### Waiting, Polling, and Streaming

- `wait_ms` is a single post-auth deadline covering body read, intake, and waiting (default 120s, range `1..=300000` ms). Work budget is `wait_ms - min(2000, wait_ms / 4)` ms; the remainder is reserved for final permission verification and response transmission. Values of several seconds or more are recommended to avoid `422 request_outcome_unknown`.
- If the run is still active when the wait budget expires, the server re-checks permissions and returns HTTP `200` with `"Request is still in progress. Query again with the same Idempotency-Key and request."`. Once waiting on human approval, it immediately returns HTTP `200` with `"Awaiting approval. Query again with the same Idempotency-Key and request."`. Both keep the run alive.
- Re-POSTing with the same key and body returns current status or final answer; same key with a different body returns `400 idempotency_key_conflict`; unconfirmable store errors return `422 request_outcome_unknown`. Polling requires a key for the original principal and endpoint.
- `stream: true` returns `text/event-stream` (`Cache-Control: no-cache`, `X-Accel-Buffering: no`), emits `: keep-alive` comments at most every 15s while waiting, then streams the assistant role chunk, final answer text chunks, finish reason, and `data: [DONE]` (mid-generation token streaming is not included in this version). Post-open failures send an error object and close.
- `usage` (`prompt_tokens`, `completion_tokens`, `total_tokens`) is returned only when every model call in the run reported input and output counts; otherwise omitted. In SSE with `stream_options.include_usage: true`, regular chunks carry `usage: null`, and a final empty-choices chunk with `usage` precedes `[DONE]` when complete counts exist.

### Supported Request Parameters

| Parameter | Behavior |
|---|---|
| `model`, `messages` | Required. Roles `system`, `developer`, `user`, `assistant`; string or text-part array (joined with `\n`). Requires at least one `user` turn with non-whitespace content in the last `user` turn. |
| `stream`, `stream_options.include_usage`, `n` | Non-streaming by default; `include_usage` valid only with `stream: true`; single choice (`n: 1`). |
| `temperature`, `top_p`, `seed`, `user`, `metadata` | Validated for type/range only; ignored for execution, identity, and audit. |
| `store`, `response_format` | Omitted, `store: false`, and plain text format only (intake/recovery state is stored separately). |
| `max_tokens`, `max_completion_tokens`, `stop` | Per-request limits/stops are unsupported (`400`). |
| `tools`, `functions`, `tool_choice`, images/audio/files, `logprobs`, etc. | `400 unsupported_parameter` (only profile-authorized tools exist on the server). |

### Errors, Rate Limits, and Retries

Errors follow `{"error":{"message":"…","type":"…","param":null,"code":"…"}}`. `param` reports only server-known field paths (`null` at top level, known parent such as `messages[0]` when nested)—caller-invented names and values are never echoed.

| HTTP | `type` | `code` |
|---|---|---|
| `400` | `invalid_request_error` | `invalid_request`, `unsupported_parameter`, `idempotency_key_conflict`, `idempotency_key_required` |
| `401` / `403` | `authentication_error` / `permission_error` | `invalid_api_key` / `access_denied` |
| `404` / `405` | `invalid_request_error` | `model_not_found`, `not_found` / `method_not_allowed` |
| `413` / `415` | `invalid_request_error` | `request_too_large` / `unsupported_media_type` |
| `408` / `429` / `503` | `invalid_request_error` / `rate_limit_error` / `server_error` | Pre-intake `request_timeout` / `rate_limit_exceeded` (`Retry-After` secs) / `service_unavailable` |
| `422` | `server_error` | `request_outcome_unknown` (poll with same key + body), `run_failed`, `run_cancelled` |

- Hitting `max_active_runs` returns `429` with `Retry-After: 1`; rate limits return ceiling seconds remaining. Polling an already-accepted key consumes no active-run slot.
- **Auth failure limit**: tracked per gateway process by direct peer IP (`X-Forwarded-For` is ignored). Reaching 20 failed `Authorization` attempts in 60s blocks all model/chat requests from that IP for 60s (`429` + `Retry-After`) before key lookup—including valid keys and polls. Missing headers and store outages do not count. Fixed in v2.0; enforce per-user limits at a shared TLS proxy if needed.
- Post-auth accepted runs never return SDK auto-retried `408`/`409`/`429`/`5xx`; uncertain commits return `422` with `x-should-retry: false`. See the official [Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) and [Models](https://developers.openai.com/api/reference/resources/models/methods/list) specs. Future Responses API and token streaming will share this auth, permission, and outcome model and stream only externally safe text in order.

## Slack Entrance

### App Setup

v2.0 receives Slack events and button clicks strictly via [Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/) (HTTP Request URL receive is unsupported). Configure only the rows needed for your deployment:

| Feature | Slack App Configuration | Gateway API / Events Used |
|---|---|---|
| Connection (required) | Enable **Socket Mode**; issue App-Level Token with `connections:write` under **Basic Information → App-Level Tokens**. | `apps.connections.open` (app token); `auth.test` (bot token) to verify team and bot user. |
| Messages & approval cards | Add bot scope `chat:write` under **OAuth & Permissions**; invite bot to target channels. | `chat.postMessage`, `chat.update` (button state updates), `chat.postEphemeral` (click notices). |
| Channel mentions | Bot scope `app_mentions:read`; enable **Event Subscriptions** and subscribe to bot event `app_mention`. | Human [`app_mention`](https://docs.slack.dev/reference/events/app_mention/) posts in registered channels. |
| Thread replies (`thread_reply`) | Public: `channels:history` + `message.channels`. Private: `groups:history` + `message.groups`. Invite bot and add `thread_reply` to binding `events`. | Human replies in **bot-participating threads** only ([`message.channels`](https://docs.slack.dev/reference/events/message.channels/), [`message.groups`](https://docs.slack.dev/reference/events/message.groups/)). |
| Reactions (`reaction`) | Bot scopes `reactions:read`, `users:read`; bot event `reaction_added`; add `reaction` to binding `events`. | Active human reactions on `item.type=message` in registered channels ([`reaction_added`](https://docs.slack.dev/reference/events/reaction_added/), [`users.info`](https://docs.slack.dev/reference/methods/users.info/)). |
| 1:1 DMs | Bot scope `im:history`; bot event `message.im`; allow user messages in **App Home → Messages Tab**; set workspace `dm: true` and add DM binding. | [`message.im`](https://docs.slack.dev/reference/events/message.im/) (`type: message`, `channel_type: im`). |
| Approval / stop buttons | Enable **Interactivity & Shortcuts → Interactivity**. | `block_actions` over Socket Mode. |
| Owner DM for channel schedules | Bot scope `im:write` (plus `chat:write` and Interactivity). | [`conversations.open`](https://docs.slack.dev/reference/methods/conversations.open/) to open a 1:1 DM with the owner (works even with DM intake disabled). |
| Reply file attachments | Bot scope `files:write`. | [`files.getUploadURLExternal`](https://docs.slack.dev/reference/methods/files.getUploadURLExternal/) → byte upload → `files.completeUploadExternal`. |
| Progress indicators & stop (optional) | Bot scope `reactions:write` (for 👀/✅/❌); enable Slack AI App / streaming with `chat:write` and Interactivity for streamed progress and stop cards. | `reactions.add`/`remove`, `chat.startStream`/`appendStream`/`stopStream`/`delete`, `agent_session_stopped`. |

Install (or reinstall after scope changes) the app in the workspace, store bot/app tokens in the profile's `SecretStore`, and configure `team_id`, `channels`, `bot_token`, and `app_token` names under `gateway.slack.workspaces.<alias>`.

### Intake, Approvals, and Delivery

- **Authenticated installation**: one Socket Mode session per `(profile, workspace alias)`. Verifies `app_id` on hello and `team_id` via `auth.test`. Events route only within that profile (no cross-profile fallback). Startup rejects duplicate `app_token` reuse across two profiles.
- **Event intake**: channel bindings select events via `events` (default `app_mention` only); `thread_reply` and `reaction` match trigger `on.event` values `thread-reply` and `reaction` ([Profile Spec](profile-spec.md)). DMs accept human `message` events (excluding edits, deletes, bots). Request ID `<channel>.<ts>` deduplicates across event types and redeliveries. Ack occurs only after durable commit. Edits, deletes, bot/app messages, self-reactions, wrong teams, unregistered channels, and malformed `ts` are ignored.
- **Thread replies**: bot participation evidence is `(team, bot ID, profile, binding, channel, root ts)`, recorded only from bot post receipts or authenticated self-posts observed on the socket (never from incoming `thread_ts`). Bot self-posts only record evidence and never trigger runs. Unverified threads ack and skip (`thread_unverified`); evidence store outages remain unacked for retry. Legacy threads prior to this version become eligible once the bot posts in them again (no backfill).
- **Single-intake deduplication**: `app_mention` and `message.channels` may carry different `event_id`s for the same post. Classification uses message content (bot mention wins if `mention` is enabled, else `thread_reply`), so arrival order never changes outcomes. Intake also deduplicates by Slack event identity — `(team, channel, ts)` for posts, `(team, event_id)` for reactions — so redeliveries after binding or profile changes converge on the first run (an upgrade migration backfills temporary keys for prior posts).
- **Reactions**: actor (`event.user`) is requester and must be a profile member; target is `item.channel` / `item.ts`; unmatched reactions ack without intake. Trigger body carries fixed fields `reaction`, `channel`, `item_ts`, `item_user`, with request ID set to Slack `event_id` (identity and evidence are never reconstructed from body).
  - *Human check*: calls `users.info` on **every** member reaction (no positive cache, so deactivated users stop immediately); requires matching `id`, `is_bot: false`, and omitted or boolean `false` `deleted` (`null`/string rejected); excludes self and `USLACKBOT` (`is_app_user` is not a rejection). Non-members never trigger `users.info`.
  - *Fail-closed ack*: missing `users:read`, rate limit, error, 2s timeout, or malformed response logs a fixed reason (`actor_not_human`, `actor_scope_missing`, `actor_unverified`) and acks within Slack's 3s limit without running candidates or fallback chat. Mentions and DMs do not require `users:read`.
  - *Reply thread*: replies in-thread only if the reacted message is the verified root of a bot-participating thread; otherwise replies at channel top level.
- **Passing unhandled events (`bot/pass`)**: direct tool triggers on authenticated Slack events may return `_meta["bot/pass"]: true` with empty `content`. Intake pins matching direct triggers in `order`; each PASS atomically commits verdict, cursor advance, and audit before advancing to the next candidate, falling back once to default chat (first agent-instructing trigger, else binding agent: raw text for mentions/replies, or `{"event":"reaction_added","item_ts":…,"reaction":…}` for reactions). Fallback requires `run` scope verified at chain creation and again on **both current and run-pinned revisions** when the last candidate passes (silently finishes if absent or revoked). Only read-only triage calls may pass ([Plugins](plugins.md)).
- **Attachments**: 3-step external upload (URL → bytes → complete); approval cards and attachments are never combined in one reply. In push conversations (channels/DMs), leading `<attachment name="attachment.md">...</attachment>` blocks in root-agent replies split out into follow-up `text/plain` attachment replies in the same thread (if the remaining body is empty, the attachment content becomes the body; delegated children, CLI, and HTTP preserve raw text).
- **Trailing notes**: trailing `※` lines separated from a Slack reply body render in a `context` block (`mrkdwn`) with broadcast mentions neutralized; notes exceeding Slack's 3,000-character block limit stay in the main `markdown` block (or fall back to plain text above the markdown block limit) without truncation.

### Progress Indicators and Stop

Standard accepted runs (`Admission::Accepted`) in 1:1 DMs and channel bot mentions display live progress and support in-thread cancellation (`crates/bot-gateway/src/slack/ux.rs`, `crates/bot-gateway/src/slack/ux/cards.rs`, `crates/bot-gateway/src/slack/ux/stop.rs`, `crates/bot-gateway/src/slack/ux/stream.rs`). Unmentioned `thread_reply`, `reaction_added`, and private DMs have neither progress UI nor stop handling.

- **In-memory watch**: tracks runs for up to 30 minutes (`WATCH_LIMIT`). After a gateway restart, expiry, or completion, Agent stop buttons do nothing, stop commands enter as normal questions, and stale confirmation card buttons update to `STOP_GONE` (`"This task has already finished or the prompt has expired."`).
- **Progress display**: reacts with 👀 (`eyes`), streams progress summaries via `chat.startStream` / `chat.appendStream` / `chat.stopStream` (marking the `task_update` complete before `chat.delete`), and replaces 👀 with ✅ (`white_check_mark`) on success or ❌ (`x`) on failure/cancel. When multiple requests overlap in one thread, only one progress card is shown at a time; completing a run closes its card while keeping `session_status: "processing"` and hands off sequentially to the next active run until every run finishes. Strictly decorative (API failures never block runs or delivery) and shows only fixed status copy, agent short names, and tool names—never user text, secrets, args, or drafts.
- **Stop paths**: (1) Slack Agent stop button (`agent_session_stopped`) posts a confirmation card; (2) a standalone stop command in an active thread (`"stop"`, `"cancel"`, `"abort"`, `"halt"`, `"stop please"`, `"cancel please"` — requires bot mention in channels, no mention in 1:1 DMs; extra text starts a new request) stops immediately; (3) confirmation card buttons **Stop here** (`bot_stop_confirm`) and **Continue** (`bot_stop_continue`) finalize the action.
- **Authorization and execution**: only the requester or a member with `admin`/`owner` role on the current revision may stop (others receive an ephemeral notice via `chat.postEphemeral`). Confirmed stops call `RunAdmin::cancel` across the run and child tree, audit `slack_user_stop`, and post the stopped card with ❌. If the run finished before confirmation, it reports completion instead.

## Secret Stores and Commands

`BOT_SECRETS` selects the secret backend:

- `env` (default, dev): reads `BOT_SECRET__<PROFILE>__<NAME>` (`-` replaced by `_`).
- `keychain` (macOS): service `bot/<profile>`, account `<name>`.
- `sops:<file>`: reads `bot.<profile>.<name>` from `sops --decrypt` JSON output.

Handles are pinned to one profile at creation and cannot read other profiles. Processes preload all declared secret names into the redactor at startup and refresh every `BOT_SECRETS_REREAD_SECS` (default `60`; `0` disables—raise for `keychain` shelling out to `security` or `sops` 30s cache). Runtime roles accept only redaction-wrapped stores (`Observe.secrets`); raw stores do not compile.

### CLI Commands

| Command | Behavior |
|---|---|
| `bot validate` | Offline validation (no DB/Slack) of profiles, host registry, entrance uniqueness, required secrets (declared, tool, remote HTTP bearer, entrance, observability, and allowed `claude-cli`/`codex-cli`/`agy-cli`/`anthropic-api`/`openai-api`, `embeddings`, and `memory_judge` keys—missing keys report name and env var), HTTP key length (≥ 32 bytes), memory seed body limit (8 KiB), agent `builtin` compatibility, and tool server installs. Prints activation and server revisions only when all checks pass. |
| `bot chat [--profile P] [--thread T] [--request-id ID] [--no-wait] "text"` | Sends a chat request (e.g., `bot chat --profile personal "Show today's agenda"`, `bot chat --profile personal --request-id walk-1 "Delete the walk event"`). Re-attach after disconnect with `bot chat --profile P --attach --request-id ID`. Exit code `3` is retryable. Escapes terminal control characters (ESC, C1, BEL, CR, DEL, etc.) as `\u{1b}`-style literals so model/tool output cannot trigger OSC 52 clipboard writes, title changes, or screen clears (`\r\n` normalizes to `\n`). |
| `bot map [--json] [--profile P]` | Prints structure map (names, classifications, and counts only). |
| `bot init <name> [--template personal\|team\|company] [--dir <dir>]` | Scaffolds `<name>/` (`host.json` with demo model + current OS user CLI account, plus `profiles/<name>/`) passing `bot validate`. Lowercase alphanumeric + hyphen names; never overwrites or follows symlinks; validates in a `$TMPDIR` copy before atomically moving a temp dir under `--dir` (opened once with final umask permissions); ignores `BOT_*` env vars. |
| `bot add agent <lineage> [--dir <dir>]` · `bot add skill <name> [--agent <lineage>] [--dir <dir>]` | Scaffolds `AGENT.md` or `SKILL.md` (`main/cs` → `agents/main/agents/cs/`; omitting `--agent` creates a profile-wide skill). Naming a skill `skill-creator` generates the full super-admin live-management `SKILL.md`. Never creates `tools.yaml` or edits `profile.yaml`. Rejects missing parents, duplicates, or visible skill-name collisions; operates relative to one opened dir fd and cleans only empty dirs on failure. |
| `bot add tool <skill>.<tool> [--class read\|write\|irreversible] [--server <alias>] [--agent <lineage>] [--dir <dir>]` | Creates or appends to `<skill>/tools.yaml` (defaults: `--class read`, `--server` existing server or skill name) after validating `tools.schema.json` and lock rules. Never edits `profile.yaml` or `host.json`; prints required `servers`/`roles`/`policy`/`approve` snippets. Rejects invalid names, reserved built-in IDs, duplicates, missing/ambiguous skills, server mismatches, and symlinks ([First Tool](first-tool.md)). |
| `bot doctor [--host <host.json>] [--profiles <dir>] [--check-db]` | Summarizes host config, model providers, tool servers, profiles, entrances, required secret **names**, and DB URL config in English using runtime validators (offline by default: no network, migrations, or server spawns; remote DNS/connect unchecked; `sops` marked `SKIP`). `--check-db` runs a 5s read-only DB ping. Never prints secret values, DB URLs, or raw DB errors; `SKIP` ≠ `PASS`; exits `1` on any `FAIL`. |
| `bot mcp sync <skill-dir>` | Fetches `tools/list` from a remote `transport: "http"` server for tools declared in `tools.yaml` and writes deterministic `tools.lock.yaml` (name, description, input schema, sha256) after lexical and secret-leak checks. Broker verifies hashes before calling ([Plugins](plugins.md)). |
| `bot profile-bundle put <profile-dir>` | Backfills a historical profile directory bundle into the store when `bot serve` cannot restore a revision still referenced by in-flight work. Self-hashed, ignores host registry, immutable (delete corrupt rows manually before re-putting). |
| `bot serve --role <roles> [--activate]` | Startup sequence: validate → read-only active revision check → store current bundles and restore historical revisions needed by in-flight runs, resumable dead letters, pending cards, static cron windows, and active user schedules → run role checks and bind listeners → atomically commit active revision + `bot_store::check_seed` memory seeds → open intake. Failures before commit leave active revision, runs, cards, and attempts untouched. Joins silently if identical, bootstraps if empty, and CAS-swaps only with `--activate`. |

### Environment Variables and Logging

| Variable | Purpose |
|---|---|
| `BOT_PROFILES`, `BOT_HOST_CONFIG` | Profile root directory and `host.json` path (demo config if unset). |
| `BOT_DATABASE_URL`, `BOT_DB_MAX_CONNECTIONS` | PostgreSQL DSN and pool cap (default `10`, range `1..=200`). |
| `BOT_SOCKET`, `BOT_INSTALLS` | CLI Unix socket override and read-only tool server install root. |
| `BOT_SECRETS`, `BOT_SECRETS_REREAD_SECS` | Secret store backend and redactor refresh interval in seconds (default `60`, `0` disables). |
| `BOT_ADMIN_TOKEN_FILE` | Absolute path (`0600`) for headless Admin UI operator secret delivery. |
| `BOT_LOG`, `BOT_LOG_FORMAT` | Log level (`error`, `warn` default, `info`, `debug`, `trace`) and stderr format (`text` default, `json` line-delimited `{ts_ms, level, target, message, fields}`). |

Intake, delivery, and approval emit `bot_core::trace` spans (`bot.private` marks private DMs) carrying only identifiers and vocabulary codes—never bodies, args, nonces, or receipts. `BOT_LOG` applies only to workspace crates: external HTTP/WebSocket/TLS crates cap at `warn` and `sqlx` is disabled completely so `trace` never leaks raw frames, private DM text, or unknown DSN params (`crates/bot-cli/src/logging.rs`). When `admin.logs` is enabled, filtered logs also ship to OTLP `/v1/logs` (`admin.logs.level` bounded by `BOT_LOG`); when `admin.issues` is enabled, errors, panics, and abnormal failed spans report to the issue tracker.

## Running Roles

`--role all` starts `gateway`, `scheduler`, `worker`, and `admin` together. Split deployments share PostgreSQL and the same active set.

- **Gateway**: handles intake and delivery. Private DM triggers require the target tool's `srt` runner and per-profile `private-intake` fingerprint key; raw text goes only to the designated tool, and uncertain outcomes become operator holds.
- **Worker**: runs models and the embedded tool broker. Refuses to publish an active revision if any allowed model alias (`claude-cli`, `codex-cli`, `agy-cli`, `anthropic-api`, `openai-api`, excluding keyless loopback) or configured `embeddings` / `memory_judge` endpoint lacks its provider key. Missing tool secrets warn by name only; calls fail before opening approval cards and audit `secret_missing` (`bot validate` reports both; see [Reliability](reliability.md)).
- **Scheduler**: enqueues due runs. Static triggers commit atomically with the active revision and memory seeds regardless of which role publishes. Removed trigger windows close at switch time while prior occurrences stay in their window ([Profile Spec](profile-spec.md)).
- **Admin**: defaults to `127.0.0.1:8787` (HTTP intake defaults to `127.0.0.1:8788`). Authenticated data responses re-verify the active set right before sending (`503` + fixed error code on drift/outage; see [Admin UI](admin.md#startup-sequence)).

Slack buttons and the Admin UI share the same broker approval path. If an external tool's install revision cannot be verified, the approval card stays pending and returns a transient error.

### Slack Scheduled Destinations

Any channel registered in the profile is selectable as a schedule destination (remove from registry to restrict) by members with `schedule.create`. Slack users may specify registered channels by alias, channel ID, or canonical route; CLI targets only the origin conversation; HTTP principals cannot invoke user schedule tools.

1. **Creation**: pins binding and `team_id/channel_id` from the registry; the owner must review and approve route, thread, and instruction (no live Slack channel membership lookup).
2. **Fire**: re-verifies the route against the current registry. Removed routes close the schedule and notify the origin once; transient errors retry.
3. **Delivery**: quiet runs returning `<quiet>` with no attachments suppress origin replies; explicit-destination drafts require separate owner approval (`Delivery draft`), re-checked before outbox enqueue and again by the Slack adapter before posting by channel ID ([Reliability](reliability.md)).

For channel-origin schedules, v2.0 opens a 1:1 DM with the owner via the channel binding's Slack installation (reusing the DM if the origin is already a DM) and posts a single fixed notice in the channel after confirming DM delivery. This path is approval-only and never changes normal reply routing or DM intake ([Reliability](reliability.md)).

### In-Flight Model Requests During Deployments

VM CLI workers (`claude-cli`, `codex-cli`, `agy-cli`) bind a relay on the anchor VM gateway address and pass the listening port to the model VM. Before each upstream request, the relay checks the active set (`403` stale, `503` outage/timeout—never sends upstream). Open streams re-check every 1s and must resolve within 10s of the first scheduled check or terminate with a fixed error event ([Architecture](architecture.md), [Reliability](reliability.md)).

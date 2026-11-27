# MCP Tool Plugins and Built-In Tools

English | [한국어](ko/plugins.md)

Operators register tool servers in `host.json` (`BOT_HOST_CONFIG`) and profiles reference them only by server alias in `tools.yaml`. Servers are language-agnostic MCP implementations (`initialize`, `tools/list`, `tools/call`) requiring no framework library. See [first-tool.md](first-tool.md) for a minimal walkthrough and [profile-spec.md](profile-spec.md) for profile schema rules.

## Local Tool Servers (MCP stdio)

Declare local servers under `servers` in `host.json`:

```json
{
  "servers": {
    "agenda": {
      "root": "/opt/bot/servers/agenda",
      "command": ["bin/server", "--stdio"]
    }
  }
}
```

- `root` is an absolute code-only directory; `command[0]` is a relative executable regular file inside `root`; remaining entries are passed without shell expansion. Symlinks and special files are rejected. Keep secrets and runtime data outside `root`.
- **Content-addressed installs:** At startup, the host copies `root` into a read-only installation and computes a deterministic SHA-256 revision from file contents, relative paths, executable bits, directories, `command`, and `state` ([`crates/bot-core/src/install.rs`](../crates/bot-core/src/install.rs)). Approval cards and broker execution pin this revision—changing code invalidates prior cards. Split deployments must deploy identical packages to decision and execution hosts.
- **Server state (`state`):** Defaults to `"ephemeral"` (fresh scratch directory per call). Set `"required"` for servers needing persisted files across calls (folds into the install revision). Current `srt` has no persistent-volume adapter and refuses `state: "required"` at startup. Future adapters must expose a dedicated `0700` `(profile, server)` volume via `BOT_STATE_DIR` (unoverridable by tool `env`), block cross-profile/model access and symlink escape, enforce disk quotas, and use inter-process write locks across hosts.
- **Private egress (`private_egress`):** For local tools calling private-network services, operators pin per-profile destinations in `host.json`, e.g. `{"private_egress":{"personal":{"calendar.internal:443":"10.0.0.8:443"}}}`. After verifying tool `egress` ∩ profile cap, the sandbox proxy connects only to the pinned IP and port.

## Remote Tool Servers (Streamable HTTP)

Connect already-running MCP servers over Streamable HTTP without installing local files:

```json
{
  "servers": {
    "docs": {
      "transport": "http",
      "url": "https://mcp.example/api",
      "auth": {"kind": "bearer", "secret": "docs-token"}
    }
  }
}
```

- **URL & HTTP rules:** `https` required; `http` is allowed only for canonical IPv4 loopback literals (`127.0.0.0/8`). Rejects IPv6 literals (including `[::1]`), `user:pass@`, `#` fragments, invalid ports, and non-canonical numeric IPv4 forms (`0x7f000001`, `2130706433`, `0177.0.0.1`, `127.1`, trailing dots). No redirects, no hidden retries, bounded response size and timeout.
- **Bearer auth (`auth`):** Optional. Names a secret in the calling profile's secret store (`Authorization` header only; never logged or audited). Any server response containing the secret value anywhere (raw body, decoded text, keys, numeric encoding, `nextCursor`, session ID) is rejected (`response_credential` on discovery/list, `result_credential` on tool call). Missing secret blocks the call.
- **Egress & DNS pinning:** Exact `url` `host:port` must appear in tool `egress` ∩ profile cap. Every connection validates all DNS answers (rejecting private, loopback, or link-local IPs without loosening via global private-network settings) and dials the vetted IP directly. Before every outbound step (`server/discover`, `tools/list`, legacy `initialize`, `tools/call`, session teardown), the broker re-verifies the process's active revision and aborts unsent if stale or unreadable.
- **Protocol negotiation:** Supports MCP `2026-07-28` (probed via read-only `server/discover`; repeats `x-mcp-header` arguments as `Mcp-Param-*` headers) and legacy `2025-11-25`, `2025-06-18`, `2025-03-26` (uses `initialize` -> `initialized` and session ID; sends `MCP-Protocol-Version` from `2025-06-18` onward). Never uses write `tools/call` for version probing or retries.
- **Constraints & failure semantics:** Remote tools cannot declare `effects: true`, `secrets`, or `env` (rejected at activation); outbound `_meta` carries only `bot/call-id`, `bot/profile-id`, and `bot/run-id`. Only `2xx` responses with matching JSON-RPC request IDs are authoritative; disconnects, timeouts, and non-`2xx` responses (including `401`/`403`/`5xx`) leave execution unknown (writes hold, reads retry transiently). Install revision binds `url`, `transport`, and bearer secret name.

### Tool Definition Locking (`tools.lock.yaml`)

When a server exposes a non-kebab tool name (e.g. `get_forecast`), set `name` on the `tools.yaml` entry (`[A-Za-z0-9_.-]+`, <= 128 chars; defaults to `id`). Models, scopes, and approvals use `skill.id`; MCP `tools/list`, `tools/call`, and `Mcp-Name` use `name`. Lock mismatches fail with `lock-name-mismatch`.

When `input` is omitted in `tools.yaml`, the loader reads the input schema and description from sibling `tools.lock.yaml` (fallback order for model-visible description: lock description -> schema `description` -> tool ID, max 4 KiB UTF-8; inline `input` ignores the lock; omitting both fails with `missing-input`). Review synced `description` text for prompt-injection instructions before deploying.

```sh
bot mcp sync <skill-dir-under-BOT_PROFILES>
```

- Validates directory structure, symlink absence, `profile.yaml` (`id == folder`), and target `SKILL.md` (front matter, name match, single-file 64 KiB cap) using the same loader as `bot validate`, reading credentials only from that profile's secret store.
- Fetches all `tools/list` pages (up to 16 pages / 4,096 tools; rejects invalid/repeating `nextCursor` or mid-fetch `list_changed`), filters to tools declared in `tools.yaml` (failing on missing or duplicate server names), scans the entire response for leaked credentials before accepting, validates input schemas against the profile vocabulary, and writes deterministic `{id, name, description, input_schema, sha256}` entries where `sha256` is the RFC 8785 canonical JSON digest of `{"name","description","inputSchema"}`.
- Currently supports remote (`transport: "http"`) servers. Run `bot validate` afterward to verify full profile references and per-agent 64 KiB skill sums.
- At execution time, the broker compares the server's live tool definition digest against the lock before calling (`tool_definition_changed`, `tool_missing`) for both HTTP and stdio servers.

## Tool Declarations and Standard Call Flow

Each tool in `tools.yaml` requires `id`, `class` (`read`, `write`, `irreversible`), and `scopes` (>= 1), plus optional `name`, `external`, `effects`, `env`, `secrets`, `egress`, `timeout_secs`, `destination`, and `input`.

1. **Authorize & approve:** Broker pins tool name, revision, and arguments; checks scopes, risk class, and rate limits; blocks until any required human approval completes.
2. **Isolate:** Starts a fresh sandbox per call from an empty environment containing only declared `secrets` and `env` keys (verifying `env` names and values match both pinned and current revisions), plus host-injected proxy and callback URLs.
3. **Invoke:** Verifies negotiated MCP version and tool schema, then sends `tools/call` with host-owned `params._meta`, e.g. `{"bot/profile-id":"personal","bot/run-id":"r-3","bot/call-id":"c-17","bot/generation":4,"bot/principal":"local:owner","bot/trigger":"message"}` (and `bot/destination` for `destination: "reply"` tools). `stdout` is MCP; `stderr` is bounded and redacted; no parent FDs are inherited.
4. **Record & terminate:** Persists the result by logical call ID and terminates the process. Downstream idempotency keys hash `[profile, run, call(, step)]` (excluding `generation` and claim). Unknown outcomes without safe read verification hold the call.

- **Identity & origin:** `bot/principal` preserves the authenticated requester (`local:<account>`, `slack:<ws>/<user>`, `http:<endpoint>/<sub>`, `service:<name>`) across delegation, approvals, and resumes; models cannot spoof `_meta`. `bot/trigger` records root origin: `message` (CLI/Slack human input including private DM), `http`, `schedule` (static cron or user schedule), `wake`, or `unknown`. Interactive tools waiting for user replies should accept only `message`.
- **Timeouts & limits:** Default timeout is `60s`; `timeout_secs` allows `1..=900` up to host `limits.tool_call_max_secs` (default `60`, max `900`; exceeding fails activation with `tool-timeout-over-cap` or blocks pre-call with `tool_timeout_over_cap`). Sandbox deadline is `max(300s, timeout + 240s)`; proxy tunnel deadline is `600s` (raised to match when `timeout_secs > 600`). Callback frames are capped at 64 KiB. Post-send `timeout` is unknown (writes hold without auto-retry; see [reliability.md](reliability.md)).
- **Input schema vocabulary:** Supported keywords: `type`, `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`, `minItems`, `maxItems`, `minLength`, `maxLength`, `minimum`, `maximum`, `pattern`, `anyOf`, `oneOf`, `allOf`, `title`, `description`, `default`, plus non-validating annotations (`$schema`, `$comment`, `examples`, `format`, `deprecated`, `readOnly`, `writeOnly`). Local `$defs`/`definitions` and `#/$defs/<name>`/`#/definitions/<name>` `$ref`s are expanded at compile time (percent-decoded, then JSON Pointer `~1`/`~0`); `$ref` siblings may be annotation keywords only; recursive/external/missing refs or > 20,000 expanded nodes are rejected. `pattern` uses portable Rust regex syntax without `\d`/`\w`/`\s`/`\b`, inline flags, or lookaround.
- **External & destination declarations:** Set `external: true` (default `false`) on any tool that sends input to third parties (including search); calling an `external` tool after recalling private memory escalates approval. `destination: "reply"` injects the original conversation into `_meta["bot/destination"]`. `destination: {recipient: "args.<path>", content: "args.<path>"}` targets an argument-specified recipient and automatically elevates the tool's effective class to `irreversible`.

## Optional Callbacks for Multi-Step Writes

Tools performing multiple external writes declare `effects: true` to record step progress against `BOT_EFFECT_URL` (`http://bot-effects.invalid/effect`, handled locally by the per-call proxy with no extra `egress` entry; NDJSON over bidirectional FD where supported). Approval still happens once at the call boundary.

```jsonl
{"v":1,"id":"0","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"}}
{"v":1,"id":"0","state":"none","send":false}
{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary","target":"inbox"}}
{"v":1,"id":"1","state":"started","effect":"e-17","send":true}
{"v":1,"id":"2","method":"effect.status","effect":"e-17","outcome":"confirmed","receipt":"file-17"}
{"v":1,"id":"2","state":"confirmed","receipt":"file-17","send":false}
```

- **`effect.request`:** Binds the argument digest to `(call_id, step)`. Returns `send:true` only when claiming a new step or consuming an operator `not_executed` verdict atomically. Mismatched args for an existing step return `conflict`.
- **Pre-approved calls:** Static cron tools with `preapproved` ([profile-spec.md](profile-spec.md#pre-approved-static-calls-and-designated-human-approval)) must send `args: {"operation":"...","target":"...","params":{...}}` for every write. New `send:true` grants require an exact allowed `(operation, target)` match and consume 1 UTC daily quota (`denied` for out-of-scope/quota/paused; `stale` for revoked/replaced/old-pause-generation; `invalid` for bad shape; `conflict` on valid-grant args mismatch).
- **`effect.status`:** Querying with `{step, args}` never claims a step (`none`, `started`, `held`, `rejected`, or `confirmed` with `receipt`; always `send:false`). Querying/reporting by `effect` + `outcome` (`confirmed`/`rejected`/`held`) records final step status; mixing step lookup with `effect`/`outcome` returns `invalid`. On `unavailable`, retry status lookup without writing.
- **Credential scrubbing:** Receipts, hold reasons, and step names are scanned before persistence. Credential leaks in a step name return `invalid`; leaks in receipts or hold reasons hold the step with `receipt_credential` or `reason_credential` without persisting the raw text.
- **Recovery after operator `not_executed` verdict:** Status returns `resolution: "not_executed"` (tools cannot send `resolution`):

```json
{"v":1,"id":"7","state":"held","effect":"e-17","resolution":"not_executed","send":false}
```

1. Call `effect.status{step, args}`. Reuse `receipt` on `confirmed`, exit on `rejected`, and verify externally without writing on `started` or unresolved `held`.
2. On `none` or `resolution: "not_executed"`, perform a read-only check at the downstream system (crashing here does not consume the verdict).
3. If the write is still needed, call `effect.request{step, args}` and write **only if `send:true`**, then report the outcome.

## Built-In Tools Exposed to Models

All 15 framework-reserved tool IDs (`delegate`, `memory.share`, `memory.search`, `memory.save`, `memory.forget`, `schedule.create`, `schedule.list`, `schedule.cancel`, `schedule.deliver`, `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove`) cannot be declared in `tools.yaml`.

- `anthropic-api`, `openai-api`, and VM CLI runners expose `memory.share`, `memory.search`, `memory.save`, `memory.forget`, `schedule.create`, `schedule.list`, and `schedule.cancel` when within the agent's scope cap, plus `delegate` (`{"agent":"main/research","instruction":"Summarize today's agenda"}`) for agents with direct children. The host uses `schedule.deliver` internally for delivery-draft approval.
- For profile `super_admins` holding the matching scopes, the 6 live-management tools below are also exposed (hidden from non-super-admins; direct calls fail with `not_super_admin`). Claude CLI exposes dot names as underscores (`memory_share`, `skill_create`); API runners map aliases per request.

| Super-admin tool | Class | Behavior |
|---|---|---|
| `skill.inspect` | `read` | Inspect agent tree, descriptions, skill byte budget (64 KiB cap), skills (or one skill body), MCP servers, secret backend, and secret **names** (never values). |
| `skill.create` | `write` | Create a profile-wide or agent-scoped `SKILL.md`, validate, and activate live. |
| `skill.update` | `write` | Update an existing skill's `description` or body (preserving `tools.yaml`/`tools.lock.yaml`), validate, and activate live. |
| `skill.delete` | `write` | Remove a skill folder and prune unused tool scopes, validate, and activate live. |
| `mcp.add` | `write` (`external`) | Register a remote Streamable HTTP (`tools/list` auto-locked) or local stdio MCP server, store secrets encrypted, validate, and activate live. |
| `mcp.remove` | `write` | Remove an MCP server, its skill tool bindings, and unused scopes (`remove_secrets: true` also deletes unused encrypted secrets), validate, and activate live. |

- Management mutations are atomic: any validation, install, or DB failure rolls back disk edits. Success commits the new bundle and active revision while preserving existing `bot_grants`, then hot-reloads the in-process registry, runners, sandboxes, and decision gate with no restart.
- Secret values (`secret_value`, `secret_values`) are registered with `Redactor`, stored encrypted in SOPS (`sops set`) or keychain (`bot/<profile>/<name>`) with immediate cache flush, and never written to plaintext files or approval cards.
- The broker checks pinned and current permissions on every built-in call. `memory.share` requires the owner to approve the exact body from their DM or `bot chat`. `memory.search` runs hybrid (embedding + lexical) recall under the same visibility and `private_memory_read` taint rules as turn-start recall. `memory.save` (private, 1:1 DM/CLI only, 500 active entries per owner, automatic exact/near-duplicate skipping, optional `kind: "core"|"fact"` and non-destructive `supersedes`) requires owner approval only when `policy.approve` lists `memory.save` (computing embeddings only after approval passes); when `memory.save` is in scope, the runtime injects concise `## Memory` guidance so models save durable facts proactively, and `host.json` `memory_judge` can additionally automate save/supersede/forget decisions on 1:1 DM/CLI user turns. `memory.forget` soft-deletes one owner-visible memory in 1:1 DM/CLI. Confirmed rejections replay deterministically by call ID ([reliability.md](reliability.md)).

### Schedules and Follow-Up Wakes

- **Chat schedules:** `schedule.create` takes `instruction`, either `at` (UTC RFC 3339, up to 1,830 days ahead) or `cron` (5-field + optional IANA `timezone`), optional `quiet` (default `false`; quiet runs returning `<quiet>` suppress replies), and optional `destination: {binding, conversation: {kind, id, thread?}}`. `schedule.list` lists the caller's active schedules; `schedule.cancel` cancels by `{"id":"<schedule-id>"}`.
- **Recipient rules:** Defaults to the originating conversation. Profile members with `schedule.create` may target other registered Slack channels on the same profile; the owner approves the recipient and instruction at creation and approves each `Delivery draft` card (`schedule.deliver`) when the schedule fires. CLI users may schedule only to the current conversation; DM bindings cannot target arbitrary DMs; HTTP and service principals cannot manage user schedules.
- **Tool-initiated wakes (`_meta["bot/wake"]`):** A successful non-private MCP tool result may include `"_meta":{"bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"Check if files arrived","quiet":true}}`. Requires `effects: true`, `schedule.create` scope on the tool and current requester/agent, and verified prior step receipts. Schedules one wake to the originating conversation and strips `_meta["bot/wake"]` from model output. Holds the call if wake creation fails after an external effect.

### Passing Unhandled Events (`bot/pass`)

On authenticated Slack **direct-tool triggers**, a tool may decline an event so the broker advances to the next candidate trigger or fallback chat (ignored and stripped on model, delegation, HTTP, cron, and wake calls):

```json
{"content":[],"isError":false,"_meta":{"bot/pass":true}}
```

- **Strict shape:** Requires top-level `_meta["bot/pass"]: true`, empty `content`, `isError: false`, and no `structuredContent` or extra `_meta` keys (`bot/wake`, etc.). Any extra field closes the candidate as `invalid_pass` without advancing.
- **No prior side effects:** Allowed only after pure reads. Any started/held/unknown effect step, outbox reply, or pending approval closes as `pass_after_effect`. Write-class tools without `effects: true` cannot pass; `effects: true` tools that recorded zero writes can pass. **Tools must log every external write via `effect.request` and never return `bot/pass` after writing**, because the host detects writes only through the effect ledger, outbox, and approvals.
- **Commit & privacy:** PASS confirmation, call conclusion, cursor advance, and audit commit atomically with distinct candidate call IDs (`<run>:tool`, `<run>:tool:1`, ...), and the marker is stripped from all outputs. On a `private: true` DM trigger, returning a valid empty PASS promotes the raw body and attachments to normal DM chat only when `private_pass: true` is declared; otherwise unpermitted passes hold in the private ledger. Tool errors, rejected approvals, and transient failures are not PASS.

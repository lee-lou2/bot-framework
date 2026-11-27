# Profile Specification

English | [한국어](ko/profile-spec.md)

`BOT_PROFILES` holds one or more declarative profile directories. Example layout for a personal assistant:

```text
profiles/personal/
  profile.yaml
  rules.md
  knowledge/
  skills/
  agents/main/
    AGENT.md
    env.yaml
    triggers.yaml
    memory/
    knowledge/
    skills/agenda/{SKILL.md,tools.yaml[,tools.lock.yaml]}
    agents/research/AGENT.md
```

`profile.yaml` and each `AGENT.md` are required; every skill requires `SKILL.md`; everything else is optional. Each profile has one root agent, and agents delegate only to direct children. Agent IDs are lineage paths (`main/research`); tool IDs are `<skill>.<tool>` (`agenda.list`). Each segment is kebab-case, max 63 chars: `^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$`.

## Minimal Profile

These two files are sufficient to run a CLI chat as `local:owner` using the `assistant` model alias allowed by the host.

**`profile.yaml`**

```yaml
version: 1
id: personal
model: assistant
roles: {owner: [run]}
members: [{identity: "local:owner", roles: [owner]}]
bindings: [{id: desk, adapter: cli, account: owner, agent: main}]
```

**`agents/main/AGENT.md`**

```markdown
Summarize the user's tasks for today concisely in English.
```

| `profile.yaml` field | Requirement / default |
|---|---|
| `version`, `id`, `model` | Required. `version` is `1`; `id` matches folder name; `model` is a host-allowed model alias. |
| `roles`, `members`, `bindings` | Required. Unregistered principals and entrances are rejected. |
| `timezone`, `secrets`, `super_admins` | Defaults: `UTC`, `[]`, `[]` (non-empty `super_admins` folds into the policy revision). |
| `policy.scopes`, `policy.agents`, `policy.approve` | Defaults: union of role scopes, inherited parent cap, `[]`. |

### Agent Built-In Read Tools

Model CLI built-in tools are disabled by default (Claude Code runs with `--tools ""`); broker-authorized MCP tools remain available. Agents that inspect files opt in explicitly in `AGENT.md` YAML front matter ([`agent.schema.json`](../schemas/v1/agent.schema.json)):

```markdown
---
description: Inspect source files and cite exact evidence
builtin: [read]
---
Locate relevant files and evidence in the provided workspace.
```

- Allowed `builtin` entries: `read` (file contents), `grep` (text search), `glob` (filename search). Omitted or `[]` grants none. Child agents do not inherit `builtin`. Duplicates, shell/write/web tools, or unknown names fail validation.
- `builtin` never mounts extra host paths; model VMs mount only the attempt's read-only input directory and writable scratch directory, with no session token file on disk.
- Direct API providers (`anthropic-api`, `openai-api`) have no built-in file tools and reject agents declaring `builtin` pre-call (`unsupported_builtin`). Pinned Claude Code `--bare` VMs support `read` only and reject `grep` or `glob` before launch. The worker assembles inherited `rules.md` and `AGENT.md` instructions as the system prompt ([architecture.md](architecture.md)).

## Host Configuration for a New Profile

The built-in demo host allows model alias `assistant` only for `personal` and `team`. To run a new profile such as `notes`, allow its alias under `models.notes` in `host.json`; otherwise `bot validate` fails with `model-not-allowed`.

Replace `uid` with `id -u` and `root` with the absolute path to the server directory (`admin.bind` defaults to `127.0.0.1:8787`):

```json
{
  "version": 1,
  "gateway": {"cli": {"accounts": {"owner": {"uid": 1000}}}},
  "models": {"notes": ["assistant"]},
  "providers": {"assistant": {"kind": "demo"}},
  "servers": {
    "demo-agenda": {
      "root": "/opt/bot/examples/servers/demo-agenda",
      "command": ["bin/server", "--stdio"]
    }
  },
  "admin": {"bind": "127.0.0.1:8790"}
}
```

Set `BOT_PROFILES` to the parent directory of `notes/` and `BOT_HOST_CONFIG` to the absolute path of `host.json`, then run `bot validate`. To open CLI entrances on two profiles simultaneously, map distinct OS users and account aliases. Use `bot map --profile notes` to inspect the delegation tree and permission caps. The demo model calls `agenda.list` for reads and `agenda.list` + `agenda.delete` for simulated deletes; testing delegation or knowledge/memory recall requires a real model provider.

### Host Registry Keys for Profiles

`BOT_HOST_CONFIG` is a strict JSON file (<= 1 MiB, `version: 1`; unknown keys are rejected). Profile-relevant keys:

| Section | Keys | Purpose |
|---|---|---|
| Top-level | `version`, `gateway`, `models`, `providers`, `servers`, `egress`, `private_egress`, `super_admins`, `sandbox`, `limits`, `admin` | Host authority registry (`models`, `egress`, `super_admins`, `private_egress` are keyed by profile ID). |
| `providers.<alias>` | `kind`, `model`, `endpoint`, `secret`, `effort`, `max_tokens`, `max_completion_tokens` | `kind` is `demo`, `claude-cli`, `anthropic-api`, or `openai-api`. `effort` is `low`/`medium`/`high`/`xhigh`/`max`. `max_tokens` (1..=1,000,000) and OpenAI-only `max_completion_tokens` are mutually exclusive. `secret` is optional only on loopback direct API endpoints. |
| `servers.<alias>` | `transport`, `root`, `command`, `state`, `url`, `auth` | Local stdio (`transport: "stdio"` default): `root`, `command`, `state` (`"ephemeral"` default or `"required"`). Remote HTTP (`transport: "http"`): `url`, optional `auth` (`{"kind":"bearer","secret":"<name>"}`). |
| `sandbox` | `srt` (`binary`, `work_root`, `extra_path`), `apple` (`image`, `network`, `relay_bind`, `work_root`, `instance`, `binary`) | Tool sandbox (`srt`, `extra_path` up to 16 absolute dirs) and digest-pinned model VM (`apple`, see [model-image.md](model-image.md)). |
| `limits` | `tool_call_max_secs` | Per-tool timeout ceiling in seconds (default `60`, max `900`). Folds into the activation fingerprint when non-default. |
| `gateway` | `cli` (`accounts`, `uid`, `os_user`), `slack` (`workspaces`, `team_id`, `bot_token`, `app_token`, `dm`, `channels`), `http` (`endpoints`, `kind`, `keys`, `secret`, `subject`, `wait_ms`, `max_active_runs`, `issuer`, `audience`, `key`) | Entrance mappings verified at activation ([gateway.md](gateway.md)). |
| `admin` | `bind` | Local Admin UI/API address (`Admin UI: http://...`, see [admin.md](admin.md)). |

## Full Profile: Scheduling, Inheritance, and Memory

```yaml
version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, calendar.read, calendar.delete]
  reader: [calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "service:morning", roles: [reader]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run, calendar.read, calendar.delete]
  agents:
    main/research: [run, calendar.read]
  approve: []
```

- **Bindings:** Map an external entrance to one agent: CLI (`adapter: cli`, `account`), Slack channel (`adapter: slack`, `workspace`, `channel`, optional `events`), Slack DM (`adapter: slack`, `workspace`, `dm: true`, optional `events: [dm]`), webhook HTTP (`adapter: http`, `endpoint`), or OpenAI-compatible HTTP (adds `model` and optional `approvers`). No entrance may be bound twice across active profiles.
- **Inheritance:** Instructions concatenate `rules.md` -> root `AGENT.md` -> ... -> current `AGENT.md`. An agent sees only its own and ancestors' `skills/` and `knowledge/`; skill name collisions along a visible path are rejected. Knowledge search reads up to 8 documents / 32 KiB per request with source citations. `AGENT.md` front matter accepts `model` (inherits parent or profile `model`), `description` (single line <= 200 chars, non-inherited, defaults to folder name), and `builtin`. `env.yaml` is a non-secret string map inherited top-down with child overrides and no shell expansion or host env inheritance.
- **Memory seeds (`memory/`):** Bootstrapped once per `(profile, agent, relative path, content hash)`. Front matter requires `owner` (a profile member); `visibility` defaults to `private` (`profile` for shared). Recall filters by profile, agent, and requester visibility (`private` only in the owner's DM/CLI; channels, HTTP, and schedules see `profile` only) and marks recalled entries as untrusted reference input. Child delegation preserves requester and conversation kind. Runtime memories pin `owner` to the authenticated principal; only the owner may edit, delete, or approve sharing to `profile`.

## Skills, Tools, and Triggers

### `SKILL.md` Front Matter and Size Limit

`SKILL.md` accepts optional Agent Skills YAML front matter delimited by top and closing `---` lines ([`skill.schema.json`](../schemas/v1/skill.schema.json)). Unclosed leading `---` (including a standalone `---` at EOF) fails with `front-matter-unclosed` (legacy `AGENT.md` standalone `---` remains compatible).

| Front matter key | Rule |
|---|---|
| `name` | Optional; 1..=64 chars `[a-z0-9]+(?:-[a-z0-9]+)*`; must match the skill folder name (`skill-name-mismatch`). |
| `description` | Optional; 1..=1024 non-blank chars; preserved in structured metadata. |
| `license` | Optional; non-blank string. |
| `compatibility` | Optional; 1..=500 non-blank chars. |
| `metadata` | Optional; map of non-empty string keys to string values. |

- **Rejected keys:** `allowed-tools` fails with `skill-allowed-tools`; any other unknown key fails with `schema`. Skills cannot grant permissions ([security.md](security.md)); tool permissions live solely in `tools.yaml` and `profile.yaml` (`roles`, `policy`).
- **Prompt body & size cap:** Front matter is stripped before prompting; empty bodies fail with `empty-file`. Visible skill bodies per agent (excluding front matter) are capped at **64 KiB (65,536 bytes)** (`skill-too-large`). Move large references to `knowledge/` or scope skills to sub-agents.
- **Super-admins & `skill-creator`:** Listing non-service members (`local:*`, `slack:*/*`, `http:*/*`) in `profile.yaml` `super_admins` (`service:*` -> `super-admin-service`, non-member -> `super-admin-not-member`, outside host `super_admins.<profile>` ceiling -> `super-admin-not-allowed`) and granting `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove` lets super-admins manage skills and MCP servers live via `bot add skill skill-creator` ([plugins.md](plugins.md#built-in-tools-exposed-to-models)).

### `tools.yaml` and `tools.lock.yaml`

`tools.yaml` binds a skill's tools to one host-registered `server` alias ([`tools.schema.json`](../schemas/v1/tools.schema.json)):

```yaml
version: 1
server: demo-agenda
tools:
  - id: list
    class: read
    scopes: [calendar.read]
    secrets: {}
    egress: []
    input: {type: object, properties: {}, additionalProperties: false}
  - id: delete
    class: irreversible
    scopes: [calendar.delete]
    secrets: {}
    egress: []
    input:
      type: object
      required: [event]
      properties: {event: {type: string}}
      additionalProperties: false
```

- **Optional tool fields:** `name` (server-side MCP tool name if different from kebab-case `id`, `[A-Za-z0-9_.-]+` <= 128 chars), `external` (boolean, default `false`; set `true` whenever input is sent to third parties, including search), `effects` (boolean, default `false`; enables multi-step write callbacks), `timeout_secs` (integer 1..=900, default 60, capped by `limits.tool_call_max_secs`), `env` (keys from inherited `env.yaml`), `destination` (`"reply"` for original conversation or `{recipient: "args.<path>", content: "args.<path>"}` for arbitrary recipients—always elevated to `irreversible`), `secrets` (`{ENV_VAR: profile-secret-name}`), `egress` (`["host:port"]`, intersected with host `egress.<profile>` cap).
- **Schema locking:** Omitting `input` loads the schema and description from sibling `tools.lock.yaml` generated by `bot mcp sync <skill-dir>` ([plugins.md](plugins.md#tool-definition-locking-toolslockyaml)). Inline `input` takes precedence; omitting both fails validation (`missing-input`). Remote HTTP servers (`transport: "http"`) forbid `effects`, `secrets`, and `env` and require the exact `url` `host:port` in `egress`.

### `triggers.yaml`

```yaml
version: 1
triggers:
  - id: morning
    "on": {cron: "* * * * *"}
    principal: "service:morning"
    target: {tool: agenda.list, args: {}}
```

`on` is one of: 5-field `cron` (or 7 `@` aliases), `{event, binding}` (`message`, `thread-reply`, `reaction`), or `{http: {method, path}, binding}`. Cron triggers require a `service:*` `principal`, support `beat: true` (coalesces duplicate health pings), and require a human `approver` if targeting an approval-gated tool (unless `preapproved`). Event and HTTP triggers use the binding's authenticated principal. `target` is either `{tool, args?}` or `{instruction}` sent to the owning agent.

## OpenAI Model Bindings

Expose an agent as a named OpenAI-compatible model by adding an HTTP binding with `model` to `profile.yaml`:

```json
{"id":"assistant","adapter":"http","endpoint":"openai","model":"assistant","agent":"main"}
```

Register the endpoint under `gateway.http.endpoints.openai` in `host.json` (`automation-api-key` is a secret name in the profile's secret store; register `http:openai/build` in `members` with `run`):

```json
{"kind":"openai","keys":{"automation":{"secret":"automation-api-key","subject":"build"}},"wait_ms":120000,"max_active_runs":32}
```

- Multiple models may share one endpoint; each `model` is a unique lowercase slug (<= 63 chars), and one endpoint belongs to one active profile. Removing a key's subject from `members` returns `403`. Triggers referencing an OpenAI binding fail validation. Changes to key slots, subjects, approvers, or agent mappings require explicit `--activate`; rotating only a key's secret value preserves the revision.
- Approver mappings may be declared before issuing keys (`bot validate` warns `approver-key-unknown` but allows activation). Removing a member's last key slot causes old keys to receive `401` after activation while leaving other slots intact.
- To route approvals for an HTTP member to a human profile member, add `approvers` to the binding (`{"principal":"local:owner"}` for local, or a same-workspace Slack binding as below):

```json
{
  "id": "assistant",
  "adapter": "http",
  "endpoint": "openai",
  "model": "assistant",
  "agent": "main",
  "approvers": {
    "http:openai/build": {"principal": "slack:home/U123", "binding": "team"}
  }
}
```

- Webhooks use `kind: "webhook"` (default for legacy JWT endpoints) and omit `model` on bindings. One alias cannot mix webhook and OpenAI modes; rename legacy webhook aliases conflicting with `/v1` before activating together. See [gateway.md](gateway.md#openai-compatible-api) for SDK usage, errors, and polling notices (`"Awaiting approval. Query again with the same Idempotency-Key and request."` / `"Request is still in progress. Query again with the same Idempotency-Key and request."`).

## Direct Event-to-Tool Routing

After authenticating an entrance request, the gateway builds a verified envelope exposing `request_id`, `binding`, `principal`, `conversation` (kind, ID, thread), `attachments` (permission-scoped references), `body`, and (for HTTP) `method`/`path`.

```yaml
version: 1
triggers:
  - id: cancel
    "on": {http: {method: POST, path: /agenda}, binding: inbox}
    order: 10
    match: {body.action: cancel}
    input: {event: body.event}
    target: {tool: agenda.delete}
    response: {mode: accepted}
```

- **Matching & mapping:** Candidates for the same binding and event (or HTTP `method`/`path`) are evaluated in ascending `order` (default `0`; duplicate `order` in a group is a validation error). `match` checks literal equality on dot-paths (omitted matches all). `input` maps tool argument names to envelope dot-paths without overlapping static `args` (no expressions, shell, or templates).
- **Private DM triggers:** On a DM binding (`{id: direct, adapter: slack, workspace: home, dm: true, agent: main}`), a direct tool trigger such as `{id: secure, on: {event: message, binding: direct}, private: true, target: {tool: inbox.receive}, input: {text: body.text}}` routes body text solely to that tool without writing raw body text to the general queue, audit log, or model context. `private: true` is allowed only on DM direct-tool triggers, must precede general handlers, and never falls back to chat on tool failure.
- **HTTP response modes:** Default `response: {mode: accepted}` commits and returns `202` + `request_id`. `{mode: sync}` returns the tool's `{status, body}` directly (falling back to `202` on timeout). `GET /{endpoint}/_requests/{id}` by the same principal honors the response mode recorded at intake even after profile updates.

### Selecting Slack Channel Events

Channel bindings accept `events` (default `[mention]`; non-empty, unique; changes update the activation revision):

```yaml
bindings:
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main,
     events: [mention, thread_reply, reaction]}
```

| Binding `events` value | Trigger `on.event` | Received event | `body` fields |
|---|---|---|---|
| `mention` | `message` | Bot mention in the registered channel | `text` |
| `thread_reply` | `thread-reply` | Human reply in a thread with verified bot participation | `text`, `channel`, `ts`, `thread_ts` |
| `reaction` | `reaction` | Human emoji reaction on a channel message | `reaction`, `channel`, `item_ts`, `item_user` |

DM bindings allow only `events: [dm]` (same as omitted); CLI and HTTP bindings forbid `events`. Referencing a disabled event fails with `event-not-enabled`; attaching `thread-reply` or `reaction` to a non-channel binding fails with `binding-kind-mismatch`. Events are accepted only when the actor is a profile member ([gateway.md](gateway.md)).

```yaml
triggers:
  - id: ack
    "on": {event: reaction, binding: team}
    match: {body.reaction: white_check_mark}
    target: {tool: reports.ack}
    input: {item: body.item_ts}
```

### Passing Unhandled Events

When multiple direct-tool triggers match a Slack event, intake pins them in ascending `order` as candidates. The first matching instruction trigger becomes the fallback (later triggers are unreachable); otherwise the fallback is the binding's agent (requiring `run` at intake and handoff). If a candidate tool returns `_meta["bot/pass"]: true` ([plugins.md](plugins.md#passing-unhandled-events-botpass)), execution advances to the next candidate and finally to fallback chat, rechecking current permissions and active revision at every step. Normal results (including errors) stop the chain. Audit records only trigger IDs and positions (`trigger.select`, `call.pass`, `trigger.next`). Declaring `private_pass: true` alongside `private: true` on a DM trigger promotes the original body and attachments to normal DM chat upon a valid empty PASS.

## Pre-Approved Static Calls and Designated Human Approval

### Pre-Approved Static Cron Calls (`preapproved`)

Operators can pre-approve bounded, recurring static tool calls at activation time—strictly for non-beat `cron` triggers with static `{tool, args}`:

```yaml
- id: notify
  "on": {cron: "0 9 * * *"}
  principal: "service:reporter"
  target: {tool: reports.notify, args: {report: daily}}
  preapproved:
    max_per_day: 24
    targets:
      - {operation: post, target: "channel:reports"}
```

- **Bounds:** `max_per_day` is `1..=65535`; `targets` has `1..=64` unique `{operation, target}` pairs (exact IDs or secret names; no wildcards, prefixes, URLs, whitespace, `..`, or secret values).
- **Eligibility:** Forbidden with `input`, `instruction`, `private`, `beat`, `human_approval`, or event/HTTP triggers (`invalid_preapproval`). Target tool must declare `effects: true` (`preapproval_unenforceable`). Allows omitting `approver`; membership, scopes, policy, secrets, install, and egress checks still apply.
- **Grant lifecycle:** Grants are issued, rotated, or revoked only on explicit `bot serve --activate` (missing grants close calls as `preapproval_stale`). Each grant binds the fingerprint of profile/agent/trigger, source/principal/scopes, tool ID, install revision, JCS SHA-256 of static `args`, sorted allowed pairs, `max_per_day`, and target mappings (declared secret names, `env`, `egress`, `destination`). Changing any input invalidates the old grant immediately; rolling back issues a new grant ID rather than reviving revoked IDs. **Limitation:** Rotating the value of the same secret name does not change the fingerprint—rename the secret to force re-approval.
- **Execution & pause:** Non-inheritable (never passed to models, children, wakes, user schedules, or other triggers). Every write uses `effect.request{step,args:{operation,target,params}}` matching an allowed `(operation, target)` and consumes 1 UTC daily quota per new `send:true` (reads/replays = 0; retry after `not_executed` = 1; rejections return `denied`/`stale`/`invalid`/`conflict`). Operators pause/resume triggers via the Admin API/UI ([admin.md](admin.md)); pauses persist across restarts and skip occurrences scheduled before the last pause/resume transition.

### Designated Human Approval (`human_approval`)

On direct-tool Slack event triggers (`message`, `thread-reply`, `reaction`), a designated human's matching message or reaction can satisfy one approval step:

```yaml
- id: confirm
  "on": {binding: team, event: reaction}
  match: {body.reaction: white_check_mark}
  input: {item: body.item_ts}
  target: {tool: reports.notify}
  human_approval:
    approvers: ["slack:work/U123"]
```

- Requires an exact non-empty confirmation literal in `match` (`body.text` for `message`/`thread-reply`, `body.reaction` for `reaction`; else `invalid_human_approval`).
- `approvers` must be 1..=16 unique Slack human profile members in the binding's workspace (`invalid_human_approval` otherwise).
- Evidence binds the authenticated actor of that exact event to the first candidate's trigger/binding/event ID at intake and is consumed once at call start (recording evidence ID, event, principal, tool, and args digest). Both current and pinned revisions must still list the actor (`human_approval_mismatch` falls back to normal approval cards). Never inherited across models, delegation, PASS, private triggers, or owner-only approvals.

## Validation, Revision Restore, and Permission Rules

Both `bot validate` and `bot serve` share the same validator against `schemas/v1/{profile,agent,skill,tools,triggers,env}.schema.json`. Validated bytes build a new revision and swap all bindings atomically; failures keep the previous revision. Runs pin instructions and tool declarations at intake and intersect with current permissions before every call, approval, or delivery. Revoked servers or bindings cannot resume.

### Revision Restore Contract After Redeploy

Before opening any role (initial publish, joining the active set, or `--activate`), processes store validated bundles in PostgreSQL keyed by `(profile_id, revision SHA-256)` and restore every required historical revision via `Registry::adopt` (or abort startup without mutating active state, cards, or attempts; see [reliability.md](reliability.md)).

- **Restore scope:** Computed over the target active set's profile IDs: each published revision + pinned revisions of unfinished runs, resumable dead letters, and pending/unconsumed cards + open or unexhausted static cron windows + creator revisions of active user schedules. The publish transaction locks the active set and re-checks required revisions; stale workers receive `Stale` once publication is visible. Profiles removed from the active set are not restored and cannot execute under old permissions. Missing or corrupt bundles abort startup with the profile ID and full 64-char SHA-256.
- **Backfilling legacy DBs:** Run `bot profile-bundle put <profile-dir>` (requires `BOT_DATABASE_URL`, no host config or secrets). It validates the directory, computes canonical `BOTPROFILE1`/`BOTPROFILE2` bundle SHA-256 hashes, stores any matching pinned revisions idempotently, fails on byte mismatch without overwriting, and does not change active state.

| Subject | Re-evaluation rule |
|---|---|
| Accepted / resumed run | Uses intake-pinned instructions and tool declarations intersected with current permissions. Absent from pinned revision -> `unknown_tool`; revoked now -> `denied` (0 executions). |
| Static cron | Uses the revision and target of the window containing the scheduled time (including missed ticks after restart). |
| User schedule / wake | Checks current membership, binding, delivery path, and creator-time scope cap at firing; pins the firing-time revision. |
| Pending approval | Re-checks current permissions, approver, risk, policy, tool, and install revision; revoked tools resolve as `stale`. |

- **Catalog & bundle format:** `bot_core::profile::ProfileCatalog` serves `active`, `current`, and `lookup`. `ProfileSource::validate` returns the set revision; `Accept.revision` records the profile revision. Bundles with `SKILL.md` front matter use header `BOTPROFILE2`; profiles without it retain `BOTPROFILE1` for stable hashes and legacy restore compatibility.
- **Principal identities & dev secrets:** Allowed formats are `local:<account>`, `slack:<workspace-alias>/<U|W...>` (Slack user ID <= 32 chars), `http:<endpoint-alias>/<sub>`, and cron-only `service:<name>`. Only `local:*` and `slack:*` can be human approvers. The `env` secret store maps `(personal, calendar-reader)` to `BOT_SECRET__personal__calendar_reader` (lowercase, hyphens to single underscores, double underscore as boundary; values never appear in diagnostics).

### `bot-profile` Validation Rules

- **Filesystem & limits:** Only profile folders directly under `BOT_PROFILES` (root itself may be a symlink). Rejects symlinks, hardlinks, sockets, FIFOs, devices, non-UTF-8, control/bidi characters, and case-only name collisions. Allowed names: profile (`profile.yaml`, `rules.md`, `agents`, `knowledge`, `skills`), agent (`AGENT.md`, `env.yaml`, `triggers.yaml`, `memory`, `knowledge`, `skills`, `agents`), skill (`SKILL.md`, `tools.yaml`, `tools.lock.yaml`), `knowledge/` (`.md`, `.txt`), `memory/` (`.md`). Ignores `.git`, `.DS_Store`, `.gitkeep`. Limits: 64 profiles, 2,000 files & 16 MiB per profile, 1 MiB per file (256 KiB YAML), depth 24, 128 agents, 8 agent levels, 64 KiB visible skill bodies per agent.
- **YAML & cron:** YAML 1.2 core schema (`yes/no/on/off` are strings); rejects anchors, aliases, tags, multi-doc, duplicate/complex keys, `Inf`/`NaN`, nesting > 32, or > 20,000 entries. `bot_core::cron::Cron` accepts 5-field cron, `@hourly`/`@daily`/`@midnight`/`@weekly`/`@monthly`/`@yearly`/`@annually`, and `?` in day-of-month/week. Tool `input` uses the restricted JSON Schema vocabulary and portable Rust regex `pattern` rules in [plugins.md](plugins.md).

| `SKILL.md` error code | Cause |
|---|---|
| `front-matter-unclosed` | Opening `---` has no closing `---` line (including standalone `---` at EOF). |
| `skill-allowed-tools` | Front matter contains `allowed-tools` (permissions belong in `tools.yaml` and `profile.yaml`). |
| `skill-name-mismatch` | Front matter `name` differs from the skill folder name. |
| `skill-too-large` | Single `SKILL.md` body or per-agent visible skill body sum exceeds 64 KiB. |
| `empty-file` | Body after stripping front matter is empty. |
| `schema` | Unknown key or invalid `name`/`description`/`license`/`compatibility`/`metadata` shape. |

#### JSON Schema vs. `bot validate`

Bundled schemas (`schemas/v1/*.schema.json`) catch single-file structure, closed objects, ranges, and formats; `bot validate` and `bot serve` enforce cross-file references, semantic rules, and host registry checks:

| Target | Checked by JSON Schema | Checked only by `bot validate` |
|---|---|---|
| `triggers.yaml` `preapproved` | Closed `{max_per_day, targets}`, `1..=65535`, `1..=64` unique `{operation, target}` (1..=128 chars, no wildcards/URLs/spaces/`..`). | Non-beat `cron` + `target.tool` only; forbids `input`/`private`/`human_approval`; target tool requires `effects: true` (`preapproval_unenforceable`). |
| `triggers.yaml` `human_approval` | Closed `{approvers}`, `1..=16` unique `slack:<ws>/<U\|W...>` (<= 102 chars). | `on.event` + `target.tool` only; exact `match` (`body.text` or `body.reaction`); forbids `private`; workspace match & member check. |
| `triggers.yaml` `on.cron` | 5 whitespace-separated fields or 7 `@` aliases. | Field ranges (0-59, 0-23, 1-31, 1-12, 0-7), step `/n`, start<=end, `?` only in DOM/DOW, <= 256 bytes (`bad-cron`). |
| `triggers.yaml` other | Required keys, `on`/`target` unions, basic `cron`/`response`/`private` placement. | `private_pass` requires `private: true`, `http.path` normalization (`..`/`//`/spaces/256B), event names, envelope roots, `sync`+tool, ID/`order` uniqueness, binding/tool/principal/`args`/`input` checks. |
| `profile`/`tools`/`env`/`AGENT`/`SKILL` | Closed objects, version, naming/scope/schema syntax, `SKILL.md` front matter rules. | `id`/skill `name` == folder, exact-case IANA `timezone`, role/scope/secret/binding/agent/tool refs, reserved tool IDs & env vars, `skill-allowed-tools`, 64 KiB skill sum, `tools.lock.yaml` hash, host registry checks. |

- **Reserved built-in tool IDs:** Profile `tools.yaml` may not declare any of the 12 reserved framework IDs: `delegate`, `memory.share`, `schedule.create`, `schedule.list`, `schedule.cancel`, `schedule.deliver`, `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove` (other tools in those skill namespaces are allowed).
- **References, `env`, and permissions:** Rejects `id != folder`, non-exact IANA `timezone`, unknown roles/scopes, duplicate principals/binding IDs/entrances, missing `env.yaml` keys (`env-missing`), reserved loader/shell/proxy/host env names (`env-reserved`), env/secret name clashes (`env-secret-conflict`), non-service cron principals, missing human `approver` on gated cron tools, `private` triggers placed after general handlers or calling gated/`effects: true` tools, `input`/`args` clashes, non-tool `sync` responses, and `timeout_secs` exceeding host `limits.tool_call_max_secs` (`tool-timeout-over-cap`).
- **Permission formula & hashes:**
  - Member scopes = `(union of role scopes) ∩ policy.scopes`; run scopes = `member scopes ∩ agent cap` (parent cap ∩ `policy.agents`).
  - Approval is required for `irreversible` tools and `write` tools whose scope is in `policy.approve`. Tools with `destination: {recipient, content}` are elevated to `irreversible` in the snapshot (`destination: "reply"` keeps its declared class).
  - Memory seed body limit is 8,192 UTF-8 bytes; content hash is `SHA-256` over `u64` big-endian length-prefixed `(owner, visibility, body)`.
  - Tool `env: [KEY]` keys fold into the tool revision; at runtime the broker verifies that declared names and values match between the pinned and current revisions.

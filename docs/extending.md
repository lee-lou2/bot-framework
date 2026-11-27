# Extending the Framework

English | [한국어](ko/extending.md)

Where to edit depends on the extension type. Each section separates **compiler-guided** sites (`cargo check` catches them) from **silent** sites (strings, schemas, SQL constraints, docs — compiles even if missed). Check silent sites first, and verify completeness with `scripts/parity_audit.py` rather than relying on file counts. See [Glossary](glossary.md) for terminology.

| Extension | Rust changes | Section |
|---|---|---|
| General tool (MCP server) | None — config only | [General tools](#general-tools) |
| OpenAI-compatible server (Ollama, vLLM, Gemini-compatible) | None — config only | [Model providers](#model-providers) |
| New model API wire protocol | Runtime wire module + `bot-cli` provider file | [Model providers](#model-providers) |
| Chat channel | Multiple crates | [Chat channels](#chat-channels) |
| `SecretStore` backend | `bot-cli` only | [SecretStore backends](#secretstore-backends) |
| Sandbox backend | Tool sandbox: `bot-broker` + `bot-cli` wiring | [Sandbox backends](#sandbox-backends) |
| Trigger type | Profile, gateway, admin | [Trigger types](#trigger-types) |
| Built-in tool | Central table (`bot-core`) + per-crate handlers | [Built-in tools](#built-in-tools) |

## General Tools

Zero Rust changes. Build an MCP server and update four configuration sites (`bot validate` reports mismatches by name):

1. Install the server and register it under `servers` in `host.json` ([Plugins](plugins.md); parsed in `crates/bot-cli/src/install.rs`).
2. Add `tools.yaml` (`schemas/v1/tools.schema.json`) in the skill directory and generate `tools.lock.yaml` with `bot mcp sync`.
3. Grant `roles`, `policy.scopes`, and (if approval-gated) `policy.approve` in `profile.yaml` ([Profile spec](profile-spec.md)).
4. Wire agent `env.yaml` and `secrets`.

## Model Providers

- **OpenAI-compatible servers:** zero Rust changes — define `providers.<alias>` with `kind: "openai-api"` in `host.json` ([Architecture](architecture.md)).
- **New API wire protocol** (e.g., native Gemini):
  - **Compiler:** `crates/bot-runtime/src/model/api/wire.rs` (all `ApiKind` dispatch) and `crates/bot-cli/src/execution/provider.rs` (kind label, activation fingerprint tag byte, `parse_api` keys).
  - **New file:** request/response codec under `crates/bot-runtime/src/model/api/wire/`.
  - **Silent:** `kind` string `match` in `parse_providers` (`provider.rs`) and `docs/architecture.md`.
  - **Trap:** assign a **new** fingerprint tag byte and never change existing bytes (doing so forces re-activation across all deployments). Pin it with a fixed-input test in `crates/bot-cli/src/execution/fingerprint_tests.rs`.
- **New VM-hosted CLI provider** (new `CliKind` variant in `bot-runtime` + `Provider::Cli`):
  - **Compiler:** `CliKind` in `crates/bot-runtime/src/model/cli.rs`, `Protocol` in `crates/bot-broker/src/model.rs`, `impl Provider` in `crates/bot-cli/src/execution/provider.rs` (fingerprint tag byte), `Apple::image_for` in `crates/bot-cli/src/execution.rs`, `crates/bot-cli/src/serve/claude.rs`, `crates/bot-cli/src/serve/models.rs`, and `crates/bot-cli/src/validate.rs`.
  - **Broker protocol:** if the CLI uses a new wire protocol beyond `Anthropic` (`/v1/messages`), `OpenAiResponses` (`/v1/responses`), and `Gemini` (`/v1beta/models/...`), add a request vetter under `crates/bot-broker/src/model/vet/`, SSE termination + error formatting in `crates/bot-broker/src/model/stream.rs`, a strict observer under `crates/bot-broker/src/front/observe/`, and session-pinned route/MCP call-ID handling in `crates/bot-broker/src/front.rs` and `crates/bot-broker/src/front/mcp.rs`.
  - **Runtime & VM:** implement `ModelRunner` under `crates/bot-runtime/src/model/` (reusing `model/cli.rs` and `model/driver.rs`), allow the binary name in `FIREWALLED_PROGRAMS` (`crates/bot-runtime/src/model/container/args.rs`) and `images/model/bot-init`, add its Containerfile and `build.sh` target, and pin its new fingerprint tag in `crates/bot-cli/src/execution/fingerprint_tests.rs`.
  - **Silent:** `parse_providers` (`provider.rs`), `parse_sandbox` `images` keys (`execution.rs`), `docs/architecture.md`, `docs/model-image.md`, and synced `docs/ko/*.md`.

## Chat Channels

### Adding an `Adapter` Variant

- **Compiler:**
  - `crates/bot-profile/src/model.rs` (`entrance`, `adapter_name`) and `crates/bot-profile/src/activate.rs` (host alias matching).
  - `crates/bot-gateway/src/entrance.rs` (`Entrance`, `Real`, `Conflict::fix`).
  - `crates/bot-gateway/src/routes.rs` (`reply_transport` → `PushTransportId`) and `crates/bot-gateway/src/schedule_routes.rs` (`slack_destination` explicit-destination dispatch).
  - `crates/bot-admin/src/map.rs`.
  - `crates/bot-cli/src/validate.rs` (`entrance_secrets`) and `crates/bot-cli/src/serve/gateway.rs` (listener startup).
- **Silent:**
  - `schemas/v1/profile.schema.json` `bindings` `oneOf`; `adapter` `match` in `crates/bot-profile/src/profile.rs` (omitting returns `unknown-adapter`, never falls back to webhook).
  - Host settings: `crates/bot-gateway/src/host.rs` (struct + validation), `crates/bot-gateway/src/mapping.rs` (activation fingerprint — configs without the new section must keep identical bytes), `HostCatalog` in `crates/bot-profile/src/host.rs`, and `catalog()` in `crates/bot-cli/src/host.rs`.
  - Adapter ingress module (receive → `Inbound` → `Ingress`) and reply `Sender` (`crates/bot-gateway/src/delivery.rs`, where `Sender::scope()` returns a `DeliveryScope` carrying a fixed `PushTransportId`), wired in `crates/bot-cli/src/serve/gateway.rs`.
  - Migration if channel-specific tables are needed (see event deduplication in `crates/bot-store/migrations/0022_slack_events.sql`).
  - Docs: [Gateway operations](gateway.md) host config table and [Profile spec](profile-spec.md) bindings.

### Related Types and Outbox Routing Contract

- **New `ConversationKind`:** compiler flags `crates/bot-store/src/accept.rs`, `crates/bot-store/src/codec.rs`, `crates/bot-store/src/delivery.rs`, `crates/bot-store/src/user_schedule.rs`, `crates/bot-runtime/src/worker/span.rs`, `crates/bot-gateway/src/inbound.rs`, `crates/bot-gateway/src/schedule_routes.rs`, `crates/bot-broker/src/runner.rs`, and `crates/bot-admin/src/json.rs`. **Silent:** widen the `conv_kind` `CHECK` constraint from `crates/bot-store/migrations/0001_runs.sql` via a new migration.
- **New `IdentityKind`:** compiler flags **nothing** (compared with `==`/`matches!`, and `RunOrigin::from_ingress` ends in `_ =>`). Audit manually: `crates/bot-core/src/identity.rs`, `crates/bot-core/src/state.rs`, `crates/bot-profile/src/approval.rs`, and `crates/bot-store/src/api.rs` (`grep -rn IdentityKind crates/*/src`).
- **Reply outbox routing contract** (required for every push adapter):
  1. **Stable `PushTransportId`:** `reply_transport` (`crates/bot-gateway/src/routes.rs`, type in `crates/bot-core/src/intake.rs`) writes `bot_runs.reply_transport` at intake; redeliveries, delegations, and origin-bound schedule firings inherit it.
  2. **Claim isolation (`Sender::scope`):** `Outbox::claim` (`crates/bot-store/src/outbox.rs`) filters in PostgreSQL by `bot_outbox.transport` and optional `(profile, binding)` rather than conversation kind, so two push adapters sharing `Direct`/`Channel` never claim or reject each other's replies.
  3. **Sequence ordering:** earlier `pending`/`sending` rows on the same `run` block later sequences regardless of `transport`.
  4. **Explicit-destination (`ScheduleRoutes`) limit:** `crates/bot-gateway/src/schedule_routes.rs`, channel owner approval DM routing (`crates/bot-store/src/approval_open.rs`), and destination enqueue (`push_to` in `crates/bot-store/src/delivery.rs`) are currently pinned to `PushTransportId::SLACK`. Opening explicit destinations to another push adapter requires storing destination transport on `bot_user_schedules` and `bot_deliveries` and lifting the Slack pin in all three places.
- **Parity audit:** run `python3 scripts/parity_audit.py slack <new-name> --skip crates/bot-gateway/src/slack/` to list files mentioning `slack` but not the new name; cross off Slack-only features (`--strict` fails if any remain). Channels are statically typed rather than dynamically registered because schemas, SQL constraints, and fingerprints are static — dynamic registration would keep most of this checklist while forfeiting compiler checks.

## SecretStore Backends

- **Implementation:** add a `SecretStore` module under `crates/bot-cli/src/secrets/` and export/describe it in `crates/bot-cli/src/secrets/mod.rs`.
- **Compiler:** `impl SecretsKind` in `crates/bot-cli/src/secrets/mod.rs` (protected paths, missing-secret hints, decryption flag) and `secret_store` in `crates/bot-cli/src/load.rs`.
- **Silent:** `parse_secrets` (`BOT_SECRETS`) in `crates/bot-cli/src/settings.rs` and [Secret stores and commands](gateway.md#secret-stores-and-commands).
- **Automatic isolation:** files returned by `protected_paths()` automatically enter the tool sandbox `deny_read` list (`denied` in `crates/bot-cli/src/serve/runner.rs`).

## Sandbox Backends

- **Tool servers:** implement `bot_broker::sandbox::Sandbox` (`crates/bot-broker/src/sandbox.rs`) under `crates/bot-broker/` (cf. `srt.rs`) and expose `pub mod` in `crates/bot-broker/src/lib.rs`.
  - **Silent:** `local_sandbox` (`crates/bot-cli/src/serve/runner.rs`) constructs `SrtSandbox`; `Sandbox` and `parse_sandbox` (`crates/bot-cli/src/execution.rs`) hold only `srt` and `apple`; startup preflight `refuse_required_state` calls `srt::supports_state` (matching per-call `admit`) and must dispatch to the selected backend; also update `crates/bot-cli/src/doctor/host.rs`, [Architecture](architecture.md), [Security](security.md), and the CI `tool-sandbox` job (`.github/workflows/ci.yml`).
  - No host-config selector table exists yet because only one backend is implemented; switch `local_sandbox` to `Arc<dyn Sandbox>` and add a selector once a second backend is chosen.
- **Model VMs:** `bot_core::sandbox::Sandbox<ModelSpec>` has one implementation (`ContainerSandbox` in `crates/bot-runtime/src/model/container.rs`). Wiring (`crates/bot-cli/src/serve/claude.rs`) depends on that concrete type and the anchor-verified gateway IPv4 relay contract ([Architecture](architecture.md)); adding another VM backend starts by lifting that contract into a port trait.

## Trigger Types

- **Compiler (`TriggerSource` variant):** `crates/bot-admin/src/map.rs` and `crates/bot-gateway/src/routes.rs` (two sites).
- **Silent:**
  - `schemas/v1/triggers.schema.json`.
  - Parsing and conversion in `crates/bot-profile/src/{triggers,model,resolve,convert}.rs` plus the definition in `crates/bot-core/src/profile.rs`.
  - **Producer:** component emitting and admitting the event (`crates/bot-runtime/src/scheduler.rs` for `cron`; `crates/bot-gateway/src/ingress.rs` for entrance events). Adding a variant without a producer does nothing.
  - Docs: [Profile spec](profile-spec.md).

## Built-in Tools

Built-in tools couple directly to approvals and PostgreSQL transactions and are intentionally kept small. IDs, descriptions, schemas, `reserved`, and `card_body` live in one table (`crates/bot-core/src/builtin.rs`).

- **Central table & classification:** `crates/bot-core/src/builtin.rs` (`model_tools`, `model_tools_for`, `reserved`, `card_body`, `owner_only_card`, `memory_tool`, `manage_tool`, `delegate_schema`). Profile validation (`crates/bot-profile/src/tools.rs`) and scaffolding (`crates/bot-cli/src/scaffold/tool.rs`) consume `builtin::reserved` directly.
- **Model exposure & handlers:** API runner `crates/bot-runtime/src/context.rs` and `crates/bot-runtime/src/worker/calls.rs`, VM relay `crates/bot-broker/src/front/{mcp,delegate}.rs`, and broker dispatch/services `crates/bot-broker/src/service/{call,preflight,share,memory,schedule,deliver,manage}.rs`.
- **Store, live management & approval cards:** `crates/bot-store/src/{share,memory_ops,memory_search,memory_auto,user_schedule,delivery,admin_approvals}.rs`, `crates/bot-cli/src/serve/manage/`, and `crates/bot-admin/src/json.rs` (store and admin share `builtin::owner_only_card` and `builtin::card_body` to protect owner-only card bodies).
- **Wiring & docs:** `crates/bot-cli/src/serve/{tools,decide}.rs`, `docs/architecture.md`, `docs/plugins.md`, and `docs/profile-spec.md` (`.github/scripts/docs_contract.py` verifies every reserved ID in `crates/bot-core/src/builtin.rs` is documented).

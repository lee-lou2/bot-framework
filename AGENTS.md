# AGENTS.md

Rust workspace that runs bots from declarative profile directories (agents, skills, tools, approval rules). See `docs/why.md` for rationale, `docs/architecture.md` for runtime flow, `docs/glossary.md` for terms, `docs/extending.md` for extension points, and `docs/gateway.md` for operations.

## Non-Negotiable Invariants

1. **Models cannot reach the host or see service keys.** Both execution paths enforce this (`docs/architecture.md`):
   - `claude-cli`, `codex-cli`, `agy-cli`: run the model CLI inside a disposable VM with no secrets (session token only) and one egress path through the broker relay.
   - `anthropic-api` / `openai-api`: no VM or relay; the worker sends conversation messages to the configured host `endpoint` and receives text/tool calls only. No host filesystem or shell exists, and the worker reads the provider key from `SecretStore` solely to attach the request header.
2. **All tool authorization, approval, and auditing pass through the broker (`crates/bot-broker/src/service.rs`).** Never add a direct model-to-tool execution path. Both paths route tool calls through `Broker::call` (`Broker::call_for_model` for model-planned calls).
3. **Durable intake first.** A request is accepted only after the PostgreSQL transaction commits.
4. **Humans approve irreversible actions.** Models can never approve actions.

## Conventions

- **Language:** Write all code, comments, identifiers, user-facing copy (CLI, Admin UI, Slack cards, error messages), docs, and conventional commit messages (title and body) in **English**. Error reasons and audit outcomes use fixed vocabulary codes—never free-form sentences, payloads, or secrets.
- **Minimal essential comments:** Keep comments minimal and high-signal. Every non-test `.rs` file under `crates/*/src/` (excluding `tests.rs`, `*_tests.rs`, `tests/`, `testing/`) must have a concise 1-line English `//!` responsibility header on line 1 (`.github/scripts/check_contracts.py`). Elsewhere, comment only non-obvious invariants, lock ordering, security boundaries, protocol quirks, or non-obvious "why"—never narrate obvious code or add redundant doc comments on self-explanatory items.
- **Concise documentation & Korean human editions:** Keep `README.md` and `docs/*.md` concise and focused on core intent and contracts. Update `docs/` in the **same change** when adding or changing config keys, schema properties, CLI flags, admin routes, or built-in tools (`.github/scripts/docs_contract.py`). Keep Korean companion editions (`README.ko.md`, `docs/ko/*.md`) in sync when human-readable docs change. Use tables or lists when a contract paragraph has 3+ numbers or 2+ rules.
- **File size & boundaries:** Keep files at most 600 lines; split along conceptual boundaries rather than arbitrary line counts.
- **Dependencies & visibility:** Add dependencies only with strong justification (`=` pinned, `deny.toml`). Never commit private names or secrets. Do not add cross-dependencies between the 6 middle crates; start new modules as `pub(crate)` and open `pub mod` only when another crate needs it.
- **Test seams:** Place cross-crate test doubles behind the `testing` feature, and crate-local ones under `cfg(test)`. `serve::Overrides` and `serve::gateway::Deps.slack_base` (`crates/bot-cli/src/serve/`) are test-only seams.
- **Fingerprint stability:** Activation fingerprint bytes must remain stable; changing them forces re-activation on all existing deployments. Add fixed-input golden tests before touching fingerprint inputs.

## Commands

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked            # PG tests need BOT_TEST_DATABASE_URL (CI sets BOT_TEST_REQUIRE_DB=1)
    python3 .github/scripts/check_contracts.py
    scripts/release-check.sh [--strict]        # Runs all gates (docs/release.md)
    cargo test -p bot-broker                   # Single crate (real sandbox tests run in CI tool-sandbox)

## Crates & Dependency Direction

`bot-core` ◄ {`bot-profile`, `bot-store`, `bot-runtime`, `bot-broker`, `bot-gateway`, `bot-admin`} ◄ `bot-cli`. The six middle crates never depend on one another and communicate only through `bot-core` port traits.

| Crate | Responsibility | Entry Points |
|---|---|---|
| `bot-core` | Cross-role types, port traits, and bit-exact algorithms (cron, canonical JSON, hashing) | `crates/bot-core/src/{lib,port,command,state}.rs` |
| `bot-profile` | Profile directory loading into validated snapshots and revisions | `crates/bot-profile/src/{lib,registry}.rs` |
| `bot-store` | PostgreSQL queue, call ledger, outbox, audit chain (public surface is `Store` via ports) | `crates/bot-store/src/{lib,ports}.rs`, `crates/bot-store/migrations/` |
| `bot-runtime` | Worker loop, scheduler, delegation, model runners | `crates/bot-runtime/src/{worker/mod,model/mod,context}.rs` |
| `bot-broker` | Tool gate, model proxy, sandboxed MCP runner (`srt`) | `crates/bot-broker/src/{lib,service,runner,srt}.rs` |
| `bot-gateway` | CLI, HTTP (OpenAI-compatible), and Slack ingress/delivery | `crates/bot-gateway/src/{ingress,delivery,slack/mod,http/mod}.rs` |
| `bot-admin` | Admin API/UI (`ui/`) and OTLP trace/log + Sentry issue exporters (`obs/`, `trace/`, `logs/`, `issues/`) | `crates/bot-admin/src/{lib,api,trace/mod,logs/mod,issues/mod}.rs` |
| `bot-cli` | `bot` binary, host config, `SecretStore` implementations, role wiring | `crates/bot-cli/src/{main,host,serve/mod}.rs` |

## Where Things Live

| Area | Location |
|---|---|
| Durable intake & idempotency | `crates/bot-gateway/src/ingress.rs` → `crates/bot-store/src/accept.rs` |
| Queue, leases, generations, retries | `crates/bot-store/src/{queue,finish,handover,shutdown}.rs`, `RunStore` in `crates/bot-core/src/port.rs` |
| Worker loop & delegation | `crates/bot-runtime/src/worker/`, `crates/bot-store/src/delegate.rs`, `crates/bot-broker/src/front/delegate.rs` |
| Tool gate | `crates/bot-broker/src/service/call.rs`, `crates/bot-broker/src/{classify,rate,recovery}.rs`, `crates/bot-core/src/scope.rs` |
| Built-in model tools table | `crates/bot-core/src/builtin.rs`, API `crates/bot-runtime/src/{context,worker/calls}.rs`, VM relay `crates/bot-broker/src/front/{mcp,delegate}.rs` |
| Live skill/MCP management (super-admin) | `crates/bot-core/src/manage.rs`, `crates/bot-broker/src/service/manage.rs`, `crates/bot-cli/src/serve/manage/`, `crates/bot-cli/src/secrets/admin.rs`, `crates/bot-cli/src/scaffold/templates.rs` |
| Pass (`bot/pass`) | `crates/bot-core/src/pass.rs` → `crates/bot-broker/src/service/pass.rs` → `crates/bot-store/src/pass.rs` |
| Approvals (cards & decisions) | `crates/bot-broker/src/{approval,service/decide}.rs`, `crates/bot-store/src/approvals.rs`, `crates/bot-admin/ui/decide.js` |
| Operator run resume/cancel | `crates/bot-store/src/admin.rs`, `crates/bot-store/migrations/0027_run_command_receipt.sql`, `crates/bot-admin/src/{run_control,lib}.rs`, `crates/bot-admin/ui/{runs,runctl}.js` |
| Effect ledger & holds | `crates/bot-store/src/{effects,effect_lookup}.rs`, `crates/bot-broker/src/effects.rs` |
| MCP execution & sandbox | `crates/bot-broker/src/{runner,srt,sandbox,egress,proxy}.rs`, `crates/bot-broker/src/mcp/` |
| Model execution | `crates/bot-runtime/src/model/` (`api/`, `claude_code.rs`, `codex.rs`, `agy.rs`, `container.rs`, `demo.rs`), `crates/bot-broker/src/{model,front}.rs` |
| Profile loading, bundles, schemas | `crates/bot-profile/src/{lib,frontmatter,parse,snapshot,restore}.rs`, `schemas/v1/`, `examples/profiles/` |
| Scaffolding (`bot init`, `bot add`) | `crates/bot-cli/src/scaffold/{mod,add,tool}.rs` |
| Host config (`host.json`) | `crates/bot-cli/src/{host,embeddings,memory_judge,execution,execution/provider,install,tool_limits}.rs`, `crates/bot-gateway/src/host.rs`, `crates/bot-admin/src/settings.rs` |
| Activation & restore | `crates/bot-cli/src/serve/{activation,restore}.rs`, `crates/bot-store/src/{active,profile_bundles}.rs` |
| Reply outbox & transport scoping | `crates/bot-core/src/intake.rs`, `crates/bot-gateway/src/{routes,delivery,slack/send}.rs`, `crates/bot-store/src/{accept,outbox,finish,delivery}.rs`, `crates/bot-store/migrations/0026_reply_transport.sql` |
| Slack progress UX & stop | `crates/bot-gateway/src/slack/{ux,ux/cards,ux/stop}.rs` |
| Schedules | Static cron `crates/bot-runtime/src/scheduler.rs`+`crates/bot-core/src/cron.rs`, chat schedules `crates/bot-store/src/user_schedule.rs`+`crates/bot-broker/src/service/schedule.rs` |
| Memory & private DM | `crates/bot-core/src/memory.rs`, `crates/bot-store/src/{memory,memory_search,memory_ops,memory_auto,share,private}.rs`, `crates/bot-runtime/src/{embeddings.rs,memory_judge.rs,context/memory.rs}`, `crates/bot-broker/src/{private.rs,service/memory.rs}` |
| Pre-approval grants | `crates/bot-core/src/grant.rs`, `crates/bot-store/src/grants.rs`, `crates/bot-broker/src/preapproval.rs`, `crates/bot-admin/src/preapproval.rs` |
| Secrets | `SecretStore` in `crates/bot-core/src/port.rs`, `crates/bot-cli/src/secrets/`, `crates/bot-cli/src/serve/watched.rs` |
| Observability, logs, issues, audit | `crates/bot-core/src/{trace,issue}.rs`, `crates/bot-admin/src/{obs,trace,logs,issues}/`, `crates/bot-cli/src/logging.rs`, `crates/bot-store/src/audit.rs` |
| Admin API & UI | `crates/bot-admin/src/{lib,api}.rs`, `crates/bot-admin/ui/`, `docs/admin.md` |
| Extension guide | `docs/extending.md`, `scripts/parity_audit.py` |

## Code-Side Invariants

- Store rules and lock order: `crates/bot-store/src/lib.rs` header
- Broker call sequence: `crates/bot-broker/src/service.rs` header
- Gateway ingress sequence (`verify → normalize → durable accept → ack`): `crates/bot-gateway/src/lib.rs` header
- Activation fingerprint: `authority_fingerprint` in `crates/bot-cli/src/host.rs`, `crates/bot-cli/src/execution.rs`, `crates/bot-gateway/src/mapping.rs`
- Legacy restore compatibility: tightening profile validation must never break `BOTPROFILE1` (`SkillMode::Legacy`) restore; add regression cases to `crates/bot-cli/tests/legacy_v1_corpus.rs` first.
- Migration immutability: never modify bytes of deployed files in `crates/bot-store/migrations/` (checksums are verified on startup); write new migrations (0029+) in English as the next numbered file.

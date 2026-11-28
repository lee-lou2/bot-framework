# Bot Framework

English | [한국어](README.ko.md)

**A Rust framework for running safe agents, tools, and human-approved workflows from declarative profile directories — built on the premise that AI models are untrusted.**

Declare agents, skills, tools, and approval rules in a profile directory, and a single binary (`bot`) accepts CLI, Slack, Telegram, OpenAI-compatible HTTP requests, and scheduled triggers, executes them inside strict trust boundaries, and replies to the original conversation.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/diagrams/runtime-architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/diagrams/runtime-architecture-light.svg">
  <img alt="Bot Framework runtime architecture" src="docs/diagrams/runtime-architecture-light.svg" width="100%">
</picture>

## Design Philosophy

Assume the model can hallucinate or be subverted by prompt injection, and prevent damage structurally rather than through prompts. ([Why this design](docs/why.md) · [Security](docs/security.md))

- **Model isolation & zero secret exposure:** Models never touch host files, shells, or service keys. Standard chat and tool-calling runs use **direct APIs (`anthropic-api`, `openai-api`)**, where the worker attaches the provider key only to the outbound request header; CLI harnesses (`claude-cli`, `codex-cli`, `agy-cli`) run inside a **disposable VM** holding only a session token.
- **Single broker gate:** Every tool call passes through `Broker::call`, which enforces `caller scope ∩ agent cap ∩ tool scope` before running inside an `srt` sandbox or calling a registered remote MCP server.
- **DB-first durability:** A request is accepted only after its PostgreSQL transaction commits. Crashes, restarts, and approval waits never lose requests, checkpoints, effect ledgers, outbox replies, or the audit chain.
- **Human approval for irreversible actions:** External writes and deletions (effects) bind to exact arguments on an approval card and require human confirmation. Models can never approve actions.

## Runtime Architecture

Run all four roles on one host with `bot serve --role all`, or split them into separate processes via `--role`.

| Role | Crate | Responsibility |
|---|---|---|
| **Gateway** | `bot-gateway` | CLI, Slack, Telegram, and OpenAI-compatible HTTP (`/v1`) intake, auth/normalization, outbox delivery |
| **Scheduler** | `bot-runtime` | Static cron and conversational schedules (can invoke tools directly without a model) |
| **Worker & Broker** | `bot-runtime` · `bot-broker` | Queue leasing, model execution (direct API / Apple `container` VM), child delegation, tool auth/approval/MCP (`srt`) |
| **Admin & Obs** | `bot-admin` | Web UI/API for approvals, holds, and run resume/cancel; OTLP traces/logs and Sentry-compatible issues |

- **8-crate unidirectional graph:** `bot-core` ◄ {`bot-profile`, `bot-gateway`, `bot-store`, `bot-runtime`, `bot-broker`, `bot-admin`} ◄ `bot-cli`. The six middle crates never depend on one another in runtime dependencies and communicate only through `bot-core` port traits. ([Architecture & diagrams](docs/architecture.md))
- **Hybrid memory & auto-capture:** Zero-extension PostgreSQL hybrid recall (3072d IEEE-754 `f16` embeddings + Unicode lexical/trigram + z-score anti-false-taint gate + RRF), built-in `memory.search`/`save`/`forget`/`share` tools, and optional System One (`memory_judge`: `d1.2-ultra`/`jev-latest`) automatic post-turn capture for 1:1 DM/CLI conversations. ([Memory architecture](docs/architecture.md#hybrid-memory-and-automatic-capture))
- **OpenAI-compatible endpoint:** Point the official Python or Node SDK's `base_url` to `http://127.0.0.1:8788/v1` with an issued key to call `models.list()` and `chat.completions.create()`. ([SDK example](docs/gateway.md#openai-compatible-api) · [Model bindings](docs/profile-spec.md#openai-model-bindings))
- **Profile-level isolation:** Separate personal, team, or org bots into profile directories validated with `bot validate` before activation. ([Personal example](examples/profiles/personal) · [Team template](examples/profiles/team))

| Surface | Support | Notes |
|---|---|---|
| **Platforms** | Linux & macOS (Direct API + `srt` tools); macOS Apple Silicon (VM CLI harnesses) | `anthropic-api` and `openai-api` run on both Linux and macOS; `claude-cli`/`codex-cli`/`agy-cli` require Apple `container` VM |
| **Channels** | CLI socket, OpenAI-compatible HTTP (`/v1`), JWT HTTP webhooks, Slack Socket Mode, Telegram Bot API (1:1 DM) | Channel-neutral core with shrink-only drift checks; new channels implement gateway intake/delivery |
| **Authentication** | Static secrets, Remote MCP OAuth 2.0 (PKCE), experimental Google Service Account (RS256) & opt-in CLI subscriptions | Host-encrypted in PostgreSQL (`bot_credentials`); models never see keys or tokens ([Auth guide](docs/auth.md)) |

## Quick Start (Local Demo)

Walk through the full pipeline locally with no external accounts using the demo model and synthetic agenda MCP server ([demo-agenda](examples/servers/demo-agenda)).

- **Prerequisites:** `rustup` (uses the pinned Rust 1.98.1 toolchain), Node.js / npm, Python 3, PostgreSQL (Linux also needs `bubblewrap`, `socat`, `ripgrep`).

### 1. Install and Start

Run from the repository root with `bot` and `srt` on `PATH` in both terminals:

```sh
cargo install --locked --path crates/bot-cli
npm install -g --ignore-scripts @anthropic-ai/sandbox-runtime@0.0.83
createdb -h localhost bot_demo
export BOT_DATABASE_URL="postgres://localhost/bot_demo"
export BOT_PROFILES="$PWD/examples/profiles"
bot validate
bot serve --role all
```

<details>
<summary>No local PostgreSQL? Start a temporary Docker container</summary>

Starts PostgreSQL 16 bound only to `127.0.0.1` with a random password. Run `docker stop bot-demo-pg` when finished.

```sh
PG_DEMO_PASS="$(python3 -c 'import secrets; print(secrets.token_urlsafe(18))')"
docker run -d --rm --name bot-demo-pg -p 127.0.0.1:5432:5432 -e POSTGRES_DB=bot_demo -e POSTGRES_PASSWORD="$PG_DEMO_PASS" postgres:16
until docker exec bot-demo-pg pg_isready -h 127.0.0.1 -U postgres -d bot_demo >/dev/null 2>&1; do sleep 1; done
export BOT_DATABASE_URL="postgres://postgres:${PG_DEMO_PASS}@127.0.0.1:5432/bot_demo"
```

</details>

### 2. Open the Admin UI

Open the URL printed by `bot serve` (`Admin UI: http://...`, default port `8787`, configurable via `admin.bind`). Enter any username and the password printed to the terminal. When running non-interactively, set `BOT_ADMIN_TOKEN_FILE` to an absolute path with mode `0600`. ([Host configuration example](docs/profile-spec.md#host-configuration-for-a-new-profile))

### 3. Try Chat, Schedules, and Approvals

From a second terminal at the repository root:

| Flow | Action | Expected Result |
|---|---|---|
| **Chat** | `bot chat --profile personal "Show today's agenda"` | `main` invokes `agenda.list` and replies in the CLI. Use `--profile team` for the team example. |
| **Schedule** | Check the Admin UI runs list for the next minute's `morning` run | Completes `agenda.list` directly without invoking a model. |
| **Approval** | Run `bot chat --profile personal --request-id walk-1 "Delete the walk event"`, then click **Approve** in the Admin UI | Approving the exact target and arguments returns a deletion receipt. Re-check results with `--attach --request-id walk-1`. |

### 4. Create Your Own Profile

```sh
bot init my-bot --template team            # personal, team, or company template
bot add agent main/cs                      # scaffold a child agent
bot add skill triage --agent main/cs       # add a skill
bot add tool triage.lookup                 # declare a tool (review scope caps manually)
bot validate && bot doctor                 # check config and host prerequisites
```

See [First Tool](docs/first-tool.md), [Team Template](docs/team-template.md), and [Secret Stores & Commands](docs/gateway.md#secret-stores-and-commands).

## Documentation

| Topic | Guides |
|---|---|
| Design & Architecture | [Why This Design](docs/why.md) · [Architecture](docs/architecture.md) · [Glossary](docs/glossary.md) · [Roadmap](docs/roadmap.md) |
| Profiles & Tools | [Profile Spec](docs/profile-spec.md) · [First Tool](docs/first-tool.md) · [Team Template](docs/team-template.md) · [MCP Tools](docs/plugins.md) |
| Security & Recovery | [Security](docs/security.md) · [Auth & OAuth](docs/auth.md) · [Reliability](docs/reliability.md) · [Model Image](docs/model-image.md) |
| Operations & Extending | [Gateway](docs/gateway.md) · [Admin & Observability](docs/admin.md) · [Extending](docs/extending.md) · [Release Checklist](docs/release.md) |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

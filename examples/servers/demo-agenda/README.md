# demo-agenda — Synthetic Agenda Tool Server (Simulated)

Example tool server used by the README 5-minute quickstart. The `agenda` skill (`list` and `delete` in `tools.yaml`) under [`examples/profiles/personal`](../../profiles/personal) points to this server. It uses only the Python standard library, with no network egress, credentials, or file writes.

**Simulated only.** The installation is read-only and each call runs as a fresh process in an empty workspace, so nothing is persisted.

| Tool | Behavior |
|---|---|
| `list` | Returns built-in synthetic events (`e-walk` Walk 07:30 · `e-meet` Team meeting 14:00 · `e-read` Reading 21:00). |
| `delete` | Accepts only those synthetic IDs (errors on unknown IDs). Returns a **simulated delete receipt** (`sim-…`) with `"simulated": true, "persisted": false`. **Not permanently deleted** — `list` remains unchanged. Identical logical calls (same `_meta` `bot/profile-id`, `bot/run-id`, and `bot/call-id`) yield the same receipt across retries/handovers, while differing run or profile IDs yield distinct receipts (`bot/generation` is excluded). |

Input schemas match `tools.yaml` and are verified directly over MCP by `crates/bot-profile/tests/suite/demo_server.rs`. User-facing copy (CLI output and model replies) must clearly state that deletion was simulated and never claim permanent deletion.

## Setup & Manual Check

Register this folder under `servers` in host config (`BOT_HOST_CONFIG`) with `command` set to `["bin/server", "--stdio"]`:

```json
{"version": 1, "servers": {"demo-agenda": {"root": "/path/to/examples/servers/demo-agenda", "command": ["bin/server", "--stdio"]}}}
```

Verify directly (one JSON-RPC message per line):

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delete","arguments":{"event":"e-walk"},"_meta":{"bot/profile-id":"personal","bot/run-id":"r-1","bot/call-id":"c-1"}}}' \
  | bin/server --stdio
```

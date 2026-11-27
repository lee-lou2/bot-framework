# Build Your First Tool

English | [한국어](ko/first-tool.md)

Skills can run on instructions (`SKILL.md`) alone, or attach tools (`tools.yaml`) backed by an external MCP server. Tool code lives outside the profile directory as a standalone MCP stdio server; the profile holds only declarations and permission caps.

`bot add tool` creates or appends entries in `tools.yaml` and never edits `profile.yaml` or `host.json` automatically. This guide wires a minimal read-only tool, validates it, and extends it into an approval-gated write tool. For a complete reference, see [`demo-agenda`](../examples/servers/demo-agenda/bin/server) and [`examples/profiles/personal/agents/main/skills/agenda/tools.yaml`](../examples/profiles/personal/agents/main/skills/agenda/tools.yaml).

## Four-Part Checklist

All four items must align for `bot validate` and runtime execution to succeed:

| Step | Location | What to configure |
|---|---|---|
| 1 | Server directory + `host.json` (`servers.<alias>`) | Executable MCP stdio binary/script (`chmod +x`), absolute `root`, and `command` args |
| 2 | Skill `tools.yaml` | `server`, `id`, `class`, `scopes`, and `input` via `bot add tool <skill>.<tool>` (or `tools.lock.yaml` for remote HTTP) |
| 3 | `profile.yaml` (`roles`, `policy`) | Grant scopes in `roles`, allow them in `policy.scopes` and `policy.agents`, and add to `policy.approve` if gating writes |
| 4 | Optional (`env.yaml`, `secrets`, `egress`) | Non-secret config (`env.yaml` + tool `env`), secret names (`profile.yaml` `secrets` + tool `secrets` + secret store), outbound `egress` |

## Step 1: Create a Minimal MCP stdio Server

At startup, the host copies `servers.<alias>.root` into a read-only installation, hashes its contents to a SHA-256 revision, and runs each call in a fresh empty workspace. Save the following stdlib-only Python 3 server to `/absolute/path/servers/notes/bin/server` and run `chmod +x /absolute/path/servers/notes/bin/server`:

```python
#!/usr/bin/env python3
"""Minimal MCP stdio tool server (Python stdlib only)."""
import json, sys

TOOLS = [
    {
        "name": "lookup",
        "description": "Look up a fixed note by key (read-only example).",
        "inputSchema": {
            "type": "object",
            "required": ["key"],
            "properties": {"key": {"type": "string"}},
            "additionalProperties": False,
        },
    }
]
NOTES = {"status": "ready for deploy", "owner": "main"}

def send(mid, **body):
    line = json.dumps({"jsonrpc": "2.0", "id": mid, **body}, ensure_ascii=False) + "\n"
    sys.stdout.buffer.write(line.encode("utf-8"))
    sys.stdout.buffer.flush()

for raw in sys.stdin.buffer:
    line = raw.decode("utf-8", errors="replace").strip()
    if not line:
        continue
    msg = json.loads(line)
    if "id" not in msg:
        continue  # Ignore notifications such as notifications/initialized
    mid, method, params = msg["id"], msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        send(mid, result={
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "notes", "version": "0.1.0"},
        })
    elif method == "ping":
        send(mid, result={})
    elif method == "tools/list":
        send(mid, result={"tools": TOOLS})
    elif method == "tools/call" and params.get("name") == "lookup":
        key = (params.get("arguments") or {}).get("key", "")
        payload = {"key": key, "value": NOTES.get(key, "not found")}
        send(mid, result={"content": [{"type": "text", "text": json.dumps(payload, ensure_ascii=False)}], "isError": False})
    else:
        send(mid, error={"code": -32601, "message": "method or tool not found"})
```

- Never place symlinks, special files, secret values, or runtime data inside `root`.
- For line-size guards, deterministic retry-safe receipts keyed by `_meta` (`bot/profile-id`, `bot/run-id`, `bot/call-id`), and error formatting, see [`demo-agenda`](../examples/servers/demo-agenda/bin/server).

## Step 2: Scaffold the Skill and `tools.yaml`

Create a workspace with `bot init`, add a skill, and declare the tool:

```sh
bot init my-bot
bot add skill notes --dir ./my-bot/profiles/my-bot
bot add tool notes.lookup --class read --server notes --dir ./my-bot/profiles/my-bot
```

Update `input` in `my-bot/profiles/my-bot/skills/notes/tools.yaml` to match the server's `inputSchema` exactly (the broker verifies this schema on every call):

```yaml
version: 1
server: notes
tools:
  - id: lookup
    class: read
    scopes: [notes.lookup]
    secrets: {}
    egress: []
    input:
      type: object
      required: [key]
      properties: {key: {type: string}}
      additionalProperties: false
```

- If the server exposes a non-kebab name such as `get_note`, add `name: get_note` to the tool entry ([plugins.md](plugins.md#tool-definition-locking-toolslockyaml)).
- For an existing remote Streamable HTTP MCP server, omit `input` and run `bot mcp sync <profile>/skills/notes` to generate `tools.lock.yaml`.

## Step 3: Wire `host.json`, `profile.yaml`, and Optional Config

Follow the instructions printed by `bot add tool` to update `host.json` and `profile.yaml`.

**`my-bot/host.json`** (add `servers`):

```json
{
  "servers": {
    "notes": {
      "root": "/absolute/path/servers/notes",
      "command": ["bin/server", "--stdio"]
    }
  }
}
```

**`my-bot/profiles/my-bot/profile.yaml`** (append `notes.lookup` alongside the default memory scopes):

```yaml
roles:
  owner: [run, memory.search, memory.save, memory.forget, notes.lookup]
policy:
  scopes: [run, memory.search, memory.save, memory.forget, notes.lookup]
  # If scoped to a sub-agent, also add notes.lookup under policy.agents.<path>
```

If the tool needs config, credentials, or external network access, configure only what it uses:

- **Non-secret config:** Add `NOTES_NAMESPACE: team` in the agent's `env.yaml` and `env: [NOTES_NAMESPACE]` on the tool entry.
- **Secrets:** Declare `secrets: [notes-api-key]` in `profile.yaml`, map `secrets: {NOTES_API_KEY: notes-api-key}` in `tools.yaml`, and store the value in your secret backend (for the dev `env` store, `BOT_SECRET__my_bot__notes_api_key`).
- **Outbound network:** List `egress: ["api.example.com:443"]` on the tool entry (intersected with any host `egress` cap).

Validate offline (no DB or network connection required) and run `bot doctor`:

```sh
export BOT_PROFILES="$PWD/my-bot/profiles"
export BOT_HOST_CONFIG="$PWD/my-bot/host.json"
bot validate
bot doctor
```

`bot validate` succeeds when it prints `Server notes: revision <hash>` and `Entrance and secret checks passed` with zero warnings.

## Step 4: Upgrade to an Approval-Gated Write Tool

Classify state-changing tools as `write` or `irreversible`:

```sh
bot add tool notes.save --class write --dir ./my-bot/profiles/my-bot
```

1. **Implement the tool:** Add `save` to `TOOLS` and `tools/call` in your server, and align `input` in `tools.yaml` with its `inputSchema`.
2. **Choose the approval rule:**
   - **`class: write` + `policy.approve`:** Add `notes.save` to `roles` and `policy.scopes`. Listing `policy.approve: [notes.save]` requires a human approval card before every call; omitting it lets authorized members run directly.
   - **`class: irreversible`:** Always requires human approval even when omitted from `policy.approve` (like `agenda.delete` in [`examples/profiles/personal/agents/main/skills/agenda/tools.yaml`](../examples/profiles/personal/agents/main/skills/agenda/tools.yaml)).
3. **Multi-step writes:** If the tool performs multiple external writes that must not duplicate after a crash, declare `effects: true` and use the `effect.request`/`effect.status` callbacks ([plugins.md](plugins.md#optional-callbacks-for-multi-step-writes)).
4. **Re-validate and activate:** Run `bot validate`, then publish to a running deployment with `bot serve --role all --activate`. Any change to server code changes its install revision, invalidating old approval cards.

See [plugins.md](plugins.md) and [profile-spec.md](profile-spec.md) for full protocol, remote HTTP, lockfile, and callback specifications.

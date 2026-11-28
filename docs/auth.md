# Authentication, OAuth, and Managed Credentials

English | [한국어](ko/auth.md)

Host processes resolve all model and remote MCP credentials through a single `AuthResolver` port (`crates/bot-core/src/auth.rs`). Models, sandboxes, and VMs never see API keys, OAuth access/refresh tokens, or service-account private keys.

## Storage and Encryption (`bot_credentials`)

Static secrets continue to live in `SecretStore` (`keychain`, `sops:<file>`, or `env`). Rotating or host-managed credentials (`oauth`, `service-account`, `subscription-token`) are encrypted on the host using **AES-256-GCM** (`ring`) and stored in PostgreSQL (`bot_credentials`, migration `0030_credentials.sql`) so multi-worker Linux and macOS deployments share one durable source of truth (per-worker caches revalidate metadata against PostgreSQL every 30 seconds so `bot auth logout`, rotations, and `needs_login` take effect across running workers without a restart).

- **Master key:** Generate a 32-byte random key (64 hex characters) and store it in your `SecretStore` under profile `host` and reserved secret name `credential-key` (`bot/host/credential-key`). `bot validate` and `bot doctor` verify its presence whenever any provider or remote MCP server references a managed `auth` entry.
- **AAD binding & egress cap:** Every ciphertext is authenticated against `(profile, name, kind, token_endpoint_host:port)` with a fresh 96-bit nonce on every write. Moving ciphertext across profiles, names, kinds, or token endpoints fails decryption. Every OAuth discovery, registration, and token endpoint requires an explicit `egress.<profile>` `host:port` allowlist entry (fails closed if omitted). Changing an existing credential's token endpoint host or OAuth issuer requires explicit `bot auth logout` first.
- **Single-flight refresh & crash safety:** Expiring tokens refresh inside a dedicated PostgreSQL connection holding a row-scoped advisory lock (`pg_try_advisory_lock`). The host verifies the active revision (`ActiveCheck`) both before marking the row `refreshing` and again immediately before sending the outbound token POST. Only permanent token-revocation errors (`invalid_grant`, `refresh_token_reused`, `refresh_token_expired`, `refresh_token_invalidated`) or an interrupted rotating OAuth refresh after a process crash quarantine the row to `needs_login` (`credential_needs_login`) until an operator re-authenticates; transient `5xx`/`429`/network/client-config errors restore `active` with a short cooldown. Detached refresh tasks finish committing cleanly on graceful shutdown.
- **Redaction:** Loaded and freshly minted access tokens, refresh tokens, client secrets, and JWT assertions are registered in the live `Redactor` before outbound transmission and screened against upstream responses.

## Supported Auth Modes and Surface Policy

| Mode | `host.json` `auth.kind` | Permitted Surfaces | Notes |
|---|---|---|---|
| Static API key / bearer | *(uses `secret`)* | Direct API, VM CLI relay, Remote MCP | Existing `SecretStore` path; unchanged fingerprints |
| Remote MCP OAuth 2.0 | `oauth` | Remote HTTP MCP (`servers.<alias>.auth`) | RFC 9728 + RFC 8414 + RFC 7591 + PKCE S256; optional pinned `issuer` bound into approvals; pre-send `401` forced refresh |
| Operator-owned OAuth 2.0 | `oauth` | Direct API, VM CLI relay, Remote MCP | Standard RFC 6749 refresh (`form` or `json` body), HTTPS + explicit `egress.<profile>` required |
| Google Service Account *(experimental)* | `service-account` | Direct API, VM CLI relay (`agy-cli`, etc.), Remote MCP | Unencrypted PKCS#8 RSA key signed locally as RS256 JWT-bearer; verified against local PG/token-endpoint harness (not yet verified against live Google upstream) |
| Opt-in Claude subscription *(experimental)* | `subscription-token` | `claude-cli` VM relay **only** (`api.anthropic.com`) | Requires `allow_subscription_auth.<profile>: ["claude"]`; merges `oauth-2025-04-20` into `anthropic-beta`; refused on `anthropic-api` and non-first-party endpoints; not yet verified against live upstream |
| Opt-in Codex subscription *(experimental)* | `oauth` *(imported)* | `codex-cli` VM relay **only** (`chatgpt.com`) | Requires `allow_subscription_auth.<profile>: ["codex"]`; dedicated `CODEX_HOME` import (`auth.openai.com:443` in `egress.<profile>`); refuses live `~/.codex/auth.json`, `openai-api`, and non-first-party endpoints; not yet verified against live upstream |
| Google/Antigravity consumer OAuth | *(not supported)* | Refused | Deliberately omitted due to Google Terms of Service restrictions; use API keys or `service-account` |

## Configuring `host.json`

### Remote MCP Server OAuth

```json
{
  "egress": {
    "personal": ["mcp.linear.app:443"]
  },
  "servers": {
    "linear": {
      "transport": "http",
      "url": "https://mcp.linear.app/mcp",
      "auth": {
        "kind": "oauth",
        "credential": "linear-oauth",
        "issuer": "https://mcp.linear.app"
      }
    }
  }
}
```

### Model Providers (`auth` vs `secret`)

`secret` and `auth` are mutually exclusive on `claude-cli`, `codex-cli`, `agy-cli`, `anthropic-api`, and `openai-api`. Optional `max_tokens` (`1..=1000000`, operational tuning excluded from activation fingerprint) is supported on `claude-cli` as well as direct API providers.

```json
{
  "allow_subscription_auth": {
    "personal": ["claude", "codex"]
  },
  "providers": {
    "claude": {
      "kind": "claude-cli",
      "model": "claude-sonnet-4-6",
      "endpoint": "https://api.anthropic.com",
      "auth": {"kind": "subscription-token", "credential": "claude-sub"},
      "max_tokens": 16384
    },
    "codex": {
      "kind": "codex-cli",
      "model": "gpt-5.4",
      "endpoint": "https://chatgpt.com/backend-api/codex",
      "auth": {"kind": "oauth", "credential": "codex-sub"}
    },
    "agy": {
      "kind": "agy-cli",
      "model": "gemini-3.1-pro-preview",
      "endpoint": "https://generativelanguage.googleapis.com",
      "auth": {"kind": "service-account", "credential": "gcp-sa"}
    }
  }
}
```

- `allow_subscription_auth` is a per-profile opt-in map accepting only `"claude"` and `"codex"`. When non-empty, it folds into the activation fingerprint (requiring `bot serve --activate`) and logs a fixed startup warning.
- `anthropic-api` and `openai-api` accept `auth.kind` `"oauth"` or `"service-account"` only; `"subscription-token"`, imported Codex subscription credentials, and `sk-ant-oat…` static keys are rejected with `credential_denied`.

## `bot auth` Commands

All `bot auth` subcommands connect to `BOT_DATABASE_URL`, read `host/credential-key` from `BOT_SECRETS`, and never echo secret values. Commands that read secret values from stdin refuse interactive TTYs.

| Command | Purpose & Flags |
|---|---|
| `bot auth login` | PKCE S256 browser login. Use `--profile <p> --server <alias>` (RFC 9728 + RFC 8414 + RFC 7591 discovery) or `--profile <p> --name <n> --authorize-url <url> --token-url <url>`. Optional: `--client-id <id>`, `--scope <s>`, `--paste` (headless redirect URL paste instead of loopback listener). |
| `bot auth set-oauth` | Store an operator-owned OAuth JSON bundle (`{access_token, refresh_token?, expires_in?, client_secret?}`) piped via stdin: `--profile <p> --name <n> --token-endpoint <url> --client-id <id>`, optional `--scope <s>`, repeatable `--header Name=Value` (allowlisted: `chatgpt-account-id`, `openai-organization`, `x-goog-user-project`), `--json-body`. |
| `bot auth set-service-account` | Validate and store a Google service-account JSON key (unencrypted PKCS#8 RSA): `--profile <p> --name <n> --file <path>`, optional `--scope <scope>` (defaults to `https://www.googleapis.com/auth/cloud-platform`). |
| `bot auth set-token` | Store an opt-in Claude setup-token (`sk-ant-oat…` from `claude setup-token`) piped via stdin: `--profile <p> --name <n> --provider claude`. |
| `bot auth import-codex` | Import a dedicated Codex OAuth login (`CODEX_HOME=<dedicated-dir> codex login --device-auth`): `--profile <p> --name <n> --from <path>`. Verifies regular file (`O_NOFOLLOW`, `nlink == 1`, owned by current UID, mode `0600`), refuses live `~/.codex/auth.json` and `$CODEX_HOME/auth.json`, drops `id_token`/`OPENAI_API_KEY`, and deletes the source file unless `--keep-source` is passed. |
| `bot auth status` | List stored credentials (`kind`, `state`, `version`, expiry, endpoint `host:port`): optional `--profile <p>`, `--json`. |
| `bot auth refresh` | Force-refresh an OAuth or service-account credential now: `--profile <p> --name <n>`. |
| `bot auth logout` | Delete a stored credential from PostgreSQL and append an audit entry: `--profile <p> --name <n>`. |

Check stored credential health alongside DB connectivity with `bot doctor --check-db`.

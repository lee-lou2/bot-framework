# Model VM Image

English | [한국어](ko/model-image.md)

The VM-hosted model CLIs (`claude-cli`, `codex-cli`, `agy-cli`) run inside Apple container VMs built locally by the operator and pinned by digest. This repository ships only the image definitions (`images/model/`) and build/verification scripts — never a prebuilt image. Each CLI has its own Containerfile (`images/model/Containerfile` for `claude`, `images/model/Containerfile.codex` for `codex`, `images/model/Containerfile.agy` for `agy`).

- **Supply chain:** base Alpine image is pinned by digest; Claude Code (`images/model/Containerfile`) is pinned by exact version (`2.1.331`) and SHA-256 with GPG verification against Anthropic's release signing key fingerprint. `Containerfile.codex` and `Containerfile.agy` follow the same pinned-digest build contract using operator-pinned Linux arm64 releases.
- **Image layout:** single flattened layer (`FROM scratch`) built from digest-pinned Alpine + `nftables` + `setpriv`, stripped of `apk` and all unused BusyBox symlinks (keeping only `/bin/sh` and `/bin/sleep`).
- **Runtime posture:** runs non-root (`10001:10001`) on a read-only rootfs with auto-updates, telemetry, error reporting, and other nonessential traffic disabled via official environment variables. Trusted `bot-init` starts as root to apply a relay-only firewall and drop all privileges before `exec`ing the selected CLI.

```sh
ref=$(images/model/build.sh --cli claude)   # `--cli claude|codex|agy` (default `claude`); prints `name@sha256:...`
images/model/verify.sh "$ref"               # Verify real image against a fake upstream (no real model or API key)
```

Set `sandbox.apple.image` (for a `claude-cli`-only host) or `sandbox.apple.images` (`{"claude": "...", "codex": "...", "agy": "..."}`) in `host.json` to the digest references printed by `build.sh`. Because `container build` produces only mutable `name:tag` refs, `build.sh` temporarily starts a digest-pinned CNCF Distribution registry bound to `127.0.0.1`, pushes the image, pulls it back **by digest** to store an immutable local ref, and deletes the registry on exit (not needed at runtime). Do not run `container image prune -a` — it deletes unused digest references.

## Adapter Contract

| Item | Contract |
|---|---|
| Entrypoint | `ENTRYPOINT` is `/usr/local/bin/bot-init`; first argument must be one of `claude`, `codex`, or `agy`, mapped to `/usr/local/bin/<cli>`. Never accepts binary path, user, or relay address via CLI arguments. |
| Capabilities | `--cap-drop ALL` plus only `CAP_NET_ADMIN` (`nft`), `CAP_SETUID`, `CAP_SETGID`, and `CAP_SETPCAP` (clear bounding set). Missing any one fails closed. |
| Host-only env | `BOT_RELAY_IP` (RFC 1918 IPv4) and `BOT_RELAY_PORT` (`1..65535`), set by the adapter from the bound listener alongside the CLI relay URL (`ANTHROPIC_BASE_URL`, `GOOGLE_GEMINI_BASE_URL`, or validated Codex `-c` URLs). `bot-init` re-validates both and unsets them before executing the CLI. |
| Init sequence | Applies `nftables` rules allowing only relay TCP and established replies, reads them back to verify, runs a separate check verifying UID/GID `10001`, no supplementary groups, zero capabilities, and `no_new_privs`, then drops privileges and `exec`s the CLI (for `agy`, expands `/work/mcp.agy.template.json` into RAM-only `/tmp/mcp_config.json` with `0600` as UID `10001` before `exec`). |
| Failure | Init exits `1` with single-word stderr `vm_init_failed` and never starts the model CLI. Once the CLI starts, its exit code passes through unchanged. |
| Anchor | `--entrypoint /usr/local/bin/bot-anchor --user 10001:10001 --cap-drop ALL`: idles only when non-root with zero capabilities, otherwise exits `1` with `vm_anchor_failed`. Separate from `bot-init`. |
| Mounts & paths | Read-only rootfs, tmpfs `/tmp`, read-only `/bot` (workdir), per-run writable `/work` (prepared/cleaned by host; init never `chown`s). Session tokens pass solely via env vars (`RELAY_TOKEN`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`) and are never written to host disk (`/work/mcp.claude.json` and `/work/mcp.agy.template.json` contain `${RELAY_TOKEN}` placeholders only; `codex` reads `RELAY_TOKEN` directly). |
| Network | Host-only (`--internal`), `--no-dns`. Only the host's `http://` relay is reachable. |

## What `verify.sh` Checks

`images/model/verify.sh` launches the real image with adapter-equivalent flags alongside a no-firewall control run confirming the blocked paths are otherwise open:

- **Version & init integrity:** `claude --version` runs via `bot-init` on a read-only rootfs and matches the image label; image `bot-init` matches `images/model/bot-init`.
- **Fail-closed init rejection:** missing/invalid relay env, any missing capability, non-root launch, or non-`claude` first argument exits non-zero with exact stderr `vm_init_failed` and never launches the model.
- **Privilege drop & firewall:** model runs as UID/GID `10001`, no supplementary groups, all capability sets zero, `no_new_privs=1`. Relay is reachable while other gateway ports, IPv4/IPv6 sibling VMs, host IPv6/LAN ports, internet (`1.1.1.1:443`, `8.8.8.8:53`), inbound peer traffic, new network namespaces, and all 10 tamper attempts (rules, addresses, routes, privileges) are blocked.
- **Fake-upstream chat:** real `claude -p --bare` completes against the local fake upstream with only 1 `/v1/messages` request and 0 attempts to other hosts; a control run clearing the disable env vars records outbound `CONNECT` attempts (e.g., `api.anthropic.com:443`).
- **Session persistence & anchor:** UID `10001` writes `/work/claude/projects/-bot/<id>.jsonl` in a `0700` `/work`, a fresh container resumes via `--resume`, and the host reads and deletes the files. `bot-anchor` stays running non-root with zero caps and exits `vm_anchor_failed` if root or granted any cap.

Both `build.sh` and `verify.sh` acquire `/tmp/bot-vm-canary.lock` to serialize real VM runs on the host. `verify.sh` creates a temporary host-only network on an unused `192.168.200..239.0/24` subnet (override via `BOT_MODEL_VERIFY_SUBNET`) and runs helper VMs on that network to avoid colliding with dormant container networks.

## Bumping Versions

1. Verify the new release's signed `manifest.json` and read `platforms.linux-arm64-musl.checksum`.
2. Update `CLAUDE_VERSION` and `CLAUDE_SHA256` together in `images/model/Containerfile` (for base-image bumps, update both `BUILDER` tag and digest).
3. Re-run `images/model/build.sh` and `images/model/verify.sh`, then put the new digest reference into `sandbox.apple.image`.

`nftables` and `setpriv` install from the pinned Alpine branch and are recorded with versions in `/etc/bot-model-image`. Rebuilding the same `Containerfile` after Alpine package updates produces a different digest, so always deploy the exact digest printed by `build.sh`.

## Notes

- **v2.0 scope:** Only image definitions and build/verify scripts are published; operators build locally (prebuilt distribution is deferred).
- **Licenses:** `deny.toml` covers Rust dependencies and committed repository files only. Inspect `/etc/bot-model-image` inside the built image for installed package versions and licenses (BusyBox, nftables, setpriv).
- **Defense in depth:** `--internal` alone still allows reaching host services bound to `0.0.0.0` and sibling VMs on the same network; `bot-init` `nftables` blocks both inside the VM, while the host binds the relay strictly to the gateway IP.
- **Platform:** Apple silicon only (`linux-arm64-musl`). Apple container provides a Linux kernel with built-in nftables (`CONFIG_NF_TABLES`).
- **Cache cleanup:** Reclaim builder cache and base images with `container builder stop` and `container image prune` (never `-a`).

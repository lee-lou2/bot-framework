#!/usr/bin/env bash
# Build the model VM image locally and pin it by digest (Apple container, Apple silicon). See docs/model-image.md.
#
#   images/model/build.sh              build, pin by digest, and print the image reference
#   NO_CACHE=1 images/model/build.sh   rebuild fetch and signature verification without cache
#
# Stdout emits only the single digest reference (`name@sha256:...`) for `sandbox.apple.image`; progress and summary go to stderr:
#   ref=$(images/model/build.sh)
# Env: MODEL_IMAGE_NAME (default bot-model-vm), MODEL_IMAGE_REGISTRY_PORT (default 5088, bound briefly to 127.0.0.1).
# Acquires `/tmp/bot-vm-canary.lock` to serialize real VM runs on the same host.
#
# Why a temporary loopback registry: `container build` produces only a mutable name:tag reference (`name@sha256:...`
# cannot be resolved or run directly). Pushing to a digest-pinned loopback CNCF Distribution registry and pulling back
# by digest stores an immutable local reference; the registry is removed on exit and is not needed at runtime.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
name=${MODEL_IMAGE_NAME:-bot-model-vm}
port=${MODEL_IMAGE_REGISTRY_PORT:-5088}
registry_image=docker.io/library/registry:3@sha256:82f6ff4a7f982960c8aa51617fea3aabc485df6e55cce80d22deb1de36f1bc3c
registry_name=bot-model-image-registry
stamp=$(date +%Y%m%d%H%M%S)-$$
tag=$name:build-$stamp
host_ref=127.0.0.1:$port/$name

say() { echo "$@" >&2; }
die() { say "error: $*"; exit 1; }

case "${1:-}" in
  -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 0 ;;
  '') ;;
  *) die "unknown argument: $1 (see --help)" ;;
esac

# Serialize real VM runs via `/tmp/bot-vm-canary.lock` (shared with adapter VM tests) to avoid flaky concurrent runs.
if [ -z "${BOT_VM_LOCKED:-}" ]; then
  BOT_VM_LOCKED=1 exec python3 -I -c 'import fcntl, os, sys
fd = os.open("/tmp/bot-vm-canary.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o644)
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    print("waiting for another VM task to finish (/tmp/bot-vm-canary.lock)", file=sys.stderr)
    fcntl.flock(fd, fcntl.LOCK_EX)
os.set_inheritable(fd, True)
os.execvp(sys.argv[1], sys.argv[1:])' "${BASH:-bash}" "$0" "$@"
fi

command -v container >/dev/null 2>&1 || die "Apple container is required (brew install container)"
command -v curl >/dev/null 2>&1 || die "curl is required"
[ "$(uname -m)" = arm64 ] || die "Apple silicon (arm64) only"
container system status >/dev/null 2>&1 || die "container service is not running: container system start"
case "$name" in '' | *[!a-z0-9._-]*) die "MODEL_IMAGE_NAME may only contain lowercase letters, digits, and . _ -" ;; esac
case "$port" in '' | *[!0-9]*) die "MODEL_IMAGE_REGISTRY_PORT must be numeric" ;; esac
if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
  die "port $port is already in use: choose another via MODEL_IMAGE_REGISTRY_PORT"
fi

cleanup() {
  container stop "$registry_name" >/dev/null 2>&1 || true
  container delete --force "$registry_name" >/dev/null 2>&1 || true
  container image delete "$host_ref:$stamp" >/dev/null 2>&1 || true
  container image delete "$tag" >/dev/null 2>&1 || true
}
trap cleanup EXIT
container delete --force "$registry_name" >/dev/null 2>&1 || true

progress=plain
[ -t 2 ] && progress=auto
say "==> Building image (pinned base digest, Claude Code version/SHA-256, and GPG signature)"
no_cache=()
[ -n "${NO_CACHE:-}" ] && no_cache=(--no-cache)
container build --platform linux/arm64 --tag "$tag" --file "$here/Containerfile" --progress "$progress" \
  ${no_cache[@]+"${no_cache[@]}"} "$here" >&2 || die "build failed (see output above)"

say "==> Pinning digest via temporary loopback registry 127.0.0.1:$port"
container image pull --platform linux/arm64 --progress none "$registry_image" >&2 || die "failed to pull temporary registry image"
container run -d --rm --name "$registry_name" -p "127.0.0.1:$port:5000" "$registry_image" >/dev/null \
  || die "failed to start temporary registry"
up=
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$port/v2/" >/dev/null 2>&1; then up=1; break; fi
  sleep 1
done
[ -n "$up" ] || die "temporary registry did not respond"

container image tag "$tag" "$host_ref:$stamp" >&2
container image push --scheme http --platform linux/arm64 --progress none "$host_ref:$stamp" >&2 || die "failed to push image"
digest=$(curl -fsSI -H 'Accept: application/vnd.oci.image.index.v1+json' "http://127.0.0.1:$port/v2/$name/manifests/$stamp" \
  | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" { print $2 }')
case "$digest" in sha256:????????????????????????????????????????????????????????????????) ;; *) die "registry did not return a digest" ;; esac
ref=$host_ref@$digest

container image delete "$host_ref:$stamp" >/dev/null 2>&1 || true
container image pull --scheme http --platform linux/arm64 --progress none "$ref" >&2 || die "failed to pull image by digest"
container image inspect "$ref" >/dev/null 2>&1 || die "digest reference not found via inspect"
# Verify pushed index digest prefix matches the locally built tag.
short=${digest#sha256:}
short=${short:0:12}
built=$(container image list 2>/dev/null | awk -v n="$name" -v t="$tag" '$1 == n && $2 == substr(t, length(n) + 2) { print $3; exit }')
[ "$built" = "$short" ] || die "pushed image (${short}) does not match built image (${built:-?})"

say "==> Stopping registry and verifying execution by digest reference"
container stop "$registry_name" >/dev/null 2>&1 || true
# Smoke-test bot-init (nft rules + privilege drop) on this host kernel using a dummy private relay address.
version=$(container run --progress none --rm --name "bot-model-image-check-$$" \
  --cap-drop ALL --cap-add CAP_NET_ADMIN --cap-add CAP_SETUID --cap-add CAP_SETGID --cap-add CAP_SETPCAP \
  -e BOT_RELAY_IP=10.0.0.1 -e BOT_RELAY_PORT=1 "$ref" claude --version 2>/dev/null | tail -1) || true
case "$version" in *"(Claude Code)") ;; *) die "failed to run via bot-init using digest reference (check manually with container run)" ;; esac

say ""
say "Image ref  : $ref"
say "Claude Code: $version"
say "Size       : $(container image list --verbose 2>/dev/null | awk -v n="$host_ref" -v d="$short" '$1 == n && $3 == d { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9.]+$/ && $(i + 1) ~ /^[KMG]B$/) { print $i, $(i + 1); exit } }') (compressed)"
say ""
say "Add to host config (BOT_HOST_CONFIG):"
say "  \"sandbox\": { \"apple\": { \"image\": \"$ref\", \"network\": \"<host-only network>\", \"relay_bind\": \"<network host IP:port>\" } }"
say "Verify (init rejection, firewall, privilege drop, anchor, chat, session): images/model/verify.sh \"$ref\""
say "Keep this reference: 'container image prune -a' removes unused local images."
echo "$ref"

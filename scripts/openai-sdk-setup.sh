#!/usr/bin/env bash
# Set up the official OpenAI SDK (Python 2.29.0, Node 6.29.0) end-to-end test environment under the target directory (never inside the repository):
#   venv  requirements.lock.txt via `pip install --require-hashes --no-deps` (exact hashes, wheels only)
#   node  package-lock.json via `npm ci --ignore-scripts` (integrity hashes, no install scripts)
# Verifies pinned SDK versions and OpenAPI schema commit/sha256; reuses an intact install offline when lock stamps match.
# Usage: scripts/openai-sdk-setup.sh [install-dir]   (default: ${CARGO_TARGET_DIR:-<repo>/target}/openai-sdk; tests read BOT_OPENAI_SDK_DIR)
# Requires: CPython 3.12 (python3.12 or python3), Node >= 22, npm. Test-only deps include hash-pinned MPL-2.0 wheels (certifi, tqdm).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/crates/bot-cli/tests/openai_sdk"
dest="${1:-${CARGO_TARGET_DIR:-$root/target}/openai-sdk}"
case "$dest" in /*) ;; *) echo "install directory must be an absolute path: $dest" >&2; exit 2;; esac

python=""
for candidate in python3.12 python3; do
  if command -v "$candidate" >/dev/null 2>&1 && [ "$("$candidate" -c 'import sys; print("%d.%d" % sys.version_info[:2])')" = 3.12 ]; then python="$(command -v "$candidate")"; break; fi
done
[ -n "$python" ] || { echo "CPython 3.12 is required (lockfile allows 3.12 wheels only)." >&2; exit 1; }
for tool in node npm; do command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required." >&2; exit 1; }; done
[ "$(node -p 'process.versions.node.split(".")[0]')" -ge 22 ] || { echo "Node 22 or newer is required." >&2; exit 1; }

mkdir -p "$dest"
# Reuse existing install offline when lockfile stamps match.
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }
stamp="$(sha256 <"$src/requirements.lock.txt")"
node_stamp="$(cat "$src/node/package.json" "$src/node/package-lock.json" | sha256)"
if [ ! -x "$dest/venv/bin/python" ] || [ "$(cat "$dest/venv.stamp" 2>/dev/null)" != "$stamp" ]; then
  rm -rf "$dest/venv"
  "$python" -m venv "$dest/venv"
  "$dest/venv/bin/python" -m pip install --quiet --disable-pip-version-check --require-hashes --no-deps --only-binary=:all: -r "$src/requirements.lock.txt"
  "$dest/venv/bin/python" -m pip check --disable-pip-version-check
  echo "$stamp" >"$dest/venv.stamp"
fi

mkdir -p "$dest/node"
cp "$src/node/package.json" "$src/node/package-lock.json" "$dest/node/"
if [ ! -f "$dest/node/node_modules/openai/package.json" ] || [ "$(cat "$dest/node.stamp" 2>/dev/null)" != "$node_stamp" ]; then
  (cd "$dest/node" && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error)
  echo "$node_stamp" >"$dest/node.stamp"
fi
cp "$src"/node/*.mjs "$dest/node/"

# Verify pinned SDK versions and schema commit/sha256 offline; optionally verify subset against BOT_OPENAI_SPEC.
"$dest/venv/bin/python" -I -c 'import importlib.metadata as m; assert m.version("openai") == "2.29.0", m.version("openai")'
[ "$(node -p "require('$dest/node/node_modules/openai/package.json').version")" = 6.29.0 ] || { echo "Node SDK version is not 6.29.0" >&2; exit 1; }
"$dest/venv/bin/python" -I -c "import sys; sys.path.insert(0, '$src/py'); from conformance import Schema; s = Schema(); print('official schema', s.source['openapi'], s.source['commit'][:12], 'sha256', s.source['spec_sha256'][:12])"
if [ -n "${BOT_OPENAI_SPEC:-}" ]; then "$dest/venv/bin/python" -I "$src/schema/extract_subset.py" "$BOT_OPENAI_SPEC" --check && echo "schema subset matches pinned spec"; fi
echo "ready: $dest   (BOT_OPENAI_SDK_DIR=$dest)"

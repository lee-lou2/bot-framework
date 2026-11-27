#!/usr/bin/env bash
# Runs the official Python SDK test suite against contract_stub.py through sdk_run.py and wire_proxy.py.
# Usage: run_stub.sh <venv_python> [mutant]   Optional: ONLY=main|tight, PATTERNS="a.py b.py" (full schema coverage is enforced only on full runs).
set -u
PY="$1"; MUTANT="${2:-}"; HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS="$(mktemp -d "${TMPDIR:-/tmp}/openai-stub.XXXXXX")"; PIDS=(); FAILED=0
trap 'kill "${PIDS[@]}" 2>/dev/null; rm -rf "$LOGS"' EXIT
port() { head -1 "$1"; }
start() {
  local name="$1"; shift; "$@" >"$LOGS/$name.port" 2>"$LOGS/$name.err" & PIDS+=($!)
  for _ in $(seq 100); do [ -s "$LOGS/$name.port" ] && return; sleep 0.1; done; echo "$name failed to start"; cat "$LOGS/$name.err"; exit 99
}
run_profile() {
  local profile="$1" wait_ms="$2"; shift 2
  local files=("$@") partial=--partial
  if [ ${#files[@]} -eq 0 ]; then
    partial=""
    while IFS= read -r file; do files+=("$file"); done < <("$PY" -I "$HERE/sdk_run.py" list "$profile" py)
    [ ${#files[@]} -gt 0 ] || { echo "[$profile] no Python test files listed in expected.json"; FAILED=$((FAILED + 1)); return; }
  fi
  start "stub-$profile" env STUB_PROFILE="$profile" STUB_MUTANT="$MUTANT" "$PY" -I "$HERE/contract_stub.py"
  local stub; stub="$(port "$LOGS/stub-$profile.port")"
  start "wire-$profile" env WIRE_VALIDATE=1 "$PY" -I "$HERE/wire_proxy.py" "$stub"
  local wire; wire="$(port "$LOGS/wire-$profile.port")"
  curl -fsS -X POST "http://127.0.0.1:$wire/__wire/reset-all" >/dev/null
  for file in "${files[@]}"; do
    env OPENAI_BASE_URL="http://127.0.0.1:$wire/v1" OPENAI_WIRE_URL="http://127.0.0.1:$wire/__wire" OPENAI_E2E_CONTROL="http://127.0.0.1:$stub/__ctl" \
        OPENAI_API_KEY=build-api-key OPENAI_ROTATED_KEY=rotated-api-key OPENAI_GUEST_KEY=guest-api-key OPENAI_OTHER_KEY=other-api-key OPENAI_WAIT_MS="$wait_ms" \
        "$PY" -I "$HERE/sdk_run.py" py "$file" 2>"$LOGS/py.err" | tail -n 1
    local code="${PIPESTATUS[0]}"
    if [ "$code" -ne 0 ]; then FAILED=$((FAILED + 1)); tail -n "${TAIL:-30}" "$LOGS/py.err"; fi
  done
  "$PY" -I "$HERE/sdk_run.py" proxy --url "http://127.0.0.1:$wire/__wire" --profile "$profile" --scope python $partial >/dev/null 2>"$LOGS/proxy.err" || { cat "$LOGS/proxy.err"; FAILED=$((FAILED + 1)); }
  echo "[$profile] proxy: $(curl -fsS "http://127.0.0.1:$wire/__wire/summary" | cut -c1-300)"
}
if [ -n "${ONLY:-}" ]; then
  # shellcheck disable=SC2086  # PATTERNS is a whitespace-separated list of test filenames.
  run_profile "$ONLY" "$([ "$ONLY" = main ] && echo 120000 || echo 3000)" ${PATTERNS:-}
else
  run_profile main 120000
  run_profile tight 3000
fi
[ "$FAILED" -eq 0 ] || exit 1

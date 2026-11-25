#!/usr/bin/env bash
# 계약 대역 위에서 공식 Node SDK 시험을 돌린다. 규칙은 파이썬 러너와 같다(`sdk_run.py`: expected.json의 시험이 전부 통과해야 하고 skip·todo·누락·미등록·통과 0건·요청 0건은 실패).
# 사용: run_node_stub.sh <venv 파이썬> <node_modules가 있는 node 폴더> [변이 이름]   ONLY=main|tight 로 한 묶음만(스키마 전수 검사는 전체 실행에서만).
set -u
PY="$1"; NODE_DIR="$2"; MUTANT="${3:-}"; HERE="$(cd "$(dirname "$0")" && pwd)"
cp "$HERE"/node/*.mjs "$NODE_DIR"/   # 시험 파일을 node_modules가 있는 곳으로 복사한다(ESM은 시험 파일 옆의 node_modules에서 SDK를 찾는다)
LOGS="$(mktemp -d "${TMPDIR:-/tmp}/openai-node-stub.XXXXXX")"; PIDS=(); FAILED=0
trap 'kill "${PIDS[@]}" 2>/dev/null; rm -rf "$LOGS"' EXIT
start() { local name="$1"; shift; "$@" >"$LOGS/$name.port" 2>"$LOGS/$name.err" & PIDS+=($!); for _ in $(seq 100); do [ -s "$LOGS/$name.port" ] && return; sleep 0.1; done; echo "$name 이 뜨지 않았다"; exit 99; }
run_profile() { # <main|tight> <대기 ms>
  local profile="$1" wait_ms="$2" partial=""
  [ -z "${ONLY:-}" ] || partial=--partial
  local files=()
  while IFS= read -r file; do files+=("$file"); done < <("$PY" -I "$HERE/sdk_run.py" list "$profile" node)
  [ ${#files[@]} -gt 0 ] || { echo "[$profile] expected.json에 Node 시험 파일이 없다"; FAILED=$((FAILED + 1)); return; }
  start "stub-$profile" env STUB_PROFILE="$profile" STUB_MUTANT="$MUTANT" "$PY" -I "$HERE/contract_stub.py"
  local stub; stub="$(head -1 "$LOGS/stub-$profile.port")"
  start "wire-$profile" env WIRE_VALIDATE=1 "$PY" -I "$HERE/wire_proxy.py" "$stub"
  local wire; wire="$(head -1 "$LOGS/wire-$profile.port")"
  curl -fsS -X POST "http://127.0.0.1:$wire/__wire/reset-all" >/dev/null
  for file in "${files[@]}"; do
    env OPENAI_BASE_URL="http://127.0.0.1:$wire/v1" OPENAI_WIRE_URL="http://127.0.0.1:$wire/__wire" OPENAI_E2E_CONTROL="http://127.0.0.1:$stub/__ctl" \
        OPENAI_API_KEY=build-api-key OPENAI_ROTATED_KEY=rotated-api-key OPENAI_GUEST_KEY=guest-api-key OPENAI_OTHER_KEY=other-api-key OPENAI_WAIT_MS="$wait_ms" \
        "$PY" -I "$HERE/sdk_run.py" node "$file" --node-dir "$NODE_DIR" 2>"$LOGS/node.err" | tail -n 1
    local code="${PIPESTATUS[0]}"
    if [ "$code" -ne 0 ]; then FAILED=$((FAILED + 1)); tail -n 40 "$LOGS/node.err"; fi
  done
  "$PY" -I "$HERE/sdk_run.py" proxy --url "http://127.0.0.1:$wire/__wire" --profile "$profile" --scope node $partial >/dev/null 2>"$LOGS/proxy.err" || { cat "$LOGS/proxy.err"; FAILED=$((FAILED + 1)); }
  echo "[$profile] 프록시: $(curl -fsS "http://127.0.0.1:$wire/__wire/summary" | cut -c1-300)"
}
if [ "${ONLY:-main}" = main ]; then run_profile main 120000; fi
if [ "${ONLY:-tight}" = tight ]; then run_profile tight 3000; fi
[ "$FAILED" -eq 0 ] || exit 1

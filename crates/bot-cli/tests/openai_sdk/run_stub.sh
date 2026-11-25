#!/usr/bin/env bash
# 계약 대역(contract_stub.py) 위에서 공식 SDK의 파이썬 시험을 돌린다 — 시험 자체가 계약을 어긴 구현을 잡는지 보는 용도다(제품 입구는 openai_compat 시험이 따로 본다).
# 시험은 `sdk_run.py`가 돌리고 검사한다: expected.json의 시험이 전부 통과해야 하고(건너뜀·xfail·누락·미등록·통과 0건은 실패), 프록시가 본 요청 0건·위반·검사하지 않은 공식 스키마도 실패다.
# 사용: run_stub.sh <venv 파이썬> [변이 이름]   종료 코드 0이면 문제 없음. ONLY=main|tight, PATTERNS="a.py b.py"로 일부만 돌릴 수 있다(변이 확인이 쓴다; 스키마 전수 검사는 전체 실행에서만).
set -u
PY="$1"; MUTANT="${2:-}"; HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS="$(mktemp -d "${TMPDIR:-/tmp}/openai-stub.XXXXXX")"; PIDS=(); FAILED=0
trap 'kill "${PIDS[@]}" 2>/dev/null; rm -rf "$LOGS"' EXIT
port() { head -1 "$1"; }
start() { # <이름> <명령...>: 첫 줄에 포트를 찍는 서버를 띄우고 그 포트를 기다린다
  local name="$1"; shift; "$@" >"$LOGS/$name.port" 2>"$LOGS/$name.err" & PIDS+=($!)
  for _ in $(seq 100); do [ -s "$LOGS/$name.port" ] && return; sleep 0.1; done; echo "$name 이 뜨지 않았다"; cat "$LOGS/$name.err"; exit 99
}
run_profile() { # <main|tight> <대기 ms> [<시험 파일>...]  (파일을 안 주면 expected.json의 그 묶음 전체)
  local profile="$1" wait_ms="$2"; shift 2
  local files=("$@") partial=--partial
  if [ ${#files[@]} -eq 0 ]; then
    partial=""
    while IFS= read -r file; do files+=("$file"); done < <("$PY" -I "$HERE/sdk_run.py" list "$profile" py)
    [ ${#files[@]} -gt 0 ] || { echo "[$profile] expected.json에 파이썬 시험 파일이 없다"; FAILED=$((FAILED + 1)); return; }
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
  echo "[$profile] 프록시: $(curl -fsS "http://127.0.0.1:$wire/__wire/summary" | cut -c1-300)"
}
if [ -n "${ONLY:-}" ]; then
  # shellcheck disable=SC2086  # PATTERNS는 공백으로 나뉜 시험 파일 이름들이다
  run_profile "$ONLY" "$([ "$ONLY" = main ] && echo 120000 || echo 3000)" ${PATTERNS:-}
else
  run_profile main 120000
  run_profile tight 3000
fi
[ "$FAILED" -eq 0 ] || exit 1

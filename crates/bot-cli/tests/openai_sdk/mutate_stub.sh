#!/usr/bin/env bash
# 계약을 일부러 어긴 대역(STUB_MUTANT)으로 SDK 시험이 그 위반을 잡는지 본다. 사용: mutate_stub.sh <venv 파이썬> <node 폴더> [변이 이름...]  (없으면 전부)
# 잡히면 «caught», 시험이 통과해 버리면 «SURVIVED»(시험이 약하다는 뜻이라 종료 코드가 0이 아니다). 기준 대역은 run_stub.sh·run_node_stub.sh가 통과해야 한다.
set -u
PY="$1"; NODE_DIR="$2"; shift 2; HERE="$(cd "$(dirname "$0")" && pwd)"
# 변이 | 파이썬 프로필 | 파이썬 시험 파일 | node 프로필(없으면 -)
CASES="any_key main test_errors.py main
no_auth_models main test_errors.py -
key_echo main test_errors.py main
list_unfiltered main test_models.py main
chat_forbidden_allowed main test_models.py main
forbidden_differs main test_models.py main
created_now main test_models.py -
no_idempotency main test_chat.py main
shared_keys main test_chat.py main
no_conflict main test_chat.py main
usage_zero_filled main test_chat.py main
n_ignored main test_chat.py -
max_tokens_accepted main test_chat.py main
echo_unknown_name main test_chat.py main
limit_as_stop main test_chat.py main
no_clip main test_chat.py main
clip_as_stop main test_chat.py main
earlier_limit_as_length main test_chat.py main
usage_partial_sum main test_usage.py main
no_logprobs main test_chat.py -
error_without_param main test_errors.py -
failed_status_500 main test_errors.py main
failed_status_429 main test_errors.py main
no_done main test_stream.py -
no_cache_control main test_stream.py -
usage_key_always main test_stream.py main
usage_chunk_unknown main test_stream.py main
approval_bypass main test_approval.py main
no_keepalive main test_heartbeat.py -
saturated_500 tight test_saturation.py tight
saturated_409 tight test_saturation.py tight
saturated_no_retry_after tight test_saturation.py tight
no_retry_marker main test_chat.py main
wait_forever tight test_wait.py tight
no_body_limit tight test_limits.py -"
WANTED=" $* "; SURVIVED=0; TOTAL=0
while read -r mutant profile pattern node; do
  [ -n "$mutant" ] || continue
  case "$WANTED" in "  ") ;; *" $mutant "*) ;; *) continue;; esac
  TOTAL=$((TOTAL + 1)); how=""
  out="$(ONLY="$profile" PATTERNS="$pattern" TAIL=400 "$HERE/run_stub.sh" "$PY" "$mutant" 2>&1)"; code=$?
  if [ "$code" -ne 0 ]; then how="python: $(echo "$out" | grep -E '^(FAIL|ERROR):' | head -2 | sed -E 's/\(.*//' | tr '\n' ' ')$(echo "$out" | grep -q '스키마 위반 있음' && echo '[스키마 위반]')"; fi
  if [ -z "$how" ] && [ "$node" != "-" ]; then
    out="$(ONLY="$node" "$HERE/run_node_stub.sh" "$PY" "$NODE_DIR" "$mutant" 2>&1)"; code=$?
    [ "$code" -ne 0 ] && how="node: $(echo "$out" | grep -E '^not ok' | head -2 | tr '\n' ' ')"
  elif [ -n "$how" ] && [ "$node" != "-" ]; then
    out="$(ONLY="$node" "$HERE/run_node_stub.sh" "$PY" "$NODE_DIR" "$mutant" 2>&1)"; code=$?
    [ "$code" -ne 0 ] && how="$how | node 도 잡음" || how="$how | node 는 못 잡음"
  fi
  if [ -n "$how" ]; then echo "caught   $mutant  ← $how"; else echo "SURVIVED $mutant"; SURVIVED=$((SURVIVED + 1)); fi
done <<< "$CASES"
echo "변이 $TOTAL 개 중 살아남은 것 $SURVIVED"
exit "$SURVIVED"

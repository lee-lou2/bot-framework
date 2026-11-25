#!/usr/bin/env bash
# 두 실행기(run_stub.sh·run_node_stub.sh)가 «필수 시험이 건너뛰어져도 통과»하지 않는지, 깨진 예시 시험으로 확인한다: 복사본에서 시험을 망가뜨리고(skip·todo·expectedFailure 추가·삭제·이름 바꿈·요청 없는 시험)
# 실행기가 **0이 아닌 종료 코드로, 그 이유 때문에** 실패해야 이 스크립트가 통과한다. 소스에 건너뛰기 표지를 심은 예시는 `sdk_run.py manifest`(소스 검사)도 같은 이유로 실패해야 한다.
# 실제 공식 SDK 환경이 필요하다(CI의 `openai-sdk` 작업과 수동 확인). 사용: check_runners.sh <venv 파이썬> <node_modules가 있는 node 폴더>
set -u
PY="$1"; NODE_DIR="$2"; HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/openai-broken.XXXXXX")"; BAD=0
trap 'rm -rf "$WORK"' EXIT
fresh() { rm -rf "$WORK/t"; mkdir "$WORK/t"; cp -R "$HERE/." "$WORK/t/"; find "$WORK/t" -name __pycache__ -prune -exec rm -rf {} +; }
expect_failure() { # <로그에 있어야 할 이유> <설명> <명령...>: 명령이 0이 아닌 코드로 끝나야 하고, **그 이유 때문에** 실패했어야 한다(엉뚱한 오류로 실패한 것은 잡은 것이 아니다)
  local reason="$1" label="$2" code; shift 2
  "$@" >"$WORK/out.log" 2>&1; code=$?
  if [ "$code" -eq 0 ]; then echo "통과해 버렸다(깨진 예시를 잡지 못함): $label"; tail -n 12 "$WORK/out.log"; BAD=$((BAD + 1))
  elif ! grep -q -- "$reason" "$WORK/out.log"; then echo "실패는 했지만 이유가 다르다(종료 코드 $code, 기대한 이유 «$reason»): $label"; tail -n 12 "$WORK/out.log"; BAD=$((BAD + 1))
  else echo "잡았다(종료 코드 $code): $label  ← $(grep -m1 -- "$reason" "$WORK/out.log" | cut -c1-100)"; fi
}
edit() { # <파일> <정규식> <바꿀 글>: 첫 일치 하나를 바꾼다(없으면 이 스크립트가 틀린 것이라 멈춘다)
  python3 - "$@" <<'PYEOF' || exit 2
import re, sys
path, pattern, repl = sys.argv[1:4]
text = open(path, encoding="utf-8").read()
new, n = re.subn(pattern, repl, text, count=1, flags=re.S | re.M)
assert n == 1, f"패턴을 찾지 못했다: {pattern}"
open(path, "w", encoding="utf-8").write(new)
PYEOF
}
py() { ONLY=main PATTERNS="$1" "$WORK/t/run_stub.sh" "$PY"; }
node_run() { ONLY=main "$WORK/t/run_node_stub.sh" "$PY" "$NODE_DIR"; }
source_check() { "$PY" -I "$WORK/t/sdk_run.py" manifest; }
PYT="$WORK/t/py/test_models.py"; NODET="$WORK/t/node/main.test.mjs"

# 먼저 망가뜨리지 않은 복사본은 통과해야 한다(그렇지 않으면 아래 «잡았다»는 아무 뜻이 없다)
fresh
if ! py test_models.py >"$WORK/base.log" 2>&1 || ! source_check >>"$WORK/base.log" 2>&1; then echo "기준(망가뜨리지 않은 시험)이 통과하지 않는다 — 아래 확인은 무의미하다"; tail -n 15 "$WORK/base.log"; exit 1; fi

# ---- 파이썬 실행기: 건너뛰기·예상된 실패·삭제·이름 바꿈·미등록·요청 0건
fresh; printf '\nclass Extra(unittest.TestCase):\n    @unittest.skip("broken example")\n    def test_added_but_skipped(self):\n        pass\n' >>"$PYT"
expect_failure "통과가 아니다(skip)" "@unittest.skip 시험 1건을 더함" py test_models.py
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$PYT" '    def test_the_list_is_sorted' '    @unittest.skip("broken example")\n    def test_the_list_is_sorted'
expect_failure "통과가 아니다(skip)" "기존 파이썬 시험 하나를 @unittest.skip" py test_models.py
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$PYT" '(    def test_the_list_is_sorted\w*\(self\):\n)' '\1        self.skipTest("broken example")\n'
expect_failure "통과가 아니다(skip)" "기존 파이썬 시험 하나가 실행 중 self.skipTest" py test_models.py
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$PYT" '    def test_the_list_is_sorted' '    @unittest.expectedFailure\n    def test_the_list_is_sorted'
expect_failure "통과가 아니다(xpass)" "기존 파이썬 시험 하나에 @unittest.expectedFailure(통과하면 뜻밖의 성공)" py test_models.py
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$PYT" '    def test_a_valid_key_that_is_not_a_member.*?(?=\n\n\n|\Z)' ''
expect_failure "기대한 시험이 통과하지 않았다" "파이썬 시험 1건 삭제(통과 수 축소; expected.json에는 남아 있다)" py test_models.py
expect_failure "시험 이름이 expected.json과 다르다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$PYT" 'def test_the_second_key_slot_of_the_same_subject_sees_the_same_models' 'def test_renamed_without_telling_the_list'
expect_failure "기대 목록에 없는 시험이 돌았다" "파이썬 시험 이름을 expected.json 모르게 바꿈" py test_models.py
fresh; printf '\nclass Idle(unittest.TestCase):\n    def test_never_sends_anything(self):\n        pass\n' >>"$PYT"
expect_failure "기대 목록에 없는 시험이 돌았다" "expected.json에 없는 시험을 파이썬 파일에 더함" py test_models.py

# ---- Node 실행기: test.skip·test.todo·옵션 skip/todo·삭제·이름 바꿈·없는 파일
fresh; printf "\ntest.skip('Z-added-skip', () => {});\n" >>"$NODET"
expect_failure "skipped 1건" "test.skip 시험 1건을 더함" node_run
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; printf "\ntest.todo('Z-added-todo');\n" >>"$NODET"
expect_failure "todo 1건" "test.todo 시험 1건을 더함" node_run
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$NODET" "^(test\\('models: [^']*'), wired" "\\1, { skip: true }, wired"
expect_failure "skipped 1건" "기존 Node 시험 하나를 { skip: true }" node_run
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$NODET" "^(test\\('models: [^']*'), wired" "\\1, { todo: true }, wired"
expect_failure "todo 1건" "기존 Node 시험 하나를 { todo: true }" node_run
expect_failure "건너뛰기 표지는 필수 시험에 둘 수 없다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$NODET" "\ntest\\('다른 주체가.*?\n\\}\\)\\);\n" "\n"
expect_failure "기대한 시험이 통과하지 않았다" "Node 시험 1건 삭제(통과 수 축소; expected.json에는 남아 있다)" node_run
expect_failure "시험 이름이 expected.json과 다르다" "  └ 같은 소스를 소스 검사도 거절" source_check
fresh; edit "$NODET" "^test\\('chat JSON: 호스트 상한" "test('이름을 바꾼 시험: 호스트 상한"
expect_failure "기대 목록에 없는 시험이 돌았다" "Node 시험 이름을 expected.json 모르게 바꿈" node_run
fresh; python3 - "$WORK/t/expected.json" <<'PYEOF'
import json, sys
path = sys.argv[1]; data = json.load(open(path)); data["suites"]["main"]["node"].append("ghost.test.mjs"); data["node"]["ghost.test.mjs"] = ["x"]
json.dump(data, open(path, "w"), ensure_ascii=False)
PYEOF
expect_failure "ghost.test.mjs" "expected.json에 있는데 파일이 없는 Node 시험" node_run

# ---- 시험이 입구에 한 번도 닿지 않고 통과하면(요청 0건) 프록시 검사가 잡는다
fresh; cat >"$PYT" <<'PYEOF'
import unittest

class Models(unittest.TestCase):
    def test_passes_without_touching_the_server(self):
        pass
PYEOF
python3 - "$WORK/t/expected.json" <<'PYEOF'
import json, sys
path = sys.argv[1]; data = json.load(open(path)); data["python"]["test_models.py"] = ["Models.test_passes_without_touching_the_server"]
json.dump(data, open(path, "w"), ensure_ascii=False)
PYEOF
expect_failure "0건이다" "입구에 한 번도 닿지 않고 통과하는 시험(프록시 요청 0건)" py test_models.py

[ "$BAD" -eq 0 ] || { echo "깨진 예시 $BAD건이 통과해 버렸다"; exit 1; }
echo "깨진 예시를 모두 잡았다"

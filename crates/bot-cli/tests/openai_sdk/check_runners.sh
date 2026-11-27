#!/usr/bin/env bash
# Verifies that run_stub.sh, run_node_stub.sh, and sdk_run.py manifest fail for the expected reason when tests are skipped, deleted, renamed, unlisted, or send zero requests.
# Usage: check_runners.sh <venv_python> <node_dir_with_node_modules>
set -u
PY="$1"; NODE_DIR="$2"; HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/openai-broken.XXXXXX")"; BAD=0
trap 'rm -rf "$WORK"' EXIT
fresh() { rm -rf "$WORK/t"; mkdir "$WORK/t"; cp -R "$HERE/." "$WORK/t/"; find "$WORK/t" -name __pycache__ -prune -exec rm -rf {} +; }
expect_failure() {
  local reason="$1" label="$2" code; shift 2
  "$@" >"$WORK/out.log" 2>&1; code=$?
  if [ "$code" -eq 0 ]; then echo "passed unexpectedly (missed broken case): $label"; tail -n 12 "$WORK/out.log"; BAD=$((BAD + 1))
  elif ! grep -q -- "$reason" "$WORK/out.log"; then echo "failed for wrong reason (exit $code, expected '$reason'): $label"; tail -n 12 "$WORK/out.log"; BAD=$((BAD + 1))
  else echo "caught (exit $code): $label  <- $(grep -m1 -- "$reason" "$WORK/out.log" | cut -c1-100)"; fi
}
edit() {
  python3 - "$@" <<'PYEOF' || exit 2
import re, sys
path, pattern, repl = sys.argv[1:4]
text = open(path, encoding="utf-8").read()
new, n = re.subn(pattern, repl, text, count=1, flags=re.S | re.M)
assert n == 1, f"pattern not found: {pattern}"
open(path, "w", encoding="utf-8").write(new)
PYEOF
}
py() { ONLY=main PATTERNS="$1" "$WORK/t/run_stub.sh" "$PY"; }
node_run() { ONLY=main "$WORK/t/run_node_stub.sh" "$PY" "$NODE_DIR"; }
source_check() { "$PY" -I "$WORK/t/sdk_run.py" manifest; }
PYT="$WORK/t/py/test_models.py"; NODET="$WORK/t/node/main.test.mjs"

# Baseline copy must pass first; otherwise downstream failures prove nothing.
fresh
if ! py test_models.py >"$WORK/base.log" 2>&1 || ! source_check >>"$WORK/base.log" 2>&1; then echo "baseline failed — aborting runner checks"; tail -n 15 "$WORK/base.log"; exit 1; fi

# Python runner: skip, expectedFailure, deletion, rename, unlisted test.
fresh; printf '\nclass Extra(unittest.TestCase):\n    @unittest.skip("broken example")\n    def test_added_but_skipped(self):\n        pass\n' >>"$PYT"
expect_failure "did not pass (skip)" "added @unittest.skip test" py test_models.py
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$PYT" '    def test_the_list_is_sorted' '    @unittest.skip("broken example")\n    def test_the_list_is_sorted'
expect_failure "did not pass (skip)" "existing Python test marked @unittest.skip" py test_models.py
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$PYT" '(    def test_the_list_is_sorted\w*\(self\):\n)' '\1        self.skipTest("broken example")\n'
expect_failure "did not pass (skip)" "existing Python test calls self.skipTest" py test_models.py
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$PYT" '    def test_the_list_is_sorted' '    @unittest.expectedFailure\n    def test_the_list_is_sorted'
expect_failure "did not pass (xpass)" "existing Python test marked @unittest.expectedFailure" py test_models.py
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$PYT" '    def test_a_valid_key_that_is_not_a_member.*?(?=\n\n\n|\Z)' ''
expect_failure "expected test did not pass" "deleted Python test still listed in expected.json" py test_models.py
expect_failure "test names differ from expected.json" "  manifest rejects same source" source_check
fresh; edit "$PYT" 'def test_the_second_key_slot_of_the_same_subject_sees_the_same_models' 'def test_renamed_without_telling_the_list'
expect_failure "unexpected test ran" "renamed Python test without updating expected.json" py test_models.py
fresh; printf '\nclass Idle(unittest.TestCase):\n    def test_never_sends_anything(self):\n        pass\n' >>"$PYT"
expect_failure "unexpected test ran" "added Python test not listed in expected.json" py test_models.py

# Node runner: test.skip, test.todo, options skip/todo, deletion, rename, missing file.
fresh; printf "\ntest.skip('Z-added-skip', () => {});\n" >>"$NODET"
expect_failure "skipped: 1" "added test.skip case" node_run
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; printf "\ntest.todo('Z-added-todo');\n" >>"$NODET"
expect_failure "todo: 1" "added test.todo case" node_run
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$NODET" "^(test\\('models: [^']*'), wired" "\\1, { skip: true }, wired"
expect_failure "skipped: 1" "existing Node test marked { skip: true }" node_run
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$NODET" "^(test\\('models: [^']*'), wired" "\\1, { todo: true }, wired"
expect_failure "todo: 1" "existing Node test marked { todo: true }" node_run
expect_failure "skip markers are forbidden in required tests" "  manifest rejects same source" source_check
fresh; edit "$NODET" "\ntest\\('isolation: .*?\n\\}\\)\\);\n" "\n"
expect_failure "expected test did not pass" "deleted Node test still listed in expected.json" node_run
expect_failure "test names differ from expected.json" "  manifest rejects same source" source_check
fresh; edit "$NODET" "^test\\('chat JSON: long normal reply" "test('renamed test: long normal reply"
expect_failure "unexpected test ran" "renamed Node test without updating expected.json" node_run
fresh; python3 - "$WORK/t/expected.json" <<'PYEOF'
import json, sys
path = sys.argv[1]; data = json.load(open(path)); data["suites"]["main"]["node"].append("ghost.test.mjs"); data["node"]["ghost.test.mjs"] = ["x"]
json.dump(data, open(path, "w"), ensure_ascii=False)
PYEOF
expect_failure "ghost.test.mjs" "missing Node test file listed in expected.json" node_run

# Proxy check catches passing tests that never hit the gateway.
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
expect_failure "0 HTTP requests" "passing test that never calls the gateway" py test_models.py

[ "$BAD" -eq 0 ] || { echo "$BAD broken case(s) passed unexpectedly"; exit 1; }
echo "all broken cases caught"

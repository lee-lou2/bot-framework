#!/usr/bin/env python3
"""공식 SDK 시험을 돌리고 «빼먹고 통과»를 막는 실행·검사기(표준 라이브러리만). 로컬 러너·Rust 종단 하네스·CI가 모두 이것으로 SDK 시험을 돌린다.
규칙: 기대 시험 이름 목록(`expected.json`)의 시험이 **전부 정확히 한 번 통과**해야 한다 — 건너뜀(skip)·예상된 실패(xfail)·뜻밖의 성공·todo·취소가 하나라도 있거나, 기대한 이름이
돌지 않았거나, 목록에 없는 시험이 끼었거나, 통과가 0건이거나, 프록시가 본 요청이 0건이면 실패다. 사람이 읽는 시험 출력은 표준 오류로, 기계가 읽는 결과 한 줄(JSON)은 표준 출력으로 낸다.

사용: sdk_run.py py <시험 파일> | node <시험 파일> --node-dir <폴더> | proxy --url <…/__wire> --profile main|tight [--scope all|python|node] [--partial] | list <프로필> py|node | manifest [--write]
(`--scope`: 프록시를 파이썬·Node가 함께 쓰면 all(기본), 따로 돌리면 그 언어 몫의 공식 스키마만 요구한다.)
옵션: --expected <경로>(기본 이 폴더의 expected.json). 종료 코드 0이면 문제 없음, 1이면 문제(이유는 표준 오류), 2는 사용 오류."""
import ast
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import unittest
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
PY_DIR, NODE_DIR = HERE / "py", HERE / "node"
TAP_LINE = re.compile(r"^(not )?ok \d+ - (.*?)(?: # (SKIP|TODO)\b.*)?$")
TAP_SUMMARY = re.compile(r"^# (tests|pass|fail|cancelled|skipped|todo) (\d+)$")


def load(path):
    return json.loads(pathlib.Path(path).read_text(encoding="utf-8"))


# ---- 순수 검사(합성 입력으로 시험한다) -----------------------------------------------------------------------------------------------------------------

def compare(expected, ok_names, label):
    """기대 이름 집합과 통과한 이름 집합을 양방향으로 대조한다."""
    problems, want, got = [], set(expected), set(ok_names)
    if len(expected) != len(want):
        problems.append(f"{label}: 기대 목록에 같은 이름이 겹쳐 있다")
    for name in sorted(want - got):
        problems.append(f"{label}: 기대한 시험이 통과하지 않았다(돌지 않았거나 건너뛰었다): {name}")
    for name in sorted(got - want):
        problems.append(f"{label}: 기대 목록에 없는 시험이 돌았다(expected.json에 등록해야 한다): {name}")
    return problems


def check_python(outcomes, expected):
    """`outcomes`는 시험 이름 → ok|fail|error|skip|xfail|xpass. `expected`는 이 시험 파일의 기대 이름 목록."""
    problems = []
    if not outcomes:
        problems.append("시험이 하나도 돌지 않았다")
    for name, outcome in sorted(outcomes.items()):
        if outcome != "ok":
            problems.append(f"통과가 아니다({outcome}): {name}")
    return problems + compare(expected, [n for n, o in outcomes.items() if o == "ok"], "python")


def parse_tap(text):
    """TAP 출력 → (이름→결과, 요약 수). 결과는 ok|fail|skip|todo. 들여쓴 하위 줄은 무시한다(시험은 평면이다)."""
    outcomes, summary = {}, {}
    for line in text.splitlines():
        found = TAP_SUMMARY.match(line)
        if found:
            summary[found.group(1)] = int(found.group(2))
            continue
        found = TAP_LINE.match(line)
        if found:
            failed, name, directive = found.groups()
            outcomes[name] = "fail" if failed else {"SKIP": "skip", "TODO": "todo"}.get(directive, "ok")
    return outcomes, summary


def check_node(outcomes, summary, returncode, expected):
    problems = []
    if returncode != 0:
        problems.append(f"node가 비정상 종료했다(종료 코드 {returncode})")
    if not outcomes:
        problems.append("시험이 하나도 돌지 않았다")
    for key in ("tests", "pass", "fail", "cancelled", "skipped", "todo"):
        if key not in summary:
            problems.append(f"TAP 요약에 `# {key}`가 없다")
    for key in ("fail", "cancelled", "skipped", "todo"):
        if summary.get(key, 0) != 0:
            problems.append(f"{key} {summary[key]}건")
    if summary.get("tests") != summary.get("pass") or summary.get("tests") != len(expected):
        problems.append(f"통과 수가 기대와 다르다: tests={summary.get('tests')} pass={summary.get('pass')} 기대={len(expected)}")
    for name, outcome in sorted(outcomes.items()):
        if outcome != "ok":
            problems.append(f"통과가 아니다({outcome}): {name}")
    return problems + compare(expected, [n for n, o in outcomes.items() if o == "ok"], "node")


def check_proxy(summary, schemas, partial):
    """와이어 프록시가 실제로 본 것: 위반 0, 요청 1건 이상, (전체 실행이면) 기대한 공식 스키마를 모두 한 번 이상 검사했다."""
    problems = []
    if summary.get("violations"):
        problems.append(f"공식 스키마·SSE 틀 위반 {len(summary['violations'])}건: {json.dumps(summary['violations'][:2], ensure_ascii=False)[:300]}")
    if not summary.get("requests"):
        problems.append("프록시가 본 HTTP 요청이 0건이다(시험이 입구에 닿지 않았다)")
    if not partial:
        for name in schemas:
            if not (summary.get("validated") or {}).get(name):
                problems.append(f"공식 스키마 {name}을 한 번도 검사하지 않았다")
    return problems


# ---- 소스에서 뽑는 시험 이름 ---------------------------------------------------------------------------------------------------------------------------

def python_names(path):
    """`test_` 메서드를 가진 클래스의 `클래스.메서드` 목록(소스 순서)."""
    tree = ast.parse(pathlib.Path(path).read_text(encoding="utf-8"))
    return [f"{cls.name}.{fn.name}" for cls in tree.body if isinstance(cls, ast.ClassDef) for fn in cls.body if isinstance(fn, ast.FunctionDef) and fn.name.startswith("test_")]


def node_names(path):
    return re.findall(r"^test\(\s*'((?:[^'\\]|\\.)*)'", pathlib.Path(path).read_text(encoding="utf-8"), re.M)


def source_manifest():
    return {"python": {p.name: python_names(p) for p in sorted(PY_DIR.glob("test_*.py"))}, "node": {p.name: node_names(p) for p in sorted(NODE_DIR.glob("*.test.mjs"))}}


def manifest_problems(expected, source):
    """소스에 있는 시험이 모두 등록돼 있고(새 시험은 의식적으로 올린다) 등록된 시험이 모두 소스에 있다(지우는 것도 의식적이다). 모든 시험 파일은 어느 묶음엔가 속한다."""
    problems = []
    for kind in ("python", "node"):
        for file in sorted(set(expected.get(kind, {})) | set(source[kind])):
            want, got = expected.get(kind, {}).get(file), source[kind].get(file)
            if want is None:
                problems.append(f"{kind} {file}: expected.json에 없는 시험 파일이다")
            elif got is None:
                problems.append(f"{kind} {file}: 소스에 없는 시험 파일이 expected.json에 있다")
            elif want != got:
                problems.append(f"{kind} {file}: 시험 이름이 expected.json과 다르다(없어진 것 {sorted(set(want) - set(got))}, 새로 생긴 것 {sorted(set(got) - set(want))})")
        used = {f for suite in expected.get("suites", {}).values() for f in suite.get(kind, [])}
        for file in sorted(set(source[kind]) - used):
            problems.append(f"{kind} {file}: 어느 묶음(suites)에도 들어 있지 않아 돌지 않는다")
        for file in sorted(used - set(source[kind])):
            problems.append(f"{kind} {file}: suites에 있는데 소스에 없다")
    return problems


# ---- 실행 ---------------------------------------------------------------------------------------------------------------------------------------------

class Recorder(unittest.TextTestResult):
    """시험마다 결과를 기록한다(소스 이름은 `모듈.클래스.메서드`에서 모듈을 뗀다)."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.outcomes = {}

    def mark(self, test, outcome):
        name = test.id().split(".", 1)[-1]
        if self.outcomes.get(name) not in (None, "ok"):
            return  # 이미 나쁜 결과가 있으면 그대로 둔다
        self.outcomes[name] = outcome

    def addSuccess(self, test):
        super().addSuccess(test)
        self.mark(test, "ok")

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self.mark(test, "fail")

    def addError(self, test, err):
        super().addError(test, err)
        self.mark(test, "error")

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        self.mark(test, "skip")

    def addExpectedFailure(self, test, err):
        super().addExpectedFailure(test, err)
        self.mark(test, "xfail")

    def addUnexpectedSuccess(self, test):
        super().addUnexpectedSuccess(test)
        self.mark(test, "xpass")

    def addSubTest(self, test, subtest, err):
        super().addSubTest(test, subtest, err)
        if err is not None:
            self.mark(test, "fail")


def run_python(file, expected, py_dir=PY_DIR):
    sys.path.insert(0, str(py_dir))
    sys.modules.pop(pathlib.Path(file).stem, None)  # 같은 이름의 시험 모듈을 다른 폴더에서 다시 돌려도 앞의 것이 남지 않게 한다
    suite = unittest.TestLoader().discover(str(py_dir), pattern=file)
    result = unittest.TextTestRunner(stream=sys.stderr, verbosity=2, resultclass=Recorder).run(suite)
    problems = check_python(result.outcomes, expected["python"].get(file, None) or [])
    if file not in expected["python"]:
        problems.insert(0, f"python {file}: expected.json에 없는 시험 파일이다")
    return result.testsRun, problems


def run_node(file, node_dir, expected):
    node = shutil.which("node")
    if node is None:
        return 0, ["node가 없다"]
    done = subprocess.run([node, "--test", "--test-reporter=tap", "--test-concurrency=1", file], cwd=node_dir, capture_output=True, text=True)
    sys.stderr.write(done.stderr)
    outcomes, summary = parse_tap(done.stdout)
    problems = check_node(outcomes, summary, done.returncode, expected["node"].get(file, []))
    if file not in expected["node"]:
        problems.insert(0, f"node {file}: expected.json에 없는 시험 파일이다")
    if problems:
        sys.stderr.write("\n".join(l for l in done.stdout.splitlines() if l.startswith(("not ok", "# ")) or l.lstrip().startswith(("error:", "expected:", "actual:", "message:", "code:"))) + "\n")
    return summary.get("tests", 0), problems


def finish(ran, problems):
    for problem in problems:
        print(f"문제: {problem}", file=sys.stderr)
    print(json.dumps({"ok": not problems, "ran": ran, "problems": problems}, ensure_ascii=False))
    return 1 if problems else 0


def main(argv):
    args, opts = [], {}
    it = iter(argv)
    for arg in it:
        if arg in ("--expected", "--node-dir", "--url", "--profile", "--scope"):
            opts[arg] = next(it, None)
        elif arg in ("--partial", "--write"):
            opts[arg] = True
        else:
            args.append(arg)
    if not args or opts.get("--expected", "") is None:
        print(__doc__, file=sys.stderr)
        return 2
    expected = load(opts.get("--expected") or HERE / "expected.json")
    mode = args[0]
    if mode == "py" and len(args) == 2:
        return finish(*run_python(args[1], expected))
    if mode == "node" and len(args) == 2 and opts.get("--node-dir"):
        return finish(*run_node(args[1], opts["--node-dir"], expected))
    if mode == "proxy" and opts.get("--url") and opts.get("--profile") in expected["schemas"] and opts.get("--scope", "all") in expected["schemas"][opts["--profile"]]:
        with urllib.request.urlopen(opts["--url"].rstrip("/") + "/summary", timeout=15) as reply:
            summary = json.load(reply)
        schemas = expected["schemas"][opts["--profile"]][opts.get("--scope", "all")]
        return finish(summary.get("requests", 0), check_proxy(summary, schemas, bool(opts.get("--partial"))))
    if mode == "list" and len(args) == 3 and args[1] in expected["suites"] and args[2] in ("py", "node"):
        print("\n".join(expected["suites"][args[1]]["python" if args[2] == "py" else "node"]))
        return 0
    if mode == "manifest":
        source = source_manifest()
        if opts.get("--write"):
            path = pathlib.Path(opts.get("--expected") or HERE / "expected.json")
            path.write_text(json.dumps({**expected, **source}, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
            return 0
        return finish(sum(len(v) for kind in source.values() for v in kind.values()), manifest_problems(expected, source))
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

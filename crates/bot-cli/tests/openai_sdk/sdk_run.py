#!/usr/bin/env python3
"""Official SDK test runner and verifier (stdlib only).
Requires every test in expected.json to pass exactly once; fails on skip, xfail, xpass, todo, cancel, missing/unlisted tests, zero passes, or zero proxied requests.
Human output goes to stderr; one machine-readable JSON result line goes to stdout.

Usage: sdk_run.py py <test_file> | node <test_file> --node-dir <dir> | proxy --url <.../__wire> --profile main|tight [--scope all|python|node] [--partial] | list <profile> py|node | manifest [--write]
Options: --expected <path> (defaults to expected.json beside this script). Exit 0 ok, 1 failure, 2 usage error."""
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


def compare(expected, ok_names, label):
    problems, want, got = [], set(expected), set(ok_names)
    if len(expected) != len(want):
        problems.append(f"{label}: duplicate test names in expected list")
    for name in sorted(want - got):
        problems.append(f"{label}: expected test did not pass (missing or skipped): {name}")
    for name in sorted(got - want):
        problems.append(f"{label}: unexpected test ran (register it in expected.json): {name}")
    return problems


def check_python(outcomes, expected):
    """`outcomes` maps test name -> ok|fail|error|skip|xfail|xpass."""
    problems = []
    if not outcomes:
        problems.append("no tests ran")
    for name, outcome in sorted(outcomes.items()):
        if outcome != "ok":
            problems.append(f"did not pass ({outcome}): {name}")
    return problems + compare(expected, [n for n, o in outcomes.items() if o == "ok"], "python")


def parse_tap(text):
    """Parses flat TAP output into (name -> ok|fail|skip|todo, summary counts); ignores indented subtests."""
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
        problems.append(f"node exited abnormally (exit code {returncode})")
    if not outcomes:
        problems.append("no tests ran")
    for key in ("tests", "pass", "fail", "cancelled", "skipped", "todo"):
        if key not in summary:
            problems.append(f"TAP summary missing '# {key}'")
    for key in ("fail", "cancelled", "skipped", "todo"):
        if summary.get(key, 0) != 0:
            problems.append(f"{key}: {summary[key]}")
    if summary.get("tests") != summary.get("pass") or summary.get("tests") != len(expected):
        problems.append(f"pass count mismatch: tests={summary.get('tests')} pass={summary.get('pass')} expected={len(expected)}")
    for name, outcome in sorted(outcomes.items()):
        if outcome != "ok":
            problems.append(f"did not pass ({outcome}): {name}")
    return problems + compare(expected, [n for n, o in outcomes.items() if o == "ok"], "node")


def check_proxy(summary, schemas, partial):
    problems = []
    if summary.get("violations"):
        problems.append(f"{len(summary['violations'])} OpenAPI/SSE schema violation(s): {json.dumps(summary['violations'][:2], ensure_ascii=False)[:300]}")
    if not summary.get("requests"):
        problems.append("proxy saw 0 HTTP requests (tests never reached the gateway)")
    if not partial:
        for name in schemas:
            if not (summary.get("validated") or {}).get(name):
                problems.append(f"official schema {name} was never validated")
    return problems


# Reject skip/focus/xfail markers in source before any runner executes.
FORBIDDEN = {
    "python": re.compile(r"@(?:unittest\.)?(?:skip\w*|expectedFailure)\b|\.skipTest\(|\braise\s+(?:unittest\.)?SkipTest\b"),
    "node": re.compile(r"\b(?:test|it|describe|suite)\.(?:skip|todo|only)\(|\{[^{}]*\b(?:skip|todo|only)\s*:|\bt\.(?:skip|todo)\("),
}
COMMENT = {"python": re.compile(r"^\s*#"), "node": re.compile(r"^\s*//")}


def forbidden_problems(kind, file, text):
    return [f"{kind} {file}:{n}: skip markers are forbidden in required tests: {line.strip()[:80]}"
            for n, line in enumerate(text.splitlines(), 1) if not COMMENT[kind].match(line) and FORBIDDEN[kind].search(line)]


def source_problems():
    problems = []
    for path in sorted(PY_DIR.glob("test_*.py")):
        problems += forbidden_problems("python", path.name, path.read_text(encoding="utf-8"))
    for path in sorted(NODE_DIR.glob("*.test.mjs")):
        problems += forbidden_problems("node", path.name, path.read_text(encoding="utf-8"))
    return problems


def python_names(path):
    tree = ast.parse(pathlib.Path(path).read_text(encoding="utf-8"))
    return [f"{cls.name}.{fn.name}" for cls in tree.body if isinstance(cls, ast.ClassDef) for fn in cls.body if isinstance(fn, ast.FunctionDef) and fn.name.startswith("test_")]


def node_names(path):
    return re.findall(r"^test\(\s*'((?:[^'\\]|\\.)*)'", pathlib.Path(path).read_text(encoding="utf-8"), re.M)


def source_manifest():
    return {"python": {p.name: python_names(p) for p in sorted(PY_DIR.glob("test_*.py"))}, "node": {p.name: node_names(p) for p in sorted(NODE_DIR.glob("*.test.mjs"))}}


def manifest_problems(expected, source):
    problems = []
    for kind in ("python", "node"):
        for file in sorted(set(expected.get(kind, {})) | set(source[kind])):
            want, got = expected.get(kind, {}).get(file), source[kind].get(file)
            if want is None:
                problems.append(f"{kind} {file}: test file missing from expected.json")
            elif got is None:
                problems.append(f"{kind} {file}: test file in expected.json missing from source")
            elif want != got:
                problems.append(f"{kind} {file}: test names differ from expected.json (removed {sorted(set(want) - set(got))}, added {sorted(set(got) - set(want))})")
        used = {f for suite in expected.get("suites", {}).values() for f in suite.get(kind, [])}
        for file in sorted(set(source[kind]) - used):
            problems.append(f"{kind} {file}: not listed in any suite (will not run)")
        for file in sorted(used - set(source[kind])):
            problems.append(f"{kind} {file}: listed in suites but missing from source")
    return problems


class Recorder(unittest.TextTestResult):
    """Records per-test outcomes keyed by Class.method."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.outcomes = {}

    def mark(self, test, outcome):
        name = test.id().split(".", 1)[-1]
        if self.outcomes.get(name) not in (None, "ok"):
            return  # Preserve first non-ok outcome (e.g. failing subtest).
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
    sys.modules.pop(pathlib.Path(file).stem, None)  # Clear cached module across runs from different temp dirs.
    suite = unittest.TestLoader().discover(str(py_dir), pattern=file)
    result = unittest.TextTestRunner(stream=sys.stderr, verbosity=2, resultclass=Recorder).run(suite)
    problems = check_python(result.outcomes, expected["python"].get(file, None) or [])
    if file not in expected["python"]:
        problems.insert(0, f"python {file}: test file missing from expected.json")
    return result.testsRun, problems


def run_node(file, node_dir, expected):
    node = shutil.which("node")
    if node is None:
        return 0, ["node not found"]
    done = subprocess.run([node, "--test", "--test-reporter=tap", "--test-concurrency=1", file], cwd=node_dir, capture_output=True, text=True)
    sys.stderr.write(done.stderr)
    outcomes, summary = parse_tap(done.stdout)
    problems = check_node(outcomes, summary, done.returncode, expected["node"].get(file, []))
    if file not in expected["node"]:
        problems.insert(0, f"node {file}: test file missing from expected.json")
    if problems:
        sys.stderr.write("\n".join(l for l in done.stdout.splitlines() if l.startswith(("not ok", "# ")) or l.lstrip().startswith(("error:", "expected:", "actual:", "message:", "code:"))) + "\n")
    return summary.get("tests", 0), problems


def finish(ran, problems):
    for problem in problems:
        print(f"problem: {problem}", file=sys.stderr)
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
        return finish(sum(len(v) for kind in source.values() for v in kind.values()), manifest_problems(expected, source) + source_problems())
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

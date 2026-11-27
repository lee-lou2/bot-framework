#!/usr/bin/env python3
"""Tests for `crates/bot-cli/tests/openai_sdk/sdk_run.py`, the runner that stops the official-SDK end-to-end tests from passing while a required scenario is skipped, missing or never ran.
Broken examples only: synthetic reports, synthetic TAP, synthetic test folders (no SDK, no server). The real expected-test list must match the real test sources."""

import importlib.util
import os
import shutil
import tempfile
import textwrap
import unittest
import unittest.mock
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SDK = ROOT / "crates/bot-cli/tests/openai_sdk"
SPEC = importlib.util.spec_from_file_location("sdk_run", SDK / "sdk_run.py")
run = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(run)

NAMES = ["A.test_one", "A.test_two"]
TAP_OK = "ok 1 - first\nok 2 - second\n# tests 2\n# pass 2\n# fail 0\n# cancelled 0\n# skipped 0\n# todo 0\n"
SUMMARY = {"tests": 2, "pass": 2, "fail": 0, "cancelled": 0, "skipped": 0, "todo": 0}


def said(problems, word):
    return any(word in p for p in problems)


class Python(unittest.TestCase):
    def test_a_full_pass_of_the_expected_tests_is_clean(self):
        self.assertEqual(run.check_python({"A.test_one": "ok", "A.test_two": "ok"}, NAMES), [])

    def test_skipped_expected_failure_unexpected_success_failure_and_error_each_fail(self):
        for outcome in ("skip", "xfail", "xpass", "fail", "error"):
            found = run.check_python({"A.test_one": "ok", "A.test_two": outcome}, NAMES)
            self.assertTrue(said(found, f"did not pass ({outcome})") and said(found, "A.test_two"), (outcome, found))

    def test_an_expected_test_that_never_ran_fails_by_name(self):
        found = run.check_python({"A.test_one": "ok"}, NAMES)
        self.assertTrue(said(found, "expected test did not pass") and said(found, "A.test_two"), found)

    def test_a_test_that_is_not_in_the_expected_list_fails_by_name(self):
        found = run.check_python({"A.test_one": "ok", "A.test_two": "ok", "A.test_extra": "ok"}, NAMES)
        self.assertTrue(said(found, "unexpected test ran") and said(found, "A.test_extra"), found)

    def test_nothing_ran_fails(self):
        self.assertTrue(said(run.check_python({}, NAMES), "no tests ran"))
        self.assertTrue(said(run.check_python({}, []), "no tests ran"), "zero passes is not a pass even with an empty expected list")

    def test_a_duplicate_name_in_the_expected_list_is_refused(self):
        self.assertTrue(said(run.check_python({"A.test_one": "ok"}, ["A.test_one", "A.test_one"]), "duplicate"))


class Node(unittest.TestCase):
    def check(self, tap, code=0, expected=("first", "second")):
        outcomes, summary = run.parse_tap(tap)
        return run.check_node(outcomes, summary, code, list(expected))

    def test_a_full_pass_is_clean(self):
        self.assertEqual(self.check(TAP_OK), [])

    def test_skip_and_todo_directives_and_their_summary_counts_fail(self):
        skipped = "ok 1 - first\nok 2 - second # SKIP not now\n# tests 2\n# pass 1\n# fail 0\n# cancelled 0\n# skipped 1\n# todo 0\n"
        found = self.check(skipped)
        self.assertTrue(said(found, "skipped: 1") and said(found, "did not pass (skip)"), found)
        todo = "ok 1 - first\nok 2 - second # TODO later\n# tests 2\n# pass 2\n# fail 0\n# cancelled 0\n# skipped 0\n# todo 1\n"
        self.assertTrue(said(self.check(todo), "todo: 1") and said(self.check(todo), "did not pass (todo)"))

    def test_a_summary_that_hides_a_skip_does_not_hide_the_directive(self):
        hidden = "ok 1 - first\nok 2 - second # SKIP\n# tests 2\n# pass 2\n# fail 0\n# cancelled 0\n# skipped 0\n# todo 0\n"
        self.assertTrue(said(self.check(hidden), "did not pass (skip)"))

    def test_zero_passes_and_a_missing_summary_fail(self):
        zero = "# tests 0\n# pass 0\n# fail 0\n# cancelled 0\n# skipped 1\n# todo 0\n"
        self.assertTrue(said(self.check(zero), "no tests ran"))
        self.assertTrue(said(self.check("ok 1 - first\nok 2 - second\n"), "TAP summary missing"))

    def test_failure_cancel_and_a_bad_exit_code_fail(self):
        failed = "ok 1 - first\nnot ok 2 - second\n# tests 2\n# pass 1\n# fail 1\n# cancelled 0\n# skipped 0\n# todo 0\n"
        self.assertTrue(said(self.check(failed), "fail: 1"))
        cancelled = "ok 1 - first\nok 2 - second\n# tests 2\n# pass 2\n# fail 0\n# cancelled 1\n# skipped 0\n# todo 0\n"
        self.assertTrue(said(self.check(cancelled), "cancelled: 1"))
        self.assertTrue(said(self.check(TAP_OK, code=1), "node exited abnormally"))

    def test_a_missing_or_unlisted_test_fails_by_name_and_a_short_pass_count_fails(self):
        found = self.check(TAP_OK, expected=("first", "second", "third"))
        self.assertTrue(said(found, "third") and said(found, "pass count mismatch"), found)
        found = self.check(TAP_OK, expected=("first",))
        self.assertTrue(said(found, "unexpected test ran") and said(found, "second"), found)

    def test_indented_subtest_lines_are_not_tests(self):
        nested = "ok 1 - first\n    ok 1 - inner\nok 2 - second\n# tests 2\n# pass 2\n# fail 0\n# cancelled 0\n# skipped 0\n# todo 0\n"
        self.assertEqual(self.check(nested), [])


class Proxy(unittest.TestCase):
    GOOD = {"violations": [], "requests": 7, "validated": {"A": 3, "B": 1}}

    def test_a_clean_run_that_saw_every_schema_is_clean(self):
        self.assertEqual(run.check_proxy(self.GOOD, ["A", "B"], partial=False), [])

    def test_no_request_at_all_fails_even_for_a_partial_run(self):
        for partial in (False, True):
            self.assertTrue(said(run.check_proxy({**self.GOOD, "requests": 0}, [], partial), "0 HTTP requests"), partial)

    def test_a_violation_fails(self):
        found = run.check_proxy({**self.GOOD, "violations": [{"schema": "A", "problems": ["x"]}]}, ["A"], partial=True)
        self.assertTrue(said(found, "1 OpenAPI/SSE schema violation(s)"), found)

    def test_a_schema_that_was_never_checked_fails_unless_the_run_is_partial(self):
        found = run.check_proxy({**self.GOOD, "validated": {"A": 3}}, ["A", "B"], partial=False)
        self.assertTrue(said(found, "official schema B was never validated"), found)
        self.assertEqual(run.check_proxy({**self.GOOD, "validated": {"A": 3}}, ["A", "B"], partial=True), [])


class Manifest(unittest.TestCase):
    SOURCE = {"python": {"test_a.py": ["A.test_one"]}, "node": {"a.test.mjs": ["first"]}}
    EXPECTED = {"suites": {"main": {"python": ["test_a.py"], "node": ["a.test.mjs"]}}, "python": {"test_a.py": ["A.test_one"]}, "node": {"a.test.mjs": ["first"]}}

    def test_matching_lists_are_clean(self):
        self.assertEqual(run.manifest_problems(self.EXPECTED, self.SOURCE), [])

    def test_a_new_test_must_be_registered_and_a_removed_one_must_be_unregistered(self):
        new = {**self.SOURCE, "python": {"test_a.py": ["A.test_one", "A.test_new"]}}
        self.assertTrue(said(run.manifest_problems(self.EXPECTED, new), "added ['A.test_new']"))
        gone = {**self.SOURCE, "python": {"test_a.py": []}}
        self.assertTrue(said(run.manifest_problems(self.EXPECTED, gone), "removed ['A.test_one']"))

    def test_a_test_file_missing_from_either_side_or_from_every_suite_is_reported(self):
        extra = {**self.SOURCE, "node": {**self.SOURCE["node"], "b.test.mjs": ["x"]}}
        found = run.manifest_problems(self.EXPECTED, extra)
        self.assertTrue(said(found, "test file missing from expected.json") and said(found, "not listed in any suite"), found)
        unlisted = {**self.EXPECTED, "suites": {"main": {"python": [], "node": ["a.test.mjs"]}}}
        self.assertTrue(said(run.manifest_problems(unlisted, self.SOURCE), "not listed in any suite (will not run)"))
        stale = {**self.EXPECTED, "python": {**self.EXPECTED["python"], "test_gone.py": []}}
        self.assertTrue(said(run.manifest_problems(stale, self.SOURCE), "test file in expected.json missing from source"))

    def test_the_real_expected_list_matches_the_real_test_sources(self):
        self.assertEqual(run.manifest_problems(run.load(SDK / "expected.json"), run.source_manifest()), [])

    def test_the_real_list_names_every_required_scenario_file(self):
        expected = run.load(SDK / "expected.json")
        for file in ("test_models.py", "test_chat.py", "test_stream.py", "test_errors.py", "test_approval.py", "test_usage.py", "test_heartbeat.py", "test_conformance.py"):
            self.assertIn(file, expected["suites"]["main"]["python"])
        for file in ("test_wait.py", "test_saturation.py", "test_limits.py"):
            self.assertIn(file, expected["suites"]["tight"]["python"])
        self.assertIn("main.test.mjs", expected["suites"]["main"]["node"])
        self.assertIn("tight.test.mjs", expected["suites"]["tight"]["node"])
        self.assertEqual(sorted(expected["schemas"]), ["main", "tight"])
        for profile, scopes in expected["schemas"].items():
            self.assertEqual(sorted(scopes), ["all", "node", "python"], profile)
            self.assertEqual(set(scopes["python"]) | set(scopes["node"]), set(scopes["all"]), f"{profile}: union must equal all")


class Source(unittest.TestCase):
    """A skip marker in the required test sources is refused by name and line, before anything runs."""

    def found(self, kind, text):
        return run.forbidden_problems(kind, "x", textwrap.dedent(text))

    def test_every_python_way_to_skip_or_excuse_a_failure_is_found(self):
        for line in ("@unittest.skip('x')", "@unittest.skipIf(True, 'x')", "@unittest.skipUnless(False, 'x')", "@unittest.expectedFailure", "@skip('x')", "self.skipTest('x')", "raise unittest.SkipTest('x')", "raise SkipTest"):
            self.assertTrue(self.found("python", f"class A(unittest.TestCase):\n    {line}\n    def test_a(self): pass\n"), line)

    def test_every_node_way_to_skip_or_focus_is_found(self):
        for line in ("test.skip('a', () => {});", "test.todo('a');", "test.only('a', () => {});", "it.skip('a', () => {});", "describe.only('a', () => {});", "test('a', { skip: true }, () => {});",
                     "test('a', { todo: 'later' }, () => {});", "test('a', { only: true }, () => {});", "t.skip('x');", "t.todo('x');"):
            self.assertTrue(self.found("node", line + "\n"), line)

    def test_ordinary_tests_and_words_inside_comments_or_strings_are_not_flagged(self):
        self.assertEqual(self.found("python", "class A(unittest.TestCase):\n    # @unittest.skip would be refused\n    def test_a(self):\n        self.assertTrue(True, 'skipped')\n"), [])
        self.assertEqual(self.found("node", "// test.skip is refused\ntest('a', wired(async () => { assert.ok(true); }));\nconst skipped = 1;\n"), [])

    def test_the_problem_names_the_file_and_the_line(self):
        found = run.forbidden_problems("node", "main.test.mjs", "test('a', () => {});\ntest.skip('b', () => {});\n")
        self.assertEqual(len(found), 1)
        self.assertIn("main.test.mjs:2", found[0])

    def test_the_real_required_test_sources_hold_no_skip_marker(self):
        self.assertEqual(run.source_problems(), [])

    def test_the_manifest_mode_itself_fails_on_a_skip_marker_and_names_it(self):
        """`sdk_run.py manifest` (called by CI and `check_runners.sh`) wires up the source check."""
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "test_x.py").write_text("import unittest\nclass X(unittest.TestCase):\n    @unittest.skip('x')\n    def test_a(self): pass\n")
            expected = Path(tmp) / "expected.json"
            expected.write_text('{"suites": {"main": {"python": ["test_x.py"], "node": []}}, "schemas": {}, "python": {"test_x.py": ["X.test_a"]}, "node": {}}')
            out = Path(tmp) / "out.txt"
            with unittest.mock.patch.object(run, "PY_DIR", Path(tmp)), unittest.mock.patch.object(run, "NODE_DIR", Path(tmp) / "none"), open(out, "w") as stdout, \
                    open(os.devnull, "w") as quiet, unittest.mock.patch("sys.stdout", stdout), unittest.mock.patch("sys.stderr", quiet):
                code = run.main(["manifest", "--expected", str(expected)])
            self.assertEqual(code, 1)
            self.assertIn("test_x.py:3", out.read_text())

    def test_the_manifest_command_reports_a_skip_marker_in_a_synthetic_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "test_x.py").write_text("import unittest\nclass X(unittest.TestCase):\n    @unittest.skip('x')\n    def test_a(self): pass\n")
            with unittest.mock.patch.object(run, "PY_DIR", Path(tmp)), unittest.mock.patch.object(run, "NODE_DIR", Path(tmp)):
                found = run.source_problems()
        self.assertTrue(said(found, "test_x.py:3"), found)


class Executed(unittest.TestCase):
    """The runner really runs a synthetic folder and refuses the broken ones (no SDK involved)."""

    def folder(self, source):
        tmp = Path(tempfile.mkdtemp(prefix="sdk-run-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "test_x.py").write_text(textwrap.dedent(source))
        return tmp

    def go(self, folder, names):
        with open(os.devnull, "w") as quiet, unittest.mock.patch("sys.stderr", quiet):
            return run.run_python("test_x.py", {"python": {"test_x.py": names}}, folder)

    GOOD = """
        import unittest
        class X(unittest.TestCase):
            def test_a(self): pass
            def test_b(self): pass
    """

    def test_a_folder_that_passes_what_is_expected_is_clean(self):
        ran, problems = self.go(self.folder(self.GOOD), ["X.test_a", "X.test_b"])
        self.assertEqual((ran, problems), (2, []))

    def test_a_skipped_test_is_refused_even_though_unittest_itself_calls_it_ok(self):
        broken = self.GOOD.replace("def test_b", "@unittest.skip('later')\n            def test_b")
        ran, problems = self.go(self.folder(broken), ["X.test_a", "X.test_b"])
        self.assertTrue(said(problems, "did not pass (skip)") and said(problems, "X.test_b"), problems)

    def test_expected_failures_and_unexpected_successes_are_refused(self):
        broken = self.GOOD.replace("def test_a(self): pass", "@unittest.expectedFailure\n            def test_a(self): self.fail('x')").replace("def test_b(self): pass", "@unittest.expectedFailure\n            def test_b(self): pass")
        _, problems = self.go(self.folder(broken), ["X.test_a", "X.test_b"])
        self.assertTrue(said(problems, "did not pass (xfail)") and said(problems, "did not pass (xpass)"), problems)

    def test_a_test_that_disappeared_or_was_added_is_refused_by_name(self):
        ran, problems = self.go(self.folder(self.GOOD), ["X.test_a", "X.test_b", "X.test_c"])
        self.assertTrue(said(problems, "X.test_c"), problems)
        _, problems = self.go(self.folder(self.GOOD), ["X.test_a"])
        self.assertTrue(said(problems, "unexpected test ran") and said(problems, "X.test_b"), problems)

    def test_a_file_that_cannot_be_imported_or_holds_no_tests_is_refused(self):
        _, problems = self.go(self.folder("import not_a_real_module_zzz\n"), ["X.test_a"])
        self.assertTrue(problems, "import errors are caught as test errors")
        _, problems = self.go(self.folder("x = 1\n"), ["X.test_a"])
        self.assertTrue(said(problems, "no tests ran"), problems)

    def test_a_file_the_list_does_not_know_is_refused(self):
        with open(os.devnull, "w") as quiet, unittest.mock.patch("sys.stderr", quiet):
            ran, problems = run.run_python("test_x.py", {"python": {}}, self.folder(self.GOOD))
        self.assertTrue(said(problems, "test file missing from expected.json"), problems)

    @unittest.skipUnless(shutil.which("node") or os.environ.get("BOT_REQUIRE_NODE") == "1", "skipped: node not found")
    def test_a_node_test_marked_skip_is_refused_by_the_real_node_runner(self):
        self.assertIsNotNone(shutil.which("node"), "BOT_REQUIRE_NODE=1 but node is missing")
        tmp = Path(tempfile.mkdtemp(prefix="sdk-run-node-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "x.test.mjs").write_text("import { test } from 'node:test';\ntest('first', () => {});\ntest('second', { skip: true }, () => {});\n")
        with open(os.devnull, "w") as quiet, unittest.mock.patch("sys.stderr", quiet):
            ran, problems = run.run_node("x.test.mjs", str(tmp), {"node": {"x.test.mjs": ["first", "second"]}})
        self.assertTrue(said(problems, "skipped: 1") or said(problems, "did not pass (skip)"), problems)

    @unittest.skipUnless(shutil.which("node") or os.environ.get("BOT_REQUIRE_NODE") == "1", "skipped: node not found")
    def test_a_node_process_that_exits_nonzero_after_all_tests_passed_is_refused(self):
        self.assertIsNotNone(shutil.which("node"), "BOT_REQUIRE_NODE=1 but node is missing")
        tmp = Path(tempfile.mkdtemp(prefix="sdk-run-node-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "x.test.mjs").write_text("import { test } from 'node:test';\ntest('first', () => {});\nprocess.on('exit', () => { process.exitCode = 1; });\n")
        with open(os.devnull, "w") as quiet, unittest.mock.patch("sys.stderr", quiet):
            ran, problems = run.run_node("x.test.mjs", str(tmp), {"node": {"x.test.mjs": ["first"]}})
        self.assertTrue(said(problems, "node exited abnormally"), problems)


if __name__ == "__main__":
    import unittest.mock  # noqa: F401  (`Executed` patches stderr)

    unittest.main()

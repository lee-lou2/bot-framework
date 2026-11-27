#!/usr/bin/env python3
"""Tests for scripts/parity_audit.py on synthetic trees, plus a smoke test on this repository."""

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("parity_audit", ROOT / "scripts/parity_audit.py")
audit_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit_module)

TREE = {
    "crates/bot-core/src/profile.rs": "enum Adapter { SlackChannel, Discord }",
    "crates/bot-profile/src/activate.rs": "match adapter { SlackChannel => 1 }",
    "crates/bot-store/migrations/0001.sql": "check (kind in ('slack'))",
    "crates/bot-gateway/src/slack/send.rs": "// Slack sender",
    "crates/bot-gateway/src/discord/send.rs": "// the new Discord sender",
    "crates/bot-gateway/src/tests/slack.rs": "slack test",
    "crates/bot-gateway/src/lib_tests.rs": "slack test",
    "crates/bot-broker/src/testing/fakes.rs": "slack fake",
    "docs/gateway.md": "Slack is configured here",
    "schemas/v1/profile.schema.json": '{"const": "SLACK"}',
    "target/debug/leftover.rs": "slack build output",
    ".git/config.json": "slack",
    "images/logo.png": "slack",
    "crates/bot-core/src/unrelated.rs": "nothing to see",
}


class Tree:
    def __init__(self, files=None):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        for name, text in (files or TREE).items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.dir.cleanup()


class Audit(unittest.TestCase):
    def test_a_file_is_missing_when_it_mentions_only_the_reference(self):
        with Tree() as tree:
            found = audit_module.audit(tree.root, "slack", "discord")
        self.assertEqual(
            found["missing"],
            [
                "crates/bot-gateway/src/slack/send.rs",
                "crates/bot-profile/src/activate.rs",
                "crates/bot-store/migrations/0001.sql",
                "docs/gateway.md",
                "schemas/v1/profile.schema.json",
            ],
        )
        self.assertEqual(found["both"], ["crates/bot-core/src/profile.rs"])
        self.assertEqual(found["only_new"], ["crates/bot-gateway/src/discord/send.rs"])

    def test_matching_ignores_case_and_finds_names_inside_longer_words(self):
        with Tree() as tree:
            found = audit_module.audit(tree.root, "SLACK", "DISCORD")
        self.assertIn("crates/bot-profile/src/activate.rs", found["missing"])  # `SlackChannel`
        self.assertIn("schemas/v1/profile.schema.json", found["missing"])  # `SLACK`

    def test_tests_test_doubles_build_output_and_vcs_folders_are_left_out_unless_asked(self):
        with Tree() as tree:
            plain = audit_module.audit(tree.root, "slack", "discord")
            with_tests = audit_module.audit(tree.root, "slack", "discord", tests=True)
        left_out = ["crates/bot-gateway/src/tests/slack.rs", "crates/bot-gateway/src/lib_tests.rs", "crates/bot-broker/src/testing/fakes.rs"]
        for path in left_out:
            self.assertNotIn(path, plain["missing"])
            self.assertIn(path, with_tests["missing"])
        for path in plain["missing"] + with_tests["missing"]:
            self.assertFalse(path.startswith(("target/", ".git/", "images/")), path)

    def test_skip_drops_paths_that_contain_the_text(self):
        with Tree() as tree:
            found = audit_module.audit(tree.root, "slack", "discord", skip=["crates/bot-gateway/src/slack/", "docs/"])
        self.assertNotIn("crates/bot-gateway/src/slack/send.rs", found["missing"])
        self.assertNotIn("docs/gateway.md", found["missing"])
        self.assertIn("crates/bot-profile/src/activate.rs", found["missing"])

    def test_the_report_groups_by_crate_and_says_how_many(self):
        with Tree() as tree:
            text = audit_module.report(audit_module.audit(tree.root, "slack", "discord"), "slack", "discord")
        self.assertIn("5 missing candidate(s)", text)
        self.assertIn("  crates/bot-gateway (1)\n    crates/bot-gateway/src/slack/send.rs", text)
        self.assertIn("  docs (1)", text)
        self.assertIn("1 file(s) mentioning `discord` only", text)


class Command(unittest.TestCase):
    def run_main(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = audit_module.main(list(args))
            except SystemExit as stop:
                code = stop.code
        return code, out.getvalue()

    def test_it_only_reports_unless_strict(self):
        with Tree() as tree:
            code, text = self.run_main("slack", "discord", "--root", str(tree.root))
            self.assertEqual(code, 0)
            self.assertIn("missing candidate(s)", text)
            self.assertEqual(self.run_main("slack", "discord", "--root", str(tree.root), "--strict")[0], 1)

    def test_strict_passes_when_nothing_is_missing(self):
        with Tree({"a.rs": "slack discord"}) as tree:
            self.assertEqual(self.run_main("slack", "discord", "--root", str(tree.root), "--strict")[0], 0)

    def test_bad_arguments_exit_with_2(self):
        with Tree() as tree:
            self.assertEqual(self.run_main("slack", "SLACK", "--root", str(tree.root))[0], 2)
            self.assertEqual(self.run_main("slack", " ", "--root", str(tree.root))[0], 2)
            self.assertEqual(self.run_main("slack", "discord", "--root", str(tree.root / "missing"))[0], 2)


class Repository(unittest.TestCase):
    def test_the_real_repository_lists_its_slack_files_without_test_code(self):
        found = audit_module.audit(ROOT, "slack", "no-such-channel-name")
        for path in ("schemas/v1/profile.schema.json", "docs/gateway.md", "crates/bot-gateway/src/entrance.rs"):
            self.assertIn(path, found["missing"])
        self.assertFalse([path for path in found["missing"] if audit_module.is_test_path(path)])
        self.assertEqual(found["both"] + found["only_new"], [])


if __name__ == "__main__":
    unittest.main()

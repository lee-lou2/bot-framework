#!/usr/bin/env python3
"""Tests for the CI workflow hardening and Rust toolchain pin rules in .github/scripts/check_contracts.py. Only synthetic text is used,
except the tests that hold the repository's real workflows and toolchain file to the same rules."""

import contextlib
import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("check_contracts", ROOT / ".github/scripts/check_contracts.py")
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

SHA = "0123456789abcdef0123456789abcdef01234567"
DIGEST = "sha256:" + "ab" * 32

GOOD = f"""\
name: Synthetic
on: [push, pull_request]
permissions:
  contents: read
jobs:
  verify:
    runs-on: ubuntu-24.04
    timeout-minutes: 30
    services: &services
      db:
        image: postgres:16@{DIGEST} # 16.16
    steps:
      - uses: actions/checkout@{SHA} # v4.4.0
        with:
          persist-credentials: false
      - run: echo ok
  second:
    runs-on: ubuntu-22.04
    timeout-minutes: 10
    services: *services
    steps:
      - uses: actions/checkout@{SHA} # v4.4.0
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: actions/setup-node@{SHA} # v4.4.0
        with:
          node-version: '24.22.3'
"""

GOOD_TOOLCHAIN = '[toolchain]\nchannel = "1.98.1"\ncomponents = ["rustfmt", "clippy"]\nprofile = "minimal"\n'


def problems(text):
    return checker.workflow_problems(text)


def swap(old, new, text=GOOD, count=1):
    assert text.count(old) >= 1, f"fixture drifted: {old!r}"
    return text.replace(old, new) if count is None else text.replace(old, new, count)


class Pinning(unittest.TestCase):
    def test_a_workflow_that_keeps_every_rule_passes(self):
        self.assertEqual(problems(GOOD), [])

    def test_a_tag_or_branch_is_not_a_pin(self):
        for ref in ("actions/checkout@v4", "actions/checkout@v4.4.0", "actions/checkout@main", "actions/checkout"):
            found = problems(swap(f"actions/checkout@{SHA}", ref))
            self.assertTrue(any("not pinned" in p for p in found), (ref, found))

    def test_a_short_or_upper_case_sha_is_not_a_pin(self):
        for ref in (SHA[:39], SHA + "0", SHA.upper()):
            found = problems(swap(f"actions/checkout@{SHA}", f"actions/checkout@{ref}"))
            self.assertTrue(any("not pinned" in p for p in found), (ref, found))

    def test_the_pin_needs_a_version_comment(self):
        found = problems(swap(f"@{SHA} # v4.4.0", f"@{SHA}"))
        self.assertTrue(any("comment naming its version" in p for p in found), found)
        found = problems(swap(f"@{SHA} # v4.4.0", f"@{SHA} # latest"))
        self.assertTrue(any("comment naming its version" in p for p in found), found)
        # `v` is optional and a two-part version is fine
        self.assertEqual(problems(swap("# v4.4.0", "# 4.4")), [])

    def test_a_quoted_reference_is_checked_too(self):
        found = problems(swap(f"actions/checkout@{SHA}", "'actions/checkout@v4'"))
        self.assertTrue(any("not pinned" in p for p in found), found)
        self.assertEqual(problems(swap(f"actions/checkout@{SHA}", f"'actions/checkout@{SHA}'")), [])

    def test_a_local_action_and_a_digest_pinned_image_need_no_sha(self):
        extra = "      - uses: ./.github/actions/local\n"
        self.assertEqual(problems(swap("      - run: echo ok\n", extra)), [])
        digest = f"      - uses: docker://ghcr.io/example/tool@{DIGEST} # v1.2.3\n"
        self.assertEqual(problems(swap("      - run: echo ok\n", digest)), [])
        # 판본 주석은 다이제스트에도 필요하다
        bare = f"      - uses: docker://ghcr.io/example/tool@{DIGEST}\n"
        self.assertTrue(any("comment naming" in p for p in problems(swap("      - run: echo ok\n", bare))))
        tagged = "      - uses: docker://ghcr.io/example/tool:1.2.3\n"
        self.assertTrue(any("not pinned" in p for p in problems(swap("      - run: echo ok\n", tagged))))

    def test_a_reusable_workflow_call_is_checked_like_an_action(self):
        text = GOOD + "  call:\n    uses: octo/repo/.github/workflows/x.yml@v1\n"
        self.assertTrue(any("not pinned" in p for p in problems(text)), problems(text))


class Permissions(unittest.TestCase):
    def test_the_top_level_must_be_exactly_contents_read(self):
        for replacement in (
            "permissions:\n  contents: write\n",
            "permissions: write-all\n",
            "permissions: read-all\n",
            "permissions:\n  contents: read\n  pull-requests: write\n",
            "permissions:\n  contents: read\n  id-token: read\n",
            "",
        ):
            found = problems(swap("permissions:\n  contents: read\n", replacement))
            self.assertTrue(any("top-level `permissions`" in p for p in found), (replacement, found))

    def test_a_job_may_only_narrow(self):
        narrow = "    permissions:\n      contents: read\n"
        self.assertEqual(problems(swap("    timeout-minutes: 30\n", "    timeout-minutes: 30\n" + narrow)), [])
        none = "    permissions: {}\n"
        self.assertEqual(problems(swap("    timeout-minutes: 30\n", "    timeout-minutes: 30\n" + none)), [])
        for wider in ("    permissions:\n      contents: write\n", "    permissions: write-all\n", "    permissions: read-all\n"):
            found = problems(swap("    timeout-minutes: 30\n", "    timeout-minutes: 30\n" + wider))
            self.assertTrue(any("only narrow" in p for p in found), (wider, found))


class Hardening(unittest.TestCase):
    def test_every_job_needs_a_bounded_timeout(self):
        found = problems(swap("    timeout-minutes: 30\n", ""))
        self.assertTrue(any("job verify" in p and "timeout-minutes" in p for p in found), found)
        for bad in ("360", "0", "-1", "true", "'30'", "null"):
            found = problems(swap("timeout-minutes: 30", f"timeout-minutes: {bad}"))
            self.assertTrue(any("timeout-minutes" in p for p in found), (bad, found))
        self.assertEqual(problems(swap("timeout-minutes: 30", "timeout-minutes: 120")), [])

    def test_checkout_must_not_keep_the_token_in_the_checkout(self):
        for replacement in ("", "        with:\n          fetch-depth: 0\n", "        with:\n          persist-credentials: true\n"):
            text = swap("        with:\n          persist-credentials: false\n", replacement)
            self.assertTrue(any("persist-credentials" in p for p in problems(text)), (replacement, problems(text)))

    def test_floating_runner_images_are_refused_but_a_named_one_is_fine(self):
        for label in ("ubuntu-latest", "macos-latest", "windows-latest"):
            found = problems(swap("ubuntu-24.04", label))
            self.assertTrue(any("floating runner" in p for p in found), (label, found))

    def test_secrets_tokens_and_fork_triggers_are_refused(self):
        trigger = swap("on: [push, pull_request]", "on:\n  pull_request_target:\n    branches: [main]")
        self.assertTrue(any("pull_request_target" in p for p in problems(trigger)), problems(trigger))
        use = swap("      - run: echo ok\n", "      - run: echo ${{ secrets.DEPLOY_VALUE }}\n")
        self.assertTrue(any("no secrets" in p for p in problems(use)), problems(use))
        token = swap("      - run: echo ok\n", "      - run: echo \"$GITHUB_TOKEN\"\n")
        self.assertTrue(any("no token" in p for p in problems(token)), problems(token))

    def test_comments_may_mention_the_forbidden_words(self):
        text = swap(
            "name: Synthetic\n",
            "name: Synthetic  # no pull_request_target, no secrets.X, no GITHUB_TOKEN, never ubuntu-latest\n",
        )
        self.assertEqual(problems(text), [])

    def test_similar_names_are_not_confused_with_the_forbidden_ones(self):
        harmless = "      - run: echo $GITHUB_PATH $GITHUB_ENV secrets-tree secrets-history\n"
        self.assertEqual(problems(swap("      - run: echo ok\n", harmless)), [])

    def test_each_problem_names_its_line_or_job(self):
        found = problems(swap("ubuntu-24.04", "ubuntu-latest"))
        self.assertEqual(len(found), 1, found)
        self.assertTrue(found[0].startswith("line "), found)

    def test_broken_input_is_reported_instead_of_crashing(self):
        self.assertEqual(problems("a: [unclosed\n"), ["not valid YAML"])
        self.assertEqual(problems("- just\n- a list\n"), ["not a workflow mapping"])
        self.assertEqual(problems("permissions:\n  contents: read\njobs: {x: 5}\n"), ["job x: not a mapping"])


class Tools(unittest.TestCase):
    def test_an_image_must_be_pinned_to_a_digest_and_say_which_version_it_is(self):
        pinned = f"postgres:16@{DIGEST} # 16.16"
        self.assertEqual(problems(GOOD), [])
        for ref in ("postgres:16", "postgres", "postgres:16@sha256:abc", f"postgres:16@sha256:{'AB' * 32}", f"postgres@sha256:{'ab' * 31}"):
            found = problems(swap(pinned, f"{ref} # 16.16"))
            self.assertTrue(any("not pinned to a digest" in p for p in found), (ref, found))
        found = problems(swap(pinned, f"postgres:16@{DIGEST}"))
        self.assertTrue(any("comment naming its version" in p for p in found), found)
        found = problems(swap(pinned, f"postgres:16@{DIGEST} # latest"))
        self.assertTrue(any("comment naming its version" in p for p in found), found)

    def test_a_container_image_is_held_to_the_same_rule(self):
        text = swap("    steps:\n", "    container:\n      image: node:24\n    steps:\n")
        self.assertTrue(any("`image: node:24`" in p for p in problems(text)), problems(text))

    def test_setup_node_needs_an_exact_release(self):
        for bad in ("'24'", "'24.22'", "24", "'lts/*'", "'24.x'", "'>=24.22.3'"):
            found = problems(swap("node-version: '24.22.3'", f"node-version: {bad}"))
            self.assertTrue(any("exact `node-version`" in p for p in found), (bad, found))
        found = problems(swap("        with:\n          node-version: '24.22.3'\n", ""))
        self.assertTrue(any("exact `node-version`" in p for p in found), found)
        # quoted or not, a three-part release is fine
        self.assertEqual(problems(swap("node-version: '24.22.3'", "node-version: 24.22.3")), [])


class Toolchain(unittest.TestCase):
    def test_a_pinned_release_with_rustfmt_and_clippy_passes(self):
        self.assertEqual(checker.toolchain_problems(GOOD_TOOLCHAIN), [])
        extra = GOOD_TOOLCHAIN.replace('"clippy"]', '"clippy", "rust-src"]')
        self.assertEqual(checker.toolchain_problems(extra), [])

    def test_a_moving_channel_or_a_loose_version_is_refused(self):
        for channel in ("stable", "beta", "nightly", "nightly-2026-01-01", "1.98", "1", "1.98.x", "^1.98.1", "1.98.1-beta"):
            found = checker.toolchain_problems(GOOD_TOOLCHAIN.replace("1.98.1", channel))
            self.assertTrue(any("exact release" in p for p in found), (channel, found))
        no_channel = GOOD_TOOLCHAIN.replace('channel = "1.98.1"\n', "")
        self.assertTrue(any("exact release" in p for p in checker.toolchain_problems(no_channel)))

    def test_the_components_ci_needs_must_be_listed(self):
        for components in ('["rustfmt"]', '["clippy"]', "[]", '"rustfmt"'):
            text = GOOD_TOOLCHAIN.replace('["rustfmt", "clippy"]', components)
            self.assertTrue(any("`components` must include" in p for p in checker.toolchain_problems(text)), components)
        without = GOOD_TOOLCHAIN.replace('components = ["rustfmt", "clippy"]\n', "")
        found = checker.toolchain_problems(without)
        self.assertTrue(any("rustfmt" in p and "clippy" in p for p in found), found)

    def test_broken_files_are_reported_instead_of_crashing(self):
        self.assertEqual(checker.toolchain_problems("[toolchain\n"), ["not valid TOML"])
        self.assertEqual(checker.toolchain_problems('channel = "1.98.1"\n'), ["missing the [toolchain] table"])
        self.assertEqual(checker.toolchain_problems("toolchain = 5\n"), ["missing the [toolchain] table"])

    def test_the_file_must_exist_and_the_repositorys_own_file_keeps_the_rules(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.chdir(folder):
            with self.assertRaises(ValueError) as caught:
                checker.check_toolchain()
            self.assertIn("missing", str(caught.exception))
            Path("rust-toolchain.toml").write_text(GOOD_TOOLCHAIN, encoding="utf-8")
            checker.check_toolchain()
            Path("rust-toolchain.toml").write_text(GOOD_TOOLCHAIN.replace("1.98.1", "stable"), encoding="utf-8")
            with self.assertRaises(ValueError) as caught:
                checker.check_toolchain()
            self.assertIn("exact release", str(caught.exception))
        self.assertEqual(checker.toolchain_problems((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")), [])


class Leftovers(unittest.TestCase):
    def check(self, files, ignore=None):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        subprocess.run(["git", "init", "-q"], cwd=folder.name, check=True)
        if ignore:
            Path(folder.name, ".gitignore").write_text(ignore, encoding="utf-8")
        for name in files:
            Path(folder.name, name).write_text("fn x() {}\n", encoding="utf-8")
        with contextlib.chdir(folder.name):
            checker.check_files()

    def test_an_editor_or_merge_leftover_in_the_public_list_is_refused_by_name(self):
        for name in ("secret_sweep.rs.bak", "main.orig", "patch.rej", ".lib.rs.swp", "notes.txt~"):
            with self.assertRaises(ValueError) as caught:
                self.check(["fine.rs", name])
            self.assertIn(name, str(caught.exception))

    def test_names_that_only_look_similar_are_fine(self):
        self.check(["backup.rs", "bak.txt", "original.md", "reject.rs", "swap.rs", "a~b.txt", "x.bak.rs", "nobak", "myorig", "norej", "noswp"])

    def test_an_ignored_leftover_is_not_published_so_it_is_fine(self):
        self.check(["fine.rs", "old.rs.bak"], ignore="*.bak\n")


class Folder(unittest.TestCase):
    def run_in(self, files):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        for name, text in files.items():
            path = Path(folder.name, ".github/workflows", name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        with contextlib.chdir(folder.name):
            checker.check_workflows()

    def test_a_folder_without_workflows_has_nothing_to_check(self):
        self.run_in({})

    def test_every_workflow_file_is_checked_and_named_in_the_failure(self):
        self.run_in({"good.yml": GOOD, "also.yaml": GOOD})
        with self.assertRaises(ValueError) as caught:
            self.run_in({"good.yml": GOOD, "bad.yaml": swap("ubuntu-24.04", "ubuntu-latest")})
        message = str(caught.exception)
        self.assertIn("bad.yaml", message)
        self.assertNotIn("good.yml", message)
        self.assertIn("floating runner", message)

    def test_all_problems_of_all_files_are_listed_at_once(self):
        with self.assertRaises(ValueError) as caught:
            self.run_in(
                {
                    "a.yml": swap("ubuntu-24.04", "ubuntu-latest"),
                    "b.yml": swap("    timeout-minutes: 30\n", ""),
                }
            )
        message = str(caught.exception)
        self.assertIn("a.yml", message)
        self.assertIn("b.yml", message)

    def test_the_repositorys_own_workflows_keep_the_rules(self):
        workflows = ROOT / ".github/workflows"
        files = sorted([*workflows.glob("*.yml"), *workflows.glob("*.yaml")])
        self.assertTrue(files, "no workflow found")
        for path in files:
            self.assertEqual(problems(path.read_text(encoding="utf-8")), [], path.name)


class Entry(unittest.TestCase):
    def test_the_script_runs_every_check(self):
        with (
            mock.patch.object(checker, "check_files") as files,
            mock.patch.object(checker, "check_examples") as examples,
            mock.patch.object(checker, "check_workflows") as workflows,
            mock.patch.object(checker, "check_toolchain") as toolchain,
        ):
            checker.main()
        for called in (files, examples, workflows, toolchain):
            called.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()

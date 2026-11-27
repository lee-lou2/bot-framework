#!/usr/bin/env python3
"""Tests for the `integration` step of release-check.sh: required mode, the official SDK environment, skip markers, zero-test files and failures.
`cargo` and the real tools (claude, srt, container, node, psql) are replaced by recording shims; only synthetic data and throwaway git repositories are used."""

import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testing_repo import committed, everything  # noqa: E402


class Integration(unittest.TestCase):
    """Required integration step tests using recording shims (no real VM or model calls)."""

    CARGO = """#!/bin/sh
echo "cargo-called $*"
# The step captures test output into a log file; record observed env separately.
echo "$* env claude=$BOT_CLAUDE_CANARY srt=$BOT_SRT_CANARY container=$BOT_CONTAINER_CANARY db=$BOT_TEST_REQUIRE_DB node=$BOT_REQUIRE_NODE openai=$BOT_OPENAI_CANARY sdk=$BOT_OPENAI_SDK_DIR" >> "$SHIM_LOG"
case "$SHIM_MODE" in
  skip) case "$*" in *claude_route*) echo "skipped: claude CLI not found" ;; esac ;;
  skipping) case "$*" in *private_serve*) echo "skipping: srt or python3 not found" ;; esac ;;
  zero) case "$*" in *private_serve*) echo "test result: ok. 0 passed; 0 failed; 0 ignored"; exit 0 ;; esac ;;
  fail) case "$*" in *vm_serve*) echo "test result: FAILED. 0 passed; 1 failed"; exit 101 ;; esac ;;
  sdk-skip) case "$*" in *openai_compat*) echo "skipped: BOT_OPENAI_SDK_DIR not set, skipping official SDK tests" ;; esac ;;
esac
# Match the list's #[ignore] budget (split_roles has 1 CI-only test) unless overridden by mode.
ignored=0
case "$*" in *split_roles*) ignored=1 ;; esac
case "$SHIM_MODE" in
  ignore) case "$*" in *openai_compat*) ignored=1 ;; esac ;;
  fewer) case "$*" in *split_roles*) ignored=0 ;; esac ;;
esac
echo "test result: ok. 2 passed; 0 failed; $ignored ignored; 0 measured; 0 filtered out"
"""

    def shims(self, *, missing=(), container_up=True, image_present=True):
        shim = Path(tempfile.mkdtemp(prefix="gate-shim-"))
        self.addCleanup(lambda: shutil.rmtree(shim, ignore_errors=True))
        bodies = {
            "psql": "#!/bin/sh\nexit 0\n",
            "claude": "#!/bin/sh\nexit 0\n",
            "srt": "#!/bin/sh\nexit 0\n",
            "node": "#!/bin/sh\nexit 0\n",
            "python3": f'#!/bin/sh\nexec "{sys.executable}" "$@"\n',
            "container": '#!/bin/sh\ncase "$1 $2" in\n  "system status") exit %d ;;\n  "image inspect") exit %d ;;\nesac\nexit 0\n'
            % (0 if container_up else 1, 0 if image_present else 1),
            "cargo": self.CARGO,
        }
        for name, body in bodies.items():
            if name in missing:
                continue
            (shim / name).write_text(body)
            (shim / name).chmod(0o755)
        return shim

    def sdk_setup(self, shim):
        """Shim for openai-sdk-setup.sh: records invocation and creates the target directory (no network)."""
        script = shim / "sdk-setup"
        script.write_text('#!/bin/sh\necho "sdk-setup $*" >> "$SHIM_LOG"\nmkdir -p "$1"\nexit "${SETUP_EXIT:-0}"\n')
        script.chmod(0o755)
        return script

    def run_step(self, *args, mode="", only="integration", shim=None, extra=None, image="img@sha256:" + "0" * 64):
        shim = shim or self.shims()
        repo = committed(self)
        handle, name = tempfile.mkstemp(prefix="gate-shim-log-")
        os.close(handle)
        self.log = Path(name)
        # Bind each log path directly so multiple run_step calls in one test all clean up.
        self.addCleanup(self.log.unlink, missing_ok=True)
        env = {
            "BOT_TEST_DATABASE_URL": "postgres://nobody-quokka@127.0.0.1:1/none",
            "PATH": f"{shim}:/usr/bin:/bin",
            "SHIM_MODE": mode,
            "SHIM_LOG": str(self.log),
            "RELEASE_CHECK_SDK_SETUP": str(self.sdk_setup(shim)),
            **({"BOT_MODEL_IMAGE": image} if image else {}),
            **(extra or {}),
        }
        return repo.gate(*args, "--only", only, env=env)

    def test_it_is_a_named_step(self):
        self.assertIn("integration", committed(self).gate("--list").stdout.split())

    def test_every_integration_test_runs_in_required_mode_and_the_step_passes(self):
        done = self.run_step("--strict")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        for target in ("claude_route", "srt_canary", "container_firewall", "vm_serve", "private_serve", "secret_sweep", "tool_secrets"):
            self.assertRegex(done.stdout, rf"PASS\s+{target}: 2 passed", target)
        # Ignore header `cargo --version` call; inspect only test invocations.
        seen = [line for line in self.log.read_text().splitlines() if " --test " in line]
        script = Path(__file__).with_name("release-check.sh").read_text(encoding="utf-8")
        listed = re.search(r"^INTEGRATION_TESTS='\n(.*?)^'$", script, re.S | re.M)
        self.assertIsNotNone(listed, "INTEGRATION_TESTS list not found")
        expected = [line.split()[1] for line in listed.group(1).splitlines() if line.strip()]
        self.assertEqual([re.search(r" --test (\S+)", line).group(1) for line in seen], expected, seen)
        required = (
            "claude_canary srt_canary cli_started cli_resume cli_refusal container_canary container_anchor container_claude "
            "container_firewall claude_route claude_wiring claude_stale claude_attempts claude_schedule claude_shutdown "
            "vm_serve private_serve tool_secrets split_roles secret_sweep openai_compat"
        ).split()
        self.assertEqual(sorted(set(required) - set(expected)), [], "required integration test missing from list")
        for line in seen:
            self.assertRegex(line, r"env claude=required srt=required container=required db=1 node=1 openai=required sdk=/\S+/openai-sdk$", f"not in required mode: {line}")
            self.assertIn("-- --nocapture --test-threads=1", line)
        made = [line for line in self.log.read_text().splitlines() if line.startswith("sdk-setup ")]
        self.assertEqual(len(made), 1, made)
        self.assertRegex(made[0], r"^sdk-setup /\S+/openai-sdk$")
        self.assertLess(self.log.read_text().index("sdk-setup"), self.log.read_text().index(" --test "), "SDK setup must precede tests")
        self.assertTrue(any("--features testing --test claude_canary" in line for line in seen))
        for hidden in ("nobody-quokka", "127.0.0.1:1", "0" * 64):
            self.assertNotIn(hidden, everything(done), "URLs and image digests must not be printed")

    def test_a_skip_marker_in_any_test_output_fails_the_step_and_names_the_file(self):
        for mode, target in (("skip", "claude_route"), ("skipping", "private_serve")):
            done = self.run_step("--strict", mode=mode)
            self.assertEqual(done.returncode, 1, done.stdout)
            self.assertRegex(done.stdout, rf"FAIL\s+{target}: 1 skip marker\(s\)", mode)

    def test_an_ignored_test_in_a_listed_file_fails_the_step_and_names_the_file(self):
        done = self.run_step("--strict", mode="ignore")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL\s+openai_compat: 1 #\[ignore\] test\(s\) \(list allows 0\)")
        self.assertRegex(done.stdout, r"PASS\s+split_roles: 2 passed", "allowed #[ignore] count still passes")

    def test_a_list_that_allows_more_ignored_tests_than_the_run_shows_fails_too(self):
        done = self.run_step("--strict", mode="fewer")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL\s+split_roles: 0 #\[ignore\] test\(s\) \(list allows 1\)")

    def test_a_file_where_no_test_ran_or_a_test_failed_fails_the_step(self):
        done = self.run_step("--strict", mode="zero")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL\s+private_serve: no tests ran")
        done = self.run_step("--strict", mode="fail")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL\s+vm_serve \(exit code 101\)")

    def test_a_missing_environment_fails_before_any_test_runs(self):
        cases = {
            "claude": "claude",
            "srt": "srt",
            "node": "node",
            "container": "container",
        }
        for tool, word in cases.items():
            done = self.run_step("--strict", shim=self.shims(missing=(tool,)))
            self.assertEqual(done.returncode, 1, (tool, done.stdout))
            self.assertIn(f"missing required tool: {word}", done.stdout)
            self.assertNotIn("cargo-called", done.stdout, tool)
        done = self.run_step("--strict", shim=self.shims(container_up=False))
        self.assertEqual(done.returncode, 1)
        self.assertIn("container service is not running", done.stdout)
        done = self.run_step("--strict", shim=self.shims(image_present=False))
        self.assertEqual(done.returncode, 1)
        self.assertIn("not present locally", done.stdout)
        for image in ("", "name:tag-only"):
            done = self.run_step("--strict", image=image)
            self.assertEqual(done.returncode, 1, image)
            self.assertIn("BOT_MODEL_IMAGE", done.stdout)
            self.assertNotIn("cargo-called", done.stdout)
        done = self.run_step("--strict", extra={"BOT_TEST_DATABASE_URL": ""})
        self.assertEqual(done.returncode, 1)
        self.assertIn("BOT_TEST_DATABASE_URL is not set", done.stdout)
        self.assertNotIn("cargo-called", done.stdout)

    def test_the_official_sdk_environment_is_prepared_first_and_a_failure_stops_before_any_test(self):
        done = self.run_step("--strict", extra={"SETUP_EXIT": "1"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("failed to prepare official OpenAI SDK test environment", done.stdout + self.log.read_text() + everything(done))
        self.assertNotIn("cargo-called", done.stdout, "no tests run when SDK setup fails")

    def test_a_relative_sdk_folder_is_refused_and_a_given_absolute_one_is_used_for_setup_and_tests(self):
        done = self.run_step("--strict", extra={"BOT_OPENAI_SDK_DIR": "relative/sdk"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("must be an absolute path", done.stdout)
        self.assertNotIn("cargo-called", done.stdout)
        folder = Path(tempfile.mkdtemp(prefix="gate-sdk-"))
        self.addCleanup(shutil.rmtree, folder, ignore_errors=True)
        done = self.run_step("--strict", extra={"BOT_OPENAI_SDK_DIR": str(folder / "env")})
        self.assertEqual(done.returncode, 0, done.stdout)
        text = self.log.read_text()
        self.assertIn(f"sdk-setup {folder / 'env'}", text)
        self.assertIn(f"sdk={folder / 'env'}", text)

    def test_a_skip_marker_from_the_sdk_end_to_end_test_fails_the_step(self):
        done = self.run_step("--strict", mode="sdk-skip")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL\s+openai_compat: 1 skip marker\(s\)")

    def test_a_full_run_without_strict_shows_the_step_as_a_skip_and_strict_never_skips_it(self):
        repo = committed(self)
        # Prevent recursive selftest inside the inner gate run.
        env = {"PATH": f"{self.shims()}:/usr/bin:/bin", "RELEASE_CHECK_IN_SELFTEST": "1"}
        done = repo.gate(env=env)
        self.assertRegex(done.stdout, r"SKIP\s+integration\s+\d+s\s+runs only in --strict")
        self.assertNotIn("+ cargo test --locked -p bot-broker", done.stdout)
        done = self.run_step("--strict")
        self.assertEqual(done.returncode, 0, done.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=1)

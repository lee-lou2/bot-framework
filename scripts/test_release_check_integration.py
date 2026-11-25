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
    """필수 통합 시험 단계. `cargo`와 실제 도구(claude·srt·container·node·psql)를 기록 대역으로 바꿔, 환경이 없으면 실패하고 건너뜀 표지·통과 0건·시험 실패를
    그냥 지나치지 않으며 필수 모드로 돌리는지 본다(실제 VM·모델 호출은 없다)."""

    CARGO = """#!/bin/sh
echo "cargo-called $*"
# 단계는 시험 출력을 파일로 받아 요약만 찍는다: 대역이 본 환경은 따로 남긴다
echo "$* env claude=$BOT_CLAUDE_CANARY srt=$BOT_SRT_CANARY container=$BOT_CONTAINER_CANARY db=$BOT_TEST_REQUIRE_DB node=$BOT_REQUIRE_NODE openai=$BOT_OPENAI_CANARY sdk=$BOT_OPENAI_SDK_DIR" >> "$SHIM_LOG"
case "$SHIM_MODE" in
  skip) case "$*" in *claude_route*) echo "skipped: claude CLI not found" ;; esac ;;
  korean) case "$*" in *private_serve*) echo "srt 또는 python3가 없어 건너뜀" ;; esac ;;
  zero) case "$*" in *private_serve*) echo "test result: ok. 0 passed; 0 failed; 0 ignored"; exit 0 ;; esac ;;
  fail) case "$*" in *vm_serve*) echo "test result: FAILED. 0 passed; 1 failed"; exit 101 ;; esac ;;
  sdk-skip) case "$*" in *openai_compat*) echo "skipped: BOT_OPENAI_SDK_DIR가 없어 공식 SDK 시험을 건너뜀" ;; esac ;;
esac
# 목록이 허용한 만큼만 #[ignore]다(split_roles는 CI 전용 시험 하나). 모드로 어긋나게 한다.
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
        """공식 SDK 환경을 만드는 스크립트의 대역: 부른 사실과 인자만 기록하고 폴더를 만든다(실제 설치·네트워크 없음). `SETUP_EXIT`로 실패시킨다."""
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
        # 호출마다 자기 파일을 지운다(`lambda: self.log…`는 정리 때의 마지막 값만 봐서, 한 시험이 `run_step`을 여러 번 부르면 앞의 파일이 임시 폴더에 남았다)
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
            self.assertRegex(done.stdout, rf"통과\s+{target}: 2건", target)
        # (머리말의 `cargo --version` 호출도 기록되니 시험 호출만 본다)
        seen = [line for line in self.log.read_text().splitlines() if " --test " in line]
        # 관문이 돌려야 하는 시험 목록은 스크립트의 `INTEGRATION_TESTS`다(다른 역할이 줄을 더해도 이 시험이 깨지지 않는다). 목록의 모든 파일을 순서대로
        # 돌려야 하고, 처음 필수로 정한 20개 파일은 목록에서 빠질 수 없다.
        script = Path(__file__).with_name("release-check.sh").read_text(encoding="utf-8")
        listed = re.search(r"^INTEGRATION_TESTS='\n(.*?)^'$", script, re.S | re.M)
        self.assertIsNotNone(listed, "INTEGRATION_TESTS 목록을 찾지 못했다")
        expected = [line.split()[1] for line in listed.group(1).splitlines() if line.strip()]
        self.assertEqual([re.search(r" --test (\S+)", line).group(1) for line in seen], expected, seen)
        required = (
            "claude_canary srt_canary cli_started cli_resume cli_refusal container_canary container_anchor container_claude "
            "container_firewall claude_route claude_wiring claude_stale claude_attempts claude_schedule claude_shutdown "
            "vm_serve private_serve tool_secrets split_roles secret_sweep openai_compat"
        ).split()
        self.assertEqual(sorted(set(required) - set(expected)), [], "필수 통합 시험이 목록에서 빠졌다")
        for line in seen:
            self.assertRegex(line, r"env claude=required srt=required container=required db=1 node=1 openai=required sdk=/\S+/openai-sdk$", f"필수 모드가 아니다: {line}")
            self.assertIn("-- --nocapture --test-threads=1", line)
        # 공식 SDK 환경은 시험 전에 한 번, 빌드 폴더 밑 절대 경로에 만든다(저장소에는 설치하지 않는다)
        made = [line for line in self.log.read_text().splitlines() if line.startswith("sdk-setup ")]
        self.assertEqual(len(made), 1, made)
        self.assertRegex(made[0], r"^sdk-setup /\S+/openai-sdk$")
        self.assertLess(self.log.read_text().index("sdk-setup"), self.log.read_text().index(" --test "), "SDK 준비가 시험보다 먼저다")
        self.assertTrue(any("--features testing --test claude_canary" in line for line in seen))
        for hidden in ("nobody-quokka", "127.0.0.1:1", "0" * 64):
            self.assertNotIn(hidden, everything(done), "주소·이미지 참조는 출력하지 않는다")

    def test_a_skip_marker_in_any_test_output_fails_the_step_and_names_the_file(self):
        for mode, target in (("skip", "claude_route"), ("korean", "private_serve")):
            done = self.run_step("--strict", mode=mode)
            self.assertEqual(done.returncode, 1, done.stdout)
            self.assertRegex(done.stdout, rf"실패\s+{target}: 건너뜀 표지 1건", mode)

    def test_an_ignored_test_in_a_listed_file_fails_the_step_and_names_the_file(self):
        done = self.run_step("--strict", mode="ignore")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패\s+openai_compat: #\[ignore\]된 시험이 1건이다\(목록이 허용한 것은 0건\)")
        self.assertRegex(done.stdout, r"통과\s+split_roles: 2건", "목록이 허용한 #[ignore] 1건은 그대로 통과한다")

    def test_a_list_that_allows_more_ignored_tests_than_the_run_shows_fails_too(self):
        done = self.run_step("--strict", mode="fewer")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패\s+split_roles: #\[ignore\]된 시험이 0건이다\(목록이 허용한 것은 1건\)")

    def test_a_file_where_no_test_ran_or_a_test_failed_fails_the_step(self):
        done = self.run_step("--strict", mode="zero")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패\s+private_serve: 시험이 하나도 돌지 않았다")
        done = self.run_step("--strict", mode="fail")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패\s+vm_serve \(종료 코드 101\)")

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
            self.assertIn(f"필요한 도구가 없다: {word}", done.stdout)
            self.assertNotIn("cargo-called", done.stdout, tool)
        done = self.run_step("--strict", shim=self.shims(container_up=False))
        self.assertEqual(done.returncode, 1)
        self.assertIn("container 서비스가 꺼져 있다", done.stdout)
        done = self.run_step("--strict", shim=self.shims(image_present=False))
        self.assertEqual(done.returncode, 1)
        self.assertIn("로컬에 없다", done.stdout)
        for image in ("", "name:tag-only"):
            done = self.run_step("--strict", image=image)
            self.assertEqual(done.returncode, 1, image)
            self.assertIn("BOT_MODEL_IMAGE", done.stdout)
            self.assertNotIn("cargo-called", done.stdout)
        done = self.run_step("--strict", extra={"BOT_TEST_DATABASE_URL": ""})
        self.assertEqual(done.returncode, 1)
        self.assertIn("BOT_TEST_DATABASE_URL이 없다", done.stdout)
        self.assertNotIn("cargo-called", done.stdout)

    def test_the_official_sdk_environment_is_prepared_first_and_a_failure_stops_before_any_test(self):
        done = self.run_step("--strict", extra={"SETUP_EXIT": "1"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("공식 OpenAI SDK 시험 환경을 만들지 못했다", done.stdout + self.log.read_text() + everything(done))
        self.assertNotIn("cargo-called", done.stdout, "환경이 없으면 시험은 하나도 돌지 않는다")

    def test_a_relative_sdk_folder_is_refused_and_a_given_absolute_one_is_used_for_setup_and_tests(self):
        done = self.run_step("--strict", extra={"BOT_OPENAI_SDK_DIR": "relative/sdk"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("절대 경로여야 한다", done.stdout)
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
        self.assertRegex(done.stdout, r"실패\s+openai_compat: 건너뜀 표지 1건")

    def test_a_full_run_without_strict_shows_the_step_as_a_skip_and_strict_never_skips_it(self):
        repo = committed(self)
        # 안쪽 관문이 자체 시험을 또 돌리지 않게 표지를 준다(시험이 시험을 다시 부르는 순환 방지 — 관문이 자체 시험을 돌릴 때 거는 값과 같다)
        env = {"PATH": f"{self.shims()}:/usr/bin:/bin", "RELEASE_CHECK_IN_SELFTEST": "1"}
        done = repo.gate(env=env)
        self.assertRegex(done.stdout, r"건너뜀\s+integration\s+\d+s\s+공개 직전\(--strict\)에만 돈다")
        self.assertNotIn("+ cargo test --locked -p bot-broker", done.stdout)
        # 같은 설정으로 --strict --only를 주면 건너뛰지 않고 돈다(대역이 모두 있어 통과한다)
        done = self.run_step("--strict")
        self.assertEqual(done.returncode, 0, done.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=1)

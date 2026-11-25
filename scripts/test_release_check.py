#!/usr/bin/env python3
"""Tests for release-check.sh: step selection, visible skips, strict mode, the pinned clone, git failures and output hygiene.
Only synthetic data and throwaway git repositories are used. The gitleaks tests need gitleaks (the gate needs it too)."""

import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testing_repo import GitShim, PUBLIC, Repo, everything  # noqa: E402

TERM = "quokka"
GITLEAKS = shutil.which("gitleaks")
needs_gitleaks = unittest.skipUnless(GITLEAKS, "gitleaks가 있어야 한다(관문도 필요로 한다)")


def aws_key():
    """A synthetic access key built at run time so that this file holds no key-shaped literal."""
    return "AK" + "IA" + "QZ7K3M2WXY5LT4NB"


def committed(case, message="work"):
    repo = Repo(case)
    repo.write("a.txt", "fine\n")
    repo.commit(message, env=PUBLIC)
    return repo


class Steps(unittest.TestCase):
    def test_a_skipped_check_is_visible_and_not_a_pass(self):
        done = committed(self).gate("--only", "private-terms")
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("건너뜀: BOT_PRIVATE_TERMS_FILE", done.stdout)
        self.assertIn("통과 0 · 실패 0 · 건너뜀 1", done.stdout)

    def test_strict_counts_a_skip_as_a_failure(self):
        self.assertEqual(committed(self).gate("--strict", "--only", "private-terms").returncode, 1)

    def test_bad_arguments_are_usage_errors_and_list_names_every_step(self):
        repo = committed(self)
        for args in (["--only", "nope"], ["--only"], ["--wat"]):
            self.assertEqual(repo.gate(*args).returncode, 2, args)
        listed = repo.gate("--list").stdout.split()
        for name in ("clean", "fmt", "clippy", "test", "contracts", "deny", "secrets-tree"):
            self.assertIn(name, listed)
        for name in ("secrets-history", "identity", "private-terms", "selftest"):
            self.assertIn(name, listed)

    def test_the_tree_must_match_the_commit_and_a_difference_is_a_visible_skip(self):
        repo = committed(self)
        done = repo.gate("--only", "clean")
        self.assertEqual((done.returncode, "작업 트리와 인덱스가 커밋과 같다" in done.stdout), (0, True), done.stdout)
        repo.write("later.txt", "uncommitted\n")
        done = repo.gate("--only", "clean")
        self.assertEqual(done.returncode, 0)
        self.assertIn("커밋하지 않은 변경이 1개", done.stdout)
        self.assertIn("건너뜀 1", done.stdout)
        self.assertEqual(repo.gate("--strict", "--only", "clean").returncode, 1, "공개 직전에는 통과할 수 없다")
        repo.git("add", "-A")
        self.assertEqual(repo.gate("--strict", "--only", "clean").returncode, 1, "staged도 커밋이 아니다")


class Identity(unittest.TestCase):
    def test_identity_requires_public_addresses_on_commits_tags_and_committers(self):
        repo = committed(self)
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 0, done.stdout)
        repo.write("b.txt", "x\n")
        repo.commit("private author", env={**PUBLIC, "GIT_AUTHOR_EMAIL": "me@example.test"})
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 1)
        self.assertRegex(done.stdout, r"커밋 [0-9a-f]+")
        self.assertNotIn("me@example.test", everything(done))

    def test_identity_checks_annotated_tags_without_printing_their_names(self):
        repo = committed(self)
        repo.git("tag", "-a", f"{TERM}-v1", "-m", "release", env={"GIT_COMMITTER_EMAIL": "me@example.test"})
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 1)
        self.assertRegex(done.stdout, r"태그 [0-9a-f]+")
        self.assertNotIn(TERM, everything(done).lower(), "태그 이름은 출력하지 않는다")

    def test_identity_checks_the_committer_too(self):
        repo = committed(self)
        repo.write("b.txt", "x\n")
        repo.commit("private committer", env={**PUBLIC, "GIT_COMMITTER_EMAIL": "me@example.test"})
        self.assertEqual(repo.gate("--only", "identity").returncode, 1)

    def test_a_git_failure_is_a_failure_not_a_clean_result(self):
        for sub, message in (("log", "커밋을 읽지 못했다"), ("for-each-ref", "고정한 참조가 하나도 없다"), ("fetch", "공개할 참조를 복제하지 못했다")):
            repo = committed(self)
            done = repo.gate("--only", "identity", env=GitShim(self, sub).env())
            self.assertEqual(done.returncode, 1, f"{sub}: {done.stdout}")
            self.assertIn(message, done.stdout, sub)
            if sub == "fetch":
                self.assertNotIn("고정한 참조가 하나도 없다", done.stdout, "복제 실패에서 바로 끝나야 한다(빈 복제본 검사로 넘어가지 않는다)")

    def test_a_shallow_clone_cannot_pass_the_history_steps(self):
        source = committed(self, "one")
        source.write("a.txt", "two\n")
        source.commit("two", env=PUBLIC)
        shallow = source.outside / "shallow"
        source.git("clone", "-q", "--depth", "1", f"file://{source.repo}", str(shallow), cwd=source.outside)
        done = subprocess.run(
            ["bash", str(Path(__file__).with_name("release-check.sh")), "--only", "identity"],
            cwd=shallow, env=source.env(), capture_output=True, text=True,
        )
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("얕은 복제", done.stdout)


class Strict(unittest.TestCase):
    def test_strict_needs_the_database_and_never_skips_the_postgres_tests(self):
        repo = committed(self)
        # 가드가 빠진 구현이 실제 서버에 닿지 않게 연결 대상을 막아 둔다.
        done = repo.gate("--strict", "--only", "test", env={"PGHOST": "127.0.0.1", "PGPORT": "1"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("BOT_TEST_DATABASE_URL이 없다", done.stdout)
        self.assertNotIn("+ cargo test", done.stdout, "DB가 없으면 시험을 돌리기 전에 실패한다")

    def test_strict_fails_when_the_database_cannot_be_reached_and_never_prints_the_address(self):
        repo = committed(self)
        url = "postgres://nobody-quokka@127.0.0.1:1/none"
        done = repo.gate("--strict", "--only", "test", env={"BOT_TEST_DATABASE_URL": url})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"연결하지 못했다|필요한 도구가 없다: psql")
        self.assertNotIn("nobody-quokka", everything(done))
        self.assertNotIn("127.0.0.1:1", everything(done))


def run_with_recorder(case, *args, then="", only="test", repo=None):
    """`cargo test` 자체를 기록 대역으로 바꿔 DB도 빌드도 없이 관문을 돌린다. `then`은 시험이 도는 «동안» 저장소에 일어나는 일(예: 커밋)을 흉내 내는 셸 한 줄이다."""
    repo = repo or committed(case)
    shim = Path(tempfile.mkdtemp(prefix="gate-shim-"))
    case.addCleanup(lambda: shutil.rmtree(shim, ignore_errors=True))
    for name, body in {
        "psql": "#!/bin/sh\nexit 0\n",
        "cargo": '#!/bin/sh\necho "cargo-called require=$BOT_TEST_REQUIRE_DB url=${BOT_TEST_DATABASE_URL:+set}"\n'
        + then
        + "\nexit 0\n",
    }.items():
        (shim / name).write_text(body)
        (shim / name).chmod(0o755)
    url = "postgres://nobody-quokka@127.0.0.1:1/none"
    env = {"BOT_TEST_DATABASE_URL": url, "PATH": f"{shim}:{os.environ['PATH']}"}
    return repo.gate(*args, "--only", only, env=env)


class StrictRunsCargoTest(unittest.TestCase):
    """`cargo test` itself is replaced by a recorder so the test needs neither a database nor a build."""

    def run_with_recorder(self, *args):
        return run_with_recorder(self, *args)

    def test_strict_runs_the_tests_with_the_database_required_and_never_prints_its_address(self):
        done = self.run_with_recorder("--strict")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("cargo-called require=1 url=set", done.stdout)
        self.assertIn("+ cargo test --workspace --locked --no-fail-fast", done.stdout, "첫 실패에서 멈추지 않는다")
        for secret in ("nobody-quokka", "127.0.0.1:1"):
            self.assertNotIn(secret, everything(done))

    def test_a_failing_test_run_still_fails_the_step_even_though_it_does_not_stop_at_the_first_failure(self):
        done = run_with_recorder(self, "--strict", then="exit 101")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패\s+test\b")

    def test_without_strict_the_database_stays_optional_for_the_tests(self):
        done = self.run_with_recorder()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("cargo-called require= url=set", done.stdout)


class PinnedCommit(unittest.TestCase):
    """공개할 커밋을 시작할 때 고정하고, 같은 커밋에서 시험·이력 확인을 하며, 검사한 해시를 출력한다."""

    MOVE = "git commit --allow-empty -q -m moved"

    def head(self, repo):
        return repo.git("rev-parse", "HEAD").strip()

    def test_the_checked_commit_hash_is_printed_at_the_start_and_at_the_end(self):
        repo = committed(self)
        done = repo.gate("--only", "identity")
        head = self.head(repo)
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn(f"검사할 커밋: {head}", done.stdout)
        self.assertIn(f"검사한 커밋: {head}", done.stdout)
        self.assertNotIn(" pin ", done.stdout, "아무것도 안 바뀌면 pin 줄은 없다")

    def test_a_commit_made_during_a_strict_run_fails_it_and_names_the_commit_that_was_pinned(self):
        repo = committed(self)
        before = self.head(repo)
        done = run_with_recorder(self, "--strict", then=self.MOVE, repo=repo)
        self.assertNotEqual(self.head(repo), before, "시험 도중 커밋이 만들어졌다")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패 pin .*HEAD")
        self.assertIn(f"검사한 커밋: {before}", done.stdout, "고정한 커밋을 출력한다(새 커밋이 아니다)")
        self.assertIn("통과 1 · 실패 1 · 건너뜀 0", done.stdout)

    def test_without_strict_the_same_move_is_a_visible_skip_not_a_pass(self):
        repo = committed(self)
        done = run_with_recorder(self, then=self.MOVE, repo=repo)
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertRegex(done.stdout, r"건너뜀 pin .*HEAD")
        self.assertIn("통과 1 · 실패 0 · 건너뜀 1", done.stdout)

    def test_a_tag_or_branch_created_during_a_strict_run_also_fails_it(self):
        for move in ("git tag late-tag", "git branch late-branch"):
            repo = committed(self)
            done = run_with_recorder(self, "--strict", then=move, repo=repo)
            self.assertEqual(done.returncode, 1, f"{move}: {done.stdout}")
            self.assertRegex(done.stdout, r"실패 pin ", move)

    def test_a_change_that_is_undone_before_the_end_is_not_detected_which_the_docs_say(self):
        # 알려 둔 한계: 시작·복제 시점·끝의 값만 견주므로 도중에 커밋했다 되돌린 변화는 보이지 않는다.
        repo = committed(self)
        before = self.head(repo)
        done = run_with_recorder(self, "--strict", then=self.MOVE + " && git reset -q --hard HEAD~1", repo=repo)
        self.assertEqual(self.head(repo), before, "도중의 커밋이 끝나기 전에 원복됐다")
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertNotIn(" pin ", done.stdout)
        docs = (Path(__file__).resolve().parent.parent / "docs" / "release.md").read_text(encoding="utf-8")
        self.assertIn("원복되는 변화", docs, "문서가 이 한계를 말해야 한다")

    def test_a_commit_on_a_detached_head_fails_a_strict_run_although_no_reference_moved(self):
        # CI의 병합 커밋 체크아웃처럼 HEAD만 움직이고 브랜치·태그는 그대로인 경우: 참조 비교만으로는 놓친다.
        repo = committed(self)
        repo.git("checkout", "-q", "--detach")
        before = repo.git("for-each-ref", "refs/heads", "refs/tags")
        done = run_with_recorder(self, "--strict", then=self.MOVE, repo=repo)
        self.assertEqual(repo.git("for-each-ref", "refs/heads", "refs/tags"), before, "참조는 그대로다")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"실패 pin ")

    def test_strict_history_steps_refuse_a_head_that_moved_before_they_ran(self):
        repo = committed(self)
        done = run_with_recorder(self, "--strict", then=self.MOVE, only="test identity", repo=repo)
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("시작할 때 고정한 커밋과 지금의 HEAD가 다르다", done.stdout)
        self.assertRegex(done.stdout, r"통과 test ")
        self.assertRegex(done.stdout, r"실패 identity ")
        self.assertNotIn("모든 커밋·태그의 작성자가", done.stdout, "다른 커밋의 이력을 검사해 통과시키지 않는다")

    def test_without_strict_the_history_steps_follow_the_current_head(self):
        repo = committed(self)
        done = run_with_recorder(self, then=self.MOVE, only="test identity", repo=repo)
        self.assertNotIn("시작할 때 고정한 커밋", done.stdout)
        self.assertRegex(done.stdout, r"통과 test ")

    def test_a_repository_without_commits_prints_that_nothing_was_pinned(self):
        done = Repo(self).gate("--only", "clean")
        self.assertIn("검사할 커밋: (커밋 없음)", done.stdout)
        self.assertIn("검사한 커밋: (커밋 없음)", done.stdout)


class PrivateTerms(unittest.TestCase):
    def hostile(self):
        repo = Repo(self)
        repo.write(f"{TERM}-dir/{TERM}-file.txt", f"{TERM}\n")
        repo.write("clean.txt", f"line with {TERM}\n")
        os.symlink(f"{TERM}-target", repo.repo / "alias")
        repo.commit(
            f"{TERM} commit message",
            env={**PUBLIC, "GIT_AUTHOR_NAME": f"{TERM} Person", "GIT_AUTHOR_EMAIL": "1+q@users.noreply.github.com"},
        )
        repo.git("branch", f"{TERM}-branch")
        repo.git("tag", "-a", f"{TERM}-tag", "-m", f"{TERM} tag message", env=PUBLIC)
        return repo

    def test_the_report_holds_no_term_name_or_absolute_path(self):
        repo = self.hostile()
        listing = repo.terms(TERM)
        done = repo.gate("--only", "private-terms", env={"BOT_PRIVATE_TERMS_FILE": str(listing)})
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("금칙어 발견", done.stdout)
        text = everything(done)
        self.assertNotIn(TERM, text.lower())
        for needle in (str(repo.repo), str(repo.outside), str(listing), str(Path.home())):
            self.assertNotIn(needle, text, "경로가 출력에 나오면 안 된다")
        self.assertNotIn(repo.repo.name, text)
        self.assertNotIn(listing.parent.name, text)

    def test_a_clean_repository_passes_and_a_bad_list_fails(self):
        repo = committed(self)
        listing = repo.terms("zebrafish")
        done = repo.gate("--only", "private-terms", env={"BOT_PRIVATE_TERMS_FILE": str(listing)})
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("금칙어 없음", done.stdout)
        done = repo.gate("--only", "private-terms", env={"BOT_PRIVATE_TERMS_FILE": str(repo.outside / "missing.txt")})
        self.assertEqual(done.returncode, 1)
        inside = repo.write("terms.txt", "zebrafish\n")
        repo.commit("list inside", env=PUBLIC)
        done = repo.gate("--only", "private-terms", env={"BOT_PRIVATE_TERMS_FILE": str(inside)})
        self.assertEqual(done.returncode, 1, done.stdout)

    def test_only_the_pinned_refs_are_scanned_for_history(self):
        repo = committed(self)
        repo.git("tag", "-a", "v1", "-m", f"tagged for the {TERM} team", env=PUBLIC)
        env = {"BOT_PRIVATE_TERMS_FILE": str(repo.terms(TERM))}
        self.assertEqual(repo.gate("--only", "private-terms", env=env).returncode, 1, "기본은 브랜치와 태그를 모두 본다")
        heads = repo.gate("--only", "private-terms", env={**env, "RELEASE_CHECK_REFS": "refs/heads/*"})
        self.assertEqual(heads.returncode, 0, heads.stdout)
        self.assertIn("참조 2개", heads.stdout, "브랜치 하나와 고정한 HEAD. 태그는 가져오지 않았다")

    def test_the_checked_out_commit_is_always_pinned_even_when_no_branch_or_tag_exists(self):
        repo = committed(self)
        repo.git("checkout", "-q", "--detach")
        repo.git("branch", "-D", "main", ok=(0, 1))
        repo.git("branch", "-D", "master", ok=(0, 1))
        self.assertEqual(repo.git("for-each-ref"), "", "분리된 HEAD: 브랜치도 태그도 없다")
        env = {"BOT_PRIVATE_TERMS_FILE": str(repo.terms(TERM))}
        repo.write("b.txt", f"{TERM}\n")
        repo.commit(f"later {TERM}", env=PUBLIC)
        done = repo.gate("--only", "private-terms", env=env)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("참조 1개", done.stdout)
        self.assertRegex(done.stdout, r"이력 blob [0-9a-f]{8} b\.txt:1")

    def test_a_pin_without_any_reference_fails_instead_of_scanning_nothing(self):
        repo = Repo(self)  # 커밋이 없어 HEAD도 없다
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("고정한 참조가 하나도 없다", done.stdout)
        repo = committed(self)
        repo.git("checkout", "-q", "--detach")
        repo.git("branch", "-D", "main", ok=(0, 1))
        repo.git("branch", "-D", "master", ok=(0, 1))
        done = repo.gate("--only", "identity", env={"RELEASE_CHECK_REFS": "refs/heads/nothing-matches*"})
        self.assertEqual(done.returncode, 0, "HEAD가 고정되므로 이력은 비지 않는다: " + done.stdout)
        self.assertIn("참조 1개", done.stdout)

    def test_a_git_failure_in_the_history_scan_is_a_failure(self):
        repo = committed(self)
        env = {"BOT_PRIVATE_TERMS_FILE": str(repo.terms("zebrafish")), **GitShim(self, "rev-list").env()}
        self.assertEqual(repo.gate("--only", "private-terms", env=env).returncode, 1)


@needs_gitleaks
class Gitleaks(unittest.TestCase):
    def merged_secret(self):
        """A key that first appears in a conflict resolution and is removed again afterwards."""
        repo = Repo(self)
        repo.write("a.txt", "base\n")
        repo.commit("base", env=PUBLIC)
        repo.git("checkout", "-q", "-b", "side")
        repo.write("a.txt", "side\n")
        repo.commit("side", env=PUBLIC)
        repo.git("checkout", "-q", "-")
        repo.write("a.txt", "main\n")
        repo.commit("main", env=PUBLIC)
        repo.git("merge", "-q", "side", ok=(0, 1))
        repo.write("a.txt", f"aws_access_key_id = {aws_key()}\n")
        repo.commit("merge", env=PUBLIC)
        repo.write("a.txt", "clean\n")
        repo.commit("clean up", env=PUBLIC)
        return repo

    def test_history_sees_a_value_first_written_in_a_merge_resolution(self):
        repo = self.merged_secret()
        plain = subprocess.run(
            [GITLEAKS, "git", ".", "--no-banner", "--redact", "--log-opts=--all"],
            cwd=repo.repo, env=repo.env(), capture_output=True, text=True,
        )
        self.assertEqual(plain.returncode, 0, "대조: `--all`만으로는 병합 해결에서 처음 생긴 값이 보이지 않는다")
        done = repo.gate("--only", "secrets-history")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertRegex(done.stdout, r"규칙 aws-access-token · a\.txt:1 · 커밋 [0-9a-f]{8}")
        self.assertNotIn(aws_key(), everything(done), "값은 가려서 출력한다")

    def test_the_report_names_rule_file_line_and_commit_but_never_author_email_message_or_value(self):
        repo = Repo(self)
        repo.write("src/app.txt", f"aws_access_key_id = {aws_key()}\n")
        repo.commit(
            f"{TERM} commit message",
            env={"GIT_AUTHOR_NAME": f"{TERM} Person", "GIT_AUTHOR_EMAIL": "1+q@users.noreply.github.com",
                 "GIT_COMMITTER_EMAIL": "1+q@users.noreply.github.com"},
        )
        done = repo.gate("--only", "secrets-tree secrets-history")
        text = re.sub(r"\x1b\[[0-9;]*m", "", everything(done))
        self.assertEqual(done.returncode, 1, text)
        self.assertRegex(text, r"규칙 [a-z0-9-]+ · src/app\.txt:1")
        self.assertRegex(text, r"규칙 [a-z0-9-]+ · src/app\.txt:1 · 커밋 [0-9a-f]{8}")
        for leaked in (TERM, aws_key(), str(repo.repo), "Person", "example.test"):
            self.assertNotIn(leaked.lower(), text.lower())
        self.assertNotIn("Author:", text)
        self.assertNotIn("Message:", text)

    def test_a_failing_gitleaks_is_a_failure_not_a_clean_scan(self):
        repo = committed(self)
        shim = Path(tempfile.mkdtemp(prefix="gate-shim-"))
        self.addCleanup(lambda: shutil.rmtree(shim, ignore_errors=True))
        fake = shim / "gitleaks"
        fake.write_text("#!/bin/sh\nexit 1\n")
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
        done = repo.gate("--only", "secrets-tree", env={"PATH": f"{shim}:{os.environ['PATH']}"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("gitleaks 실행 실패", done.stdout)
        self.assertNotIn("비밀 값 없음", done.stdout)

    def test_the_working_tree_scan_covers_staged_content_the_tree_no_longer_shows(self):
        repo = committed(self)
        repo.write("a.txt", f"aws_access_key_id = {aws_key()}\n")
        repo.git("add", "a.txt")
        repo.write("a.txt", "fine\n")
        done = repo.gate("--only", "secrets-tree")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)

    def test_a_clean_repository_passes_both_secret_steps(self):
        repo = committed(self)
        done = repo.gate("--only", "secrets-tree secrets-history")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_git_failure_never_turns_into_an_empty_clean_scan(self):
        repo = committed(self)
        done = repo.gate("--only", "secrets-tree", env=GitShim(self, "ls-files").env())
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("git ls-files 실패", done.stdout)
        done = repo.gate("--only", "secrets-history", env=GitShim(self, "fetch").env())
        self.assertEqual(done.returncode, 1, done.stdout)

    def test_a_repository_without_publishable_files_fails_instead_of_passing_vacuously(self):
        repo = Repo(self)
        done = repo.gate("--only", "secrets-tree")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("하나도 없다", done.stdout)


class Integration(unittest.TestCase):
    """필수 통합 시험 단계. `cargo`와 실제 도구(claude·srt·container·node·psql)를 기록 대역으로 바꿔, 환경이 없으면 실패하고 건너뜀 표지·통과 0건·시험 실패를
    그냥 지나치지 않으며 필수 모드로 돌리는지 본다(실제 VM·모델 호출은 없다)."""

    CARGO = """#!/bin/sh
echo "cargo-called $*"
# 단계는 시험 출력을 파일로 받아 요약만 찍는다: 대역이 본 환경은 따로 남긴다
echo "$* env claude=$BOT_CLAUDE_CANARY srt=$BOT_SRT_CANARY container=$BOT_CONTAINER_CANARY db=$BOT_TEST_REQUIRE_DB node=$BOT_REQUIRE_NODE" >> "$SHIM_LOG"
case "$SHIM_MODE" in
  skip) case "$*" in *claude_route*) echo "skipped: claude CLI not found" ;; esac ;;
  korean) case "$*" in *private_serve*) echo "srt 또는 python3가 없어 건너뜀" ;; esac ;;
  zero) case "$*" in *private_serve*) echo "test result: ok. 0 passed; 0 failed; 0 ignored"; exit 0 ;; esac ;;
  fail) case "$*" in *vm_serve*) echo "test result: FAILED. 0 passed; 1 failed"; exit 101 ;; esac ;;
esac
echo "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out"
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
            "vm_serve private_serve tool_secrets split_roles secret_sweep"
        ).split()
        self.assertEqual(sorted(set(required) - set(expected)), [], "필수 통합 시험이 목록에서 빠졌다")
        for line in seen:
            self.assertTrue(
                line.endswith("env claude=required srt=required container=required db=1 node=1"),
                f"필수 모드가 아니다: {line}",
            )
            self.assertIn("-- --nocapture --test-threads=1", line)
        self.assertTrue(any("--features testing --test claude_canary" in line for line in seen))
        for hidden in ("nobody-quokka", "127.0.0.1:1", "0" * 64):
            self.assertNotIn(hidden, everything(done), "주소·이미지 참조는 출력하지 않는다")

    def test_a_skip_marker_in_any_test_output_fails_the_step_and_names_the_file(self):
        for mode, target in (("skip", "claude_route"), ("korean", "private_serve")):
            done = self.run_step("--strict", mode=mode)
            self.assertEqual(done.returncode, 1, done.stdout)
            self.assertRegex(done.stdout, rf"실패\s+{target}: 건너뜀 표지 1건", mode)

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


class Environment(unittest.TestCase):
    def test_the_self_tests_ignore_the_operators_gate_settings(self):
        """리허설에서 `RELEASE_CHECK_REFS`를 준 채 돌리면 임시 저장소의 이력 검사가 엉뚱한 참조를 보던 일: 시험은 명시하지 않은 설정을 받지 않는다."""
        repo = committed(self)
        env = {"RELEASE_CHECK_REFS": "refs/heads/nonexistent", "BOT_CLAUDE_CANARY": "required"}
        saved = {k: os.environ.get(k) for k in env}
        os.environ.update(env)
        try:
            done = repo.gate("--only", "identity")
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
        self.assertEqual(done.returncode, 0, done.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=1)

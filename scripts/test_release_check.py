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
from testing_repo import GitShim, PUBLIC, Repo, committed, everything  # noqa: E402

TERM = "quokka"
GITLEAKS = shutil.which("gitleaks")
needs_gitleaks = unittest.skipUnless(GITLEAKS, "requires gitleaks (also required by the gate)")


def aws_key():
    """A synthetic access key built at run time so that this file holds no key-shaped literal."""
    return "AK" + "IA" + "QZ7K3M2WXY5LT4NB"


class Steps(unittest.TestCase):
    def test_a_skipped_check_is_visible_and_not_a_pass(self):
        done = committed(self).gate("--only", "private-terms")
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("SKIP: BOT_PRIVATE_TERMS_FILE", done.stdout)
        self.assertIn("PASS 0 · FAIL 0 · SKIP 1", done.stdout)

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
        self.assertEqual((done.returncode, "working tree and index match HEAD" in done.stdout), (0, True), done.stdout)
        repo.write("later.txt", "uncommitted\n")
        done = repo.gate("--only", "clean")
        self.assertEqual(done.returncode, 0)
        self.assertIn("found 1 uncommitted change(s)", done.stdout)
        self.assertIn("SKIP 1", done.stdout)
        self.assertEqual(repo.gate("--strict", "--only", "clean").returncode, 1, "cannot pass under --strict")
        repo.git("add", "-A")
        self.assertEqual(repo.gate("--strict", "--only", "clean").returncode, 1, "staged changes are still uncommitted")


class Identity(unittest.TestCase):
    def test_identity_requires_public_addresses_on_commits_tags_and_committers(self):
        repo = committed(self)
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 0, done.stdout)
        repo.write("b.txt", "x\n")
        repo.commit("private author", env={**PUBLIC, "GIT_AUTHOR_EMAIL": "me@example.test"})
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 1)
        self.assertRegex(done.stdout, r"commit [0-9a-f]+")
        self.assertNotIn("me@example.test", everything(done))

    def test_identity_checks_annotated_tags_without_printing_their_names(self):
        repo = committed(self)
        repo.git("tag", "-a", f"{TERM}-v1", "-m", "release", env={"GIT_COMMITTER_EMAIL": "me@example.test"})
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 1)
        self.assertRegex(done.stdout, r"tag [0-9a-f]+")
        self.assertNotIn(TERM, everything(done).lower(), "tag names must not be printed")

    def test_identity_checks_the_committer_too(self):
        repo = committed(self)
        repo.write("b.txt", "x\n")
        repo.commit("private committer", env={**PUBLIC, "GIT_COMMITTER_EMAIL": "me@example.test"})
        self.assertEqual(repo.gate("--only", "identity").returncode, 1)

    def test_a_git_failure_is_a_failure_not_a_clean_result(self):
        for sub, message in (("log", "failed to read commits"), ("for-each-ref", "no refs were pinned"), ("fetch", "failed to fetch publishable refs")):
            repo = committed(self)
            done = repo.gate("--only", "identity", env=GitShim(self, sub).env())
            self.assertEqual(done.returncode, 1, f"{sub}: {done.stdout}")
            self.assertIn(message, done.stdout, sub)
            if sub == "fetch":
                self.assertNotIn("no refs were pinned", done.stdout, "must fail immediately on fetch error without falling through")

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
        self.assertIn("shallow clone", done.stdout)


class Strict(unittest.TestCase):
    def test_strict_needs_the_database_and_never_skips_the_postgres_tests(self):
        repo = committed(self)
        # Point PGHOST/PGPORT at a closed port so a broken guard cannot reach a real server.
        done = repo.gate("--strict", "--only", "test", env={"PGHOST": "127.0.0.1", "PGPORT": "1"})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("BOT_TEST_DATABASE_URL is not set", done.stdout)
        self.assertNotIn("+ cargo test", done.stdout, "must fail before running cargo test when DB is missing")

    def test_strict_fails_when_the_database_cannot_be_reached_and_never_prints_the_address(self):
        repo = committed(self)
        url = "postgres://nobody-quokka@127.0.0.1:1/none"
        done = repo.gate("--strict", "--only", "test", env={"BOT_TEST_DATABASE_URL": url})
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"failed to connect|missing required tool: psql")
        self.assertNotIn("nobody-quokka", everything(done))
        self.assertNotIn("127.0.0.1:1", everything(done))


def run_with_recorder(case, *args, then="", only="test", repo=None):
    """Replace `cargo test` with a recorder shim (no DB or build). `then` simulates repo mutations during the run."""
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
        self.assertIn("+ cargo test --workspace --locked --no-fail-fast", done.stdout, "must not stop at first failure")
        for secret in ("nobody-quokka", "127.0.0.1:1"):
            self.assertNotIn(secret, everything(done))

    def test_a_failing_test_run_still_fails_the_step_even_though_it_does_not_stop_at_the_first_failure(self):
        done = run_with_recorder(self, "--strict", then="exit 101")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL\s+test\b")

    def test_without_strict_the_database_stays_optional_for_the_tests(self):
        done = self.run_with_recorder()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("cargo-called require= url=set", done.stdout)

    def test_clippy_checks_both_workspace_targets_and_the_bot_binary(self):
        repo = committed(self)
        shim = Path(tempfile.mkdtemp(prefix="gate-clippy-"))
        self.addCleanup(lambda: shutil.rmtree(shim, ignore_errors=True))
        cargo = shim / "cargo"
        cargo.write_text(
            "#!/bin/sh\n"
            'echo "clippy-call $*"\n'
            'case "$*" in *"$FAIL_ON"*) [ -n "$FAIL_ON" ] && exit 101 ;; esac\n'
            "exit 0\n"
        )
        cargo.chmod(0o755)
        base_env = {"PATH": f"{shim}:{os.environ['PATH']}"}
        done = repo.gate("--only", "clippy", env=base_env)
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("clippy-call clippy --workspace --all-targets --locked -- -D warnings", done.stdout)
        self.assertIn("clippy-call clippy -p bot-cli --bin bot --locked -- -D warnings", done.stdout)
        for fail_on in ("--all-targets", "--bin bot"):
            failed = repo.gate("--only", "clippy", env={**base_env, "FAIL_ON": fail_on})
            self.assertEqual(failed.returncode, 1, (fail_on, failed.stdout))


class PinnedCommit(unittest.TestCase):
    """Pin HEAD/refs at start, run checks against the pinned commit, and print the checked commit hash."""

    MOVE = "git commit --allow-empty -q -m moved"

    def head(self, repo):
        return repo.git("rev-parse", "HEAD").strip()

    def test_the_checked_commit_hash_is_printed_at_the_start_and_at_the_end(self):
        repo = committed(self)
        done = repo.gate("--only", "identity")
        head = self.head(repo)
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn(f"Commit to check: {head}", done.stdout)
        self.assertIn(f"Checked commit: {head}", done.stdout)
        self.assertNotIn(" pin ", done.stdout, "no pin row when nothing moved")

    def test_a_commit_made_during_a_strict_run_fails_it_and_names_the_commit_that_was_pinned(self):
        repo = committed(self)
        before = self.head(repo)
        done = run_with_recorder(self, "--strict", then=self.MOVE, repo=repo)
        self.assertNotEqual(self.head(repo), before, "commit was created during the run")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL pin .*HEAD")
        self.assertIn(f"Checked commit: {before}", done.stdout, "prints the pinned commit, not the moved one")
        self.assertIn("PASS 1 · FAIL 1 · SKIP 0", done.stdout)

    def test_without_strict_the_same_move_is_a_visible_skip_not_a_pass(self):
        repo = committed(self)
        done = run_with_recorder(self, then=self.MOVE, repo=repo)
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertRegex(done.stdout, r"SKIP pin .*HEAD")
        self.assertIn("PASS 1 · FAIL 0 · SKIP 1", done.stdout)

    def test_a_tag_or_branch_created_during_a_strict_run_also_fails_it(self):
        for move in ("git tag late-tag", "git branch late-branch"):
            repo = committed(self)
            done = run_with_recorder(self, "--strict", then=move, repo=repo)
            self.assertEqual(done.returncode, 1, f"{move}: {done.stdout}")
            self.assertRegex(done.stdout, r"FAIL pin ", move)

    def test_a_change_that_is_undone_before_the_end_is_not_detected_which_the_docs_say(self):
        # Documented limitation: only start, mirror, and end checkpoints are compared.
        repo = committed(self)
        before = self.head(repo)
        done = run_with_recorder(self, "--strict", then=self.MOVE + " && git reset -q --hard HEAD~1", repo=repo)
        self.assertEqual(self.head(repo), before, "commit was reverted before end")
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertNotIn(" pin ", done.stdout)
        docs = (Path(__file__).resolve().parent.parent / "docs" / "release.md").read_text(encoding="utf-8")
        self.assertIn("`pin`", docs, "docs/release.md must document the pin-check limitation")

    def test_a_commit_on_a_detached_head_fails_a_strict_run_although_no_reference_moved(self):
        # Detached HEAD (e.g. CI merge checkout): HEAD moves while branches and tags stay unchanged.
        repo = committed(self)
        repo.git("checkout", "-q", "--detach")
        before = repo.git("for-each-ref", "refs/heads", "refs/tags")
        done = run_with_recorder(self, "--strict", then=self.MOVE, repo=repo)
        self.assertEqual(repo.git("for-each-ref", "refs/heads", "refs/tags"), before, "refs are unchanged")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertRegex(done.stdout, r"FAIL pin ")

    def test_strict_history_steps_refuse_a_head_that_moved_before_they_ran(self):
        repo = committed(self)
        done = run_with_recorder(self, "--strict", then=self.MOVE, only="test identity", repo=repo)
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("current HEAD differs from pinned start commit", done.stdout)
        self.assertRegex(done.stdout, r"PASS test ")
        self.assertRegex(done.stdout, r"FAIL identity ")
        self.assertNotIn("all commits and tags use", done.stdout, "must not pass history from a different commit")

    def test_without_strict_the_history_steps_follow_the_current_head(self):
        repo = committed(self)
        done = run_with_recorder(self, then=self.MOVE, only="test identity", repo=repo)
        self.assertNotIn("pinned start commit", done.stdout)
        self.assertRegex(done.stdout, r"PASS test ")

    def test_a_repository_without_commits_prints_that_nothing_was_pinned(self):
        done = Repo(self).gate("--only", "clean")
        self.assertIn("Commit to check: (no commit)", done.stdout)
        self.assertIn("Checked commit: (no commit)", done.stdout)


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
        self.assertIn("private term match", done.stdout)
        text = everything(done)
        self.assertNotIn(TERM, text.lower())
        for needle in (str(repo.repo), str(repo.outside), str(listing), str(Path.home())):
            self.assertNotIn(needle, text, "paths must never appear in output")
        self.assertNotIn(repo.repo.name, text)
        self.assertNotIn(listing.parent.name, text)

    def test_a_clean_repository_passes_and_a_bad_list_fails(self):
        repo = committed(self)
        listing = repo.terms("zebrafish")
        done = repo.gate("--only", "private-terms", env={"BOT_PRIVATE_TERMS_FILE": str(listing)})
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("no private terms found", done.stdout)
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
        self.assertEqual(repo.gate("--only", "private-terms", env=env).returncode, 1, "default scans both branches and tags")
        heads = repo.gate("--only", "private-terms", env={**env, "RELEASE_CHECK_REFS": "refs/heads/*"})
        self.assertEqual(heads.returncode, 0, heads.stdout)
        self.assertIn("refs: 2", heads.stdout, "one branch plus pinned HEAD; tag excluded")

    def test_the_checked_out_commit_is_always_pinned_even_when_no_branch_or_tag_exists(self):
        repo = committed(self)
        repo.git("checkout", "-q", "--detach")
        repo.git("branch", "-D", "main", ok=(0, 1))
        repo.git("branch", "-D", "master", ok=(0, 1))
        self.assertEqual(repo.git("for-each-ref"), "", "detached HEAD with no branches or tags")
        env = {"BOT_PRIVATE_TERMS_FILE": str(repo.terms(TERM))}
        repo.write("b.txt", f"{TERM}\n")
        repo.commit(f"later {TERM}", env=PUBLIC)
        done = repo.gate("--only", "private-terms", env=env)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("refs: 1", done.stdout)
        self.assertRegex(done.stdout, r"history blob [0-9a-f]{8} b\.txt:1")

    def test_a_pin_without_any_reference_fails_instead_of_scanning_nothing(self):
        repo = Repo(self)  # No commits, so HEAD does not exist.
        done = repo.gate("--only", "identity")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("no refs were pinned", done.stdout)
        repo = committed(self)
        repo.git("checkout", "-q", "--detach")
        repo.git("branch", "-D", "main", ok=(0, 1))
        repo.git("branch", "-D", "master", ok=(0, 1))
        done = repo.gate("--only", "identity", env={"RELEASE_CHECK_REFS": "refs/heads/nothing-matches*"})
        self.assertEqual(done.returncode, 0, "HEAD is pinned so history is non-empty: " + done.stdout)
        self.assertIn("refs: 1", done.stdout)

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
        self.assertEqual(plain.returncode, 0, "control: `--all` alone misses values introduced in a merge resolution")
        done = repo.gate("--only", "secrets-history")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertRegex(done.stdout, r"rule aws-access-token · a\.txt:1 · commit [0-9a-f]{8}")
        self.assertNotIn(aws_key(), everything(done), "values must be redacted")

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
        self.assertRegex(text, r"rule [a-z0-9-]+ · src/app\.txt:1")
        self.assertRegex(text, r"rule [a-z0-9-]+ · src/app\.txt:1 · commit [0-9a-f]{8}")
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
        self.assertIn("gitleaks execution failed", done.stdout)
        self.assertNotIn("no secrets found", done.stdout)

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
        self.assertIn("git ls-files failed", done.stdout)
        done = repo.gate("--only", "secrets-history", env=GitShim(self, "fetch").env())
        self.assertEqual(done.returncode, 1, done.stdout)

    def test_a_repository_without_publishable_files_fails_instead_of_passing_vacuously(self):
        repo = Repo(self)
        done = repo.gate("--only", "secrets-tree")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("no publishable files found", done.stdout)


class Environment(unittest.TestCase):
    def test_the_self_tests_ignore_the_operators_gate_settings(self):
        """Caller gate env vars (e.g. RELEASE_CHECK_REFS) must not leak into throwaway test repos."""
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

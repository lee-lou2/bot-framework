"""Throwaway git repositories for the release-gate tests. Synthetic data only."""

import os
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
SCANNER = SCRIPTS / "private_terms_check.py"
GATE = SCRIPTS / "release-check.sh"
PRIVATE = {
    "GIT_AUTHOR_NAME": "Alice Example",
    "GIT_AUTHOR_EMAIL": "alice@example.test",
    "GIT_COMMITTER_NAME": "Alice Example",
    "GIT_COMMITTER_EMAIL": "alice@example.test",
}
PUBLIC = {
    "GIT_AUTHOR_EMAIL": "1+alice@users.noreply.github.com",
    "GIT_COMMITTER_EMAIL": "1+alice@users.noreply.github.com",
}


class GitShim:
    """A `git` first on PATH that fails when one of its arguments equals `subcommand` and otherwise forwards to the real git."""

    def __init__(self, case, subcommand):
        self.dir = Path(tempfile.mkdtemp(prefix="gate-shim-"))
        case.addCleanup(lambda: shutil.rmtree(self.dir, ignore_errors=True))
        script = self.dir / "git"
        script.write_text(
            f'#!/bin/sh\nfor arg in "$@"; do\n  case "$arg" in {subcommand}) exit 1 ;; esac\ndone\nexec "{shutil.which("git")}" "$@"\n'
        )
        script.chmod(script.stat().st_mode | stat.S_IEXEC)

    def env(self):
        return {"PATH": f"{self.dir}:{os.environ['PATH']}"}


class Repo:
    """A temporary git repository plus a term list that lives outside it."""

    def __init__(self, case, bare_name=None):
        self.repo = Path(tempfile.mkdtemp(prefix="gate-repo-")).resolve()
        self.outside = Path(tempfile.mkdtemp(prefix="gate-list-")).resolve()
        case.addCleanup(lambda: shutil.rmtree(self.repo, ignore_errors=True))
        case.addCleanup(lambda: shutil.rmtree(self.outside, ignore_errors=True))
        self.git("init", "-q")
        self.git("config", "commit.gpgsign", "false")

    def env(self, extra=None):
        return {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, **PRIVATE, **(extra or {})}

    def git(self, *args, env=None, ok=(0,), cwd=None):
        done = subprocess.run(
            ["git", *args], cwd=cwd or self.repo, env=self.env(env), capture_output=True, text=True
        )
        assert done.returncode in ok, done.stderr
        return done.stdout

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def commit(self, message="work", env=None):
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message, env=env)

    def terms(self, *lines):
        path = self.outside / "terms.txt"
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return path

    def scan(self, *lines, terms=None, git_dir=None, cwd=None, env=None):
        listing = terms or self.terms(*lines)
        cmd = [sys.executable, "-I", str(SCANNER), str(listing)]
        if git_dir:
            cmd += ["--git-dir", str(git_dir)]
        return subprocess.run(
            cmd, cwd=cwd or self.repo, env=self.env(env), capture_output=True, text=True
        )

    def gate(self, *args, env=None, drop=("BOT_PRIVATE_TERMS_FILE", "BOT_TEST_DATABASE_URL", "BOT_TEST_REQUIRE_DB")):
        merged = self.env(env)
        # Strip caller RELEASE_CHECK_* and BOT_* env vars unless explicitly passed (keep RELEASE_CHECK_IN_SELFTEST to prevent recursive selftest).
        for name in [*drop, *(n for n in merged if n.startswith(("RELEASE_CHECK_", "BOT_")))]:
            if name == "RELEASE_CHECK_IN_SELFTEST":
                continue
            if not env or name not in env:
                merged.pop(name, None)
        return subprocess.run(
            ["bash", str(GATE), *args], cwd=self.repo, env=merged, capture_output=True, text=True
        )


def everything(done):
    """All the text a run printed."""
    return done.stdout + done.stderr


def committed(case, message="work"):
    """A repository with one public-author commit (the minimal thing the gate steps can run on)."""
    repo = Repo(case)
    repo.write("a.txt", "fine\n")
    repo.commit(message, env=PUBLIC)
    return repo

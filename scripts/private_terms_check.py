#!/usr/bin/env python3
"""Scan what would be published for entries of a private term list.

The list holds private names, so it must live outside this repository. Its path is the first argument.
Nothing from the list or from a matching name is ever printed: a finding shows an opaque location
(commit/blob/tag object ids; a path only when the path itself holds no term, otherwise a short hash of it),
a line number and the line number of the list entry. Output and error text are both safe to share.

Scanned: every publishable working-tree file (name, link target, content), staged content that differs from the
working tree, and everything reachable from the refs of a git directory: ref names, nested tag objects, commits
(author, committer, message), every blob version and every path that ever existed.

Usage: private_terms_check.py <list> [--git-dir <bare or mirror repository>]
Exit status: 0 clean, 1 findings, 2 usage, list or git error (a scan that cannot read something is a failure).

List format (one entry per line, blank lines and lines starting with '#' are ignored):
  <text>       case-insensitive substring
  word:<text>  case-insensitive, whole word (neighbours are not letters or digits)
  re:<regex>   case-insensitive Python regular expression
  cs:<regex>   case-sensitive Python regular expression (for identifier shapes)
"""

import hashlib
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPORT_LIMIT = 200
GIT_ENV = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}


class Fail(Exception):
    """A condition that must fail the gate. The message never carries repository or list content."""


class Terms:
    """Compiled entries; `hits` returns the list line numbers whose entry matches a text."""

    def __init__(self, path):
        self.plain, self.regex = [], []
        try:
            raw = Path(path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise Fail(f"failed to read private terms list: {error.__class__.__name__}")
        for number, line in enumerate(raw.splitlines(), 1):
            entry = line.strip()
            if not entry or entry.startswith("#"):
                continue
            try:
                self._add(number, entry)
            except re.error:
                raise Fail(f"invalid regex on line {number} of private terms list")
        if not (self.plain or self.regex):
            raise Fail("private terms list has no entries")

    def _add(self, number, entry):
        if entry.startswith("word:"):
            word = entry[5:].strip()
            if word:
                pattern = rf"(?<![A-Za-z0-9]){re.escape(word)}(?![A-Za-z0-9])"
                self.regex.append((number, re.compile(pattern, re.IGNORECASE)))
        elif entry.startswith("re:"):
            self.regex.append((number, re.compile(entry[3:], re.IGNORECASE)))
        elif entry.startswith("cs:"):
            self.regex.append((number, re.compile(entry[3:])))
        else:
            self.plain.append((number, entry.casefold()))

    @property
    def count(self):
        return len(self.plain) + len(self.regex)

    def hits(self, text):
        folded = text.casefold()
        found = {n for n, term in self.plain if term in folded}
        found.update(n for n, rx in self.regex if rx.search(text))
        return sorted(found)


def decode(data):
    return data.decode("utf-8", "replace")


def digest8(text):
    return hashlib.sha256(text.encode("utf-8", "replace")).hexdigest()[:8]


class Report:
    """Rows are (location, list lines). A location is printed only if it holds no term and no control character."""

    def __init__(self, terms):
        self.terms = terms
        self.rows = set()

    def path(self, text):
        """A path (or ref name) as it may be shown: itself when it is clean, otherwise an opaque hash."""
        if self.terms.hits(text) or any(ord(c) < 32 or ord(c) == 127 for c in text):
            return f"path#{digest8(text)}"
        return text

    def add(self, where, hits):
        # `where` is fixed text, hex object ids and paths that went through `path()`; nothing else reaches it.
        if hits:
            self.rows.add((where, tuple(hits)))

    def text(self, where, data):
        """Scan a blob or file body: per line for text, one row for binary data."""
        if b"\0" in data:
            self.add(f"{where} (binary)", self.terms.hits(decode(data)))
            return
        for number, line in enumerate(decode(data).split("\n"), 1):
            self.add(f"{where}:{number}", self.terms.hits(line))

    def print(self):
        rows = sorted(self.rows)
        for where, hits in rows[:REPORT_LIMIT]:
            print(f"  {where}  [list line {', '.join(map(str, hits))}]")
        if len(rows) > REPORT_LIMIT:
            print(f"  ... and {len(rows) - REPORT_LIMIT} more")


def git(args, cwd=None, git_dir=None):
    cmd = ["git", "-c", "core.quotepath=false"]
    if git_dir:
        cmd += ["--git-dir", str(git_dir)]
    done = subprocess.run(cmd + args, cwd=cwd, capture_output=True, env=GIT_ENV)
    if done.returncode != 0:
        raise Fail(f"git {args[0]} failed")
    return done.stdout


def objects(git_dir, oids):
    """Yield (oid, type, body) for every requested object; an unreadable object fails the scan."""
    if not oids:
        return
    with tempfile.TemporaryFile() as feed:
        feed.write(("\n".join(oids) + "\n").encode())
        feed.seek(0)
        cmd = ["git", "--git-dir", str(git_dir), "cat-file", "--batch"]
        with subprocess.Popen(cmd, stdin=feed, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=GIT_ENV) as proc:
            seen = 0
            while header := proc.stdout.readline():
                parts = header.split()
                if len(parts) != 3:
                    raise Fail("failed to read git object")
                body = proc.stdout.read(int(parts[2]))
                proc.stdout.read(1)
                seen += 1
                yield decode(parts[0]), decode(parts[1]), body
            if proc.wait() != 0 or seen != len(oids):
                raise Fail("failed to read git object")


def git_dir_of(root):
    return Path(decode(git(["rev-parse", "--absolute-git-dir"], cwd=root)).strip())


def publishable_files(root):
    """Everything a `git add -A` would publish, plus Cargo.lock even when it is ignored."""
    listing = git(["ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=root)
    names = {os.fsdecode(entry) for entry in listing.split(b"\0") if entry}
    if (root / "Cargo.lock").is_file():
        names.add("Cargo.lock")
    return sorted(names)


def scan_worktree(root, rep):
    names = publishable_files(root)
    for name in names:
        path = root / name
        shown = rep.path(name)
        rep.add(f"worktree {shown} (name)", rep.terms.hits(name))
        if path.is_symlink():
            rep.add(f"worktree {shown} (symlink target)", rep.terms.hits(os.readlink(path)))
        elif path.is_file():
            rep.text(f"worktree {shown}", path.read_bytes())
    return len(names)


def scan_index(root, rep):
    """Staged content that the working tree no longer shows would still be committed: read those blobs too."""
    changed = {os.fsdecode(p) for p in git(["diff", "--name-only", "-z", "--no-renames"], cwd=root).split(b"\0") if p}
    if not changed:
        return 0
    staged = {}
    for entry in git(["ls-files", "-s", "-z"], cwd=root).split(b"\0"):
        meta, _, name = entry.partition(b"\t")
        fields = meta.split(b" ")
        if name and len(fields) == 3 and fields[0] != b"160000" and os.fsdecode(name) in changed:
            staged[decode(fields[1])] = os.fsdecode(name)
    for oid, _, body in objects(git_dir_of(root), list(staged)):
        rep.text(f"index {rep.path(staged[oid])}", body)
    return len(staged)


def scan_history(git_dir, rep):
    """Everything reachable from the refs of `git_dir`, which must be a complete (not shallow) repository."""
    if decode(git(["rev-parse", "--is-shallow-repository"], git_dir=git_dir)).strip() != "false":
        raise Fail("shallow clone has truncated history; run on a full clone")
    format = "--format=%(refname)%1f%(objectname)%1f%(objecttype)"
    refs = [line.split("\x1f") for line in decode(git(["for-each-ref", format], git_dir=git_dir)).splitlines()]
    for name, _, _ in refs:
        rep.add(f"history ref {rep.path(name)} (name)", rep.terms.hits(name))
    # Tag objects (including nested tags): name, tagger, message; skip header lines 1-2 (object, type).
    pending, done = [oid for _, oid, kind in refs if kind == "tag"], set()
    while pending:
        todo = [oid for oid in dict.fromkeys(pending) if oid not in done]
        pending = []
        for oid, _, body in objects(git_dir, todo):
            done.add(oid)
            text = decode(body)
            rep.add(f"history tag {oid[:8]} (name/message)", rep.terms.hits("\n".join(text.split("\n")[2:])))
            target = re.search(r"^object ([0-9a-f]+)$", text, re.MULTILINE)
            if target and re.search(r"^type tag$", text, re.MULTILINE):
                pending.append(target.group(1))
    first_path = {}
    for line in decode(git(["rev-list", "--objects", "--all"], git_dir=git_dir)).splitlines():
        oid, _, path = line.partition(" ")
        first_path.setdefault(oid, path)
    commits = 0
    for oid, kind, body in objects(git_dir, list(first_path)):
        if kind == "commit":
            head, _, message = decode(body).partition("\n\n")
            lines = [h for h in head.split("\n") if not h.startswith(("tree ", "parent "))]
            rep.add(f"history commit {oid[:8]} (author/message)", rep.terms.hits("\n".join(lines) + "\n" + message))
            commits += 1
        elif kind == "blob":
            where = rep.path(first_path[oid]) if first_path[oid] else "(no path)"
            rep.text(f"history blob {oid[:8]} {where}", body)
    # Include deleted/renamed paths across all commits (and each merge parent).
    log = git(["log", "--all", "--full-history", "-m", "--no-renames", "--name-only", "--format=", "-z"], git_dir=git_dir)
    for path in sorted({os.fsdecode(p) for p in log.split(b"\0") if p}):
        rep.add(f"history path {rep.path(path)} (name)", rep.terms.hits(path))
    return len(refs), commits, len(first_path)


def parse(argv):
    args, git_dir, items = [], None, iter(argv)
    for item in items:
        if item == "--git-dir":
            git_dir = next(items, None)
            if git_dir is None:
                raise Fail("--git-dir requires a repository path")
        else:
            args.append(item)
    if len(args) != 1:
        raise Fail("Usage: private_terms_check.py <list> [--git-dir <repository>]")
    return args[0], git_dir


def run(argv):
    listing, history_dir = parse(argv)
    top = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, env=GIT_ENV)
    if top.returncode != 0:
        raise Fail("must run inside a git repository")
    root = Path(decode(top.stdout).strip()).resolve()
    path = Path(listing).resolve()
    if path == root or root in path.parents:
        raise Fail("private terms list is inside the repository; keep it outside")
    rep = Report(Terms(path))
    files = scan_worktree(root, rep)
    staged = scan_index(root, rep)
    refs, commits, blobs = scan_history(Path(history_dir) if history_dir else git_dir_of(root), rep)
    print(
        f"  scanned terms: {rep.terms.count} · worktree files: {files} · staged-only index: {staged} · "
        f"refs: {refs} · commits: {commits} · history objects: {blobs}"
    )
    if rep.rows:
        print(f"found {len(rep.rows)} private term match(es) (terms and names omitted)")
        rep.print()
        return 1
    print("no private terms found")
    return 0


def main(argv):
    try:
        return run(argv)
    except Fail as error:
        print(error, file=sys.stderr)
    except Exception as error:  # Never leak repository content even on unexpected errors.
        print(f"scanner internal error: {error.__class__.__name__}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

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
            raise Fail(f"금칙어 목록을 읽지 못했다: {error.__class__.__name__}")
        for number, line in enumerate(raw.splitlines(), 1):
            entry = line.strip()
            if not entry or entry.startswith("#"):
                continue
            try:
                self._add(number, entry)
            except re.error:
                raise Fail(f"금칙어 목록 {number}번째 줄의 정규식이 올바르지 않다")
        if not (self.plain or self.regex):
            raise Fail("금칙어 목록에 항목이 없다")

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
            return f"경로#{digest8(text)}"
        return text

    def add(self, where, hits):
        # `where` is fixed text, hex object ids and paths that went through `path()`; nothing else reaches it.
        if hits:
            self.rows.add((where, tuple(hits)))

    def text(self, where, data):
        """Scan a blob or file body: per line for text, one row for binary data."""
        if b"\0" in data:
            self.add(f"{where} (이진)", self.terms.hits(decode(data)))
            return
        for number, line in enumerate(decode(data).split("\n"), 1):
            self.add(f"{where}:{number}", self.terms.hits(line))

    def print(self):
        rows = sorted(self.rows)
        for where, hits in rows[:REPORT_LIMIT]:
            print(f"  {where}  [목록 {', '.join(map(str, hits))}번째 줄]")
        if len(rows) > REPORT_LIMIT:
            print(f"  … 외 {len(rows) - REPORT_LIMIT}건")


def git(args, cwd=None, git_dir=None):
    cmd = ["git", "-c", "core.quotepath=false"]
    if git_dir:
        cmd += ["--git-dir", str(git_dir)]
    done = subprocess.run(cmd + args, cwd=cwd, capture_output=True, env=GIT_ENV)
    if done.returncode != 0:
        raise Fail(f"git {args[0]} 실패")
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
                    raise Fail("git 객체를 읽지 못했다")
                body = proc.stdout.read(int(parts[2]))
                proc.stdout.read(1)
                seen += 1
                yield decode(parts[0]), decode(parts[1]), body
            if proc.wait() != 0 or seen != len(oids):
                raise Fail("git 객체를 읽지 못했다")


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
        rep.add(f"작업 트리 {shown} (이름)", rep.terms.hits(name))
        if path.is_symlink():
            rep.add(f"작업 트리 {shown} (링크 대상)", rep.terms.hits(os.readlink(path)))
        elif path.is_file():
            rep.text(f"작업 트리 {shown}", path.read_bytes())
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
        rep.text(f"인덱스 {rep.path(staged[oid])}", body)
    return len(staged)


def scan_history(git_dir, rep):
    """Everything reachable from the refs of `git_dir`, which must be a complete (not shallow) repository."""
    if decode(git(["rev-parse", "--is-shallow-repository"], git_dir=git_dir)).strip() != "false":
        raise Fail("얕은 복제라 이력이 잘려 있다. 전체 복제본에서 검사해야 한다")
    format = "--format=%(refname)%1f%(objectname)%1f%(objecttype)"
    refs = [line.split("\x1f") for line in decode(git(["for-each-ref", format], git_dir=git_dir)).splitlines()]
    for name, _, _ in refs:
        rep.add(f"이력 참조 {rep.path(name)} (이름)", rep.terms.hits(name))
    # 태그 객체(태그가 태그를 가리키는 중첩 포함): 이름·태거·메시지. 첫 두 줄(object·type)은 식별자와 종류라 건너뛴다.
    pending, done = [oid for _, oid, kind in refs if kind == "tag"], set()
    while pending:
        todo = [oid for oid in dict.fromkeys(pending) if oid not in done]
        pending = []
        for oid, _, body in objects(git_dir, todo):
            done.add(oid)
            text = decode(body)
            rep.add(f"이력 태그 {oid[:8]} (이름·메시지)", rep.terms.hits("\n".join(text.split("\n")[2:])))
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
            rep.add(f"이력 commit {oid[:8]} (작성자·메시지)", rep.terms.hits("\n".join(lines) + "\n" + message))
            commits += 1
        elif kind == "blob":
            where = rep.path(first_path[oid]) if first_path[oid] else "(경로 없음)"
            rep.text(f"이력 blob {oid[:8]} {where}", body)
    # 삭제·이름 변경으로 지금은 없는 경로까지: 모든 커밋(병합은 부모마다)이 바꾼 경로의 합집합
    log = git(["log", "--all", "--full-history", "-m", "--no-renames", "--name-only", "--format=", "-z"], git_dir=git_dir)
    for path in sorted({os.fsdecode(p) for p in log.split(b"\0") if p}):
        rep.add(f"이력 경로 {rep.path(path)} (이름)", rep.terms.hits(path))
    return len(refs), commits, len(first_path)


def parse(argv):
    args, git_dir, items = [], None, iter(argv)
    for item in items:
        if item == "--git-dir":
            git_dir = next(items, None)
            if git_dir is None:
                raise Fail("--git-dir 뒤에 저장소 경로가 필요하다")
        else:
            args.append(item)
    if len(args) != 1:
        raise Fail("사용법: private_terms_check.py <금칙어 목록> [--git-dir <저장소>]")
    return args[0], git_dir


def run(argv):
    listing, history_dir = parse(argv)
    top = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, env=GIT_ENV)
    if top.returncode != 0:
        raise Fail("git 저장소 안에서 실행해야 한다")
    root = Path(decode(top.stdout).strip()).resolve()
    path = Path(listing).resolve()
    if path == root or root in path.parents:
        raise Fail("금칙어 목록이 저장소 안에 있다. 목록은 비공개이니 저장소 밖에 둔다")
    rep = Report(Terms(path))
    files = scan_worktree(root, rep)
    staged = scan_index(root, rep)
    refs, commits, blobs = scan_history(Path(history_dir) if history_dir else git_dir_of(root), rep)
    print(
        f"  항목 {rep.terms.count}개 · 작업 트리 파일 {files}개 · 따로 바뀐 인덱스 {staged}개 · "
        f"참조 {refs}개 · 커밋 {commits}개 · 이력 객체 {blobs}개를 검사했다"
    )
    if rep.rows:
        print(f"금칙어 발견 {len(rep.rows)}곳 (낱말과 이름은 싣지 않는다)")
        rep.print()
        return 1
    print("금칙어 없음")
    return 0


def main(argv):
    try:
        return run(argv)
    except Fail as error:
        print(error, file=sys.stderr)
    except Exception as error:  # 예상 밖의 오류도 저장소 내용을 싣지 않는다
        print(f"검사기 내부 오류: {error.__class__.__name__}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

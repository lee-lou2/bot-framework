#!/usr/bin/env python3
"""Tests for private_terms_check.py. Only synthetic terms and throwaway git repositories are used."""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testing_repo import SCANNER, GitShim, Repo, everything  # noqa: E402

TERM = "quokka"


def rows(done):
    """The finding rows of a run: `(location, list lines)`."""
    return re.findall(r"^  (.+?)  \[목록 ([0-9, ]+)번째 줄\]$", done.stdout, re.MULTILINE)


def where(done):
    return [w for w, _ in rows(done)]


class WorkingTree(unittest.TestCase):
    def test_clean_tree_passes(self):
        repo = Repo(self)
        repo.write("a.txt", "nothing to see\n")
        repo.commit()
        done = repo.scan("zebrafish")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("금칙어 없음", done.stdout)

    def test_reports_file_line_and_list_line_but_never_the_term(self):
        repo = Repo(self)
        repo.write("src/a.txt", "one\ntwo Zebrafish three\n")
        repo.commit()
        done = repo.scan("# header", TERM, "zebrafish")
        self.assertEqual(done.returncode, 1)
        self.assertIn(("작업 트리 src/a.txt:2", "3"), rows(done))
        self.assertNotIn("zebrafish", everything(done).lower())

    def test_new_untracked_and_hidden_files_are_scanned(self):
        repo = Repo(self)
        repo.write("tracked.txt", "fine\n")
        repo.commit()
        repo.write("new/untracked.txt", f"has {TERM}\n")
        repo.write(".config/hidden.toml", f"x = '{TERM}'\n")
        found = where(repo.scan(TERM))
        self.assertIn("작업 트리 new/untracked.txt:1", found)
        self.assertIn("작업 트리 .config/hidden.toml:1", found)

    def test_ignored_files_are_not_published_so_not_scanned_except_the_lock_file(self):
        repo = Repo(self)
        repo.write(".gitignore", "/local/\nCargo.lock\n")
        repo.write("local/notes.txt", f"{TERM}\n")
        repo.write("Cargo.lock", f'name = "{TERM}"\n')
        repo.commit()
        found = where(repo.scan(TERM))
        self.assertIn("작업 트리 Cargo.lock:1", found)
        self.assertFalse([w for w in found if "local/notes" in w])

    def test_a_name_or_link_target_with_a_term_is_reported_without_being_printed(self):
        repo = Repo(self)
        repo.write(f"{TERM}-notes.txt", "fine\n")
        repo.write("clean-name.txt", "fine\n")
        os.symlink(f"{TERM}-target", repo.repo / "alias")
        repo.commit()
        done = repo.scan(TERM)
        found = where(done)
        self.assertTrue([w for w in found if re.fullmatch(r"작업 트리 경로#[0-9a-f]{8} \(이름\)", w)], found)
        self.assertIn("작업 트리 alias (링크 대상)", found, "깨끗한 이름은 그대로 보인다")
        self.assertNotIn(TERM, everything(done).lower())

    def test_binary_files_are_reported_without_a_line_number(self):
        repo = Repo(self)
        (repo.repo / "blob.bin").write_bytes(b"\x00\x01" + TERM.encode() + b"\x00")
        repo.commit()
        self.assertIn("작업 트리 blob.bin (이진)", where(repo.scan(TERM)))


class Index(unittest.TestCase):
    def test_staged_content_the_working_tree_no_longer_shows_is_still_scanned(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        repo.write("a.txt", f"fine\nstaged {TERM}\n")
        repo.git("add", "a.txt")
        repo.write("a.txt", "fine\n")  # 작업 트리에서는 지웠지만 커밋하면 인덱스의 내용이 올라간다
        done = repo.scan(TERM)
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("인덱스 a.txt:2", where(done))
        self.assertFalse([w for w in where(done) if w.startswith("작업 트리")])

    def test_a_clean_index_adds_nothing(self):
        repo = Repo(self)
        repo.write("a.txt", f"{TERM}\n")
        repo.commit()
        self.assertFalse([w for w in where(repo.scan(TERM)) if w.startswith("인덱스")])


class EntryKinds(unittest.TestCase):
    def setUp(self):
        self.repo = Repo(self)
        self.repo.write("a.txt", "zed-x\nzedx\nZED.\ntag QX1234 and qx1234\n")
        self.repo.commit()

    def lines(self, *entries):
        return {w.split()[-1] for w in where(self.repo.scan(*entries)) if w.startswith("작업 트리")}

    def test_word_entries_stop_at_letters_and_digits_only(self):
        self.assertEqual(self.lines("word:zed"), {"a.txt:1", "a.txt:3"})

    def test_plain_entries_are_case_insensitive_substrings(self):
        self.assertEqual(self.lines("ZED"), {"a.txt:1", "a.txt:2", "a.txt:3"})

    def test_regex_entries_ignore_case_and_cs_entries_do_not(self):
        shape = r"\bQX[0-9]{4}\b"
        self.assertEqual(self.lines("re:" + shape), {"a.txt:4"})
        self.assertEqual(self.lines("cs:" + shape), {"a.txt:4"})
        self.repo.write("b.txt", "only lower qx5678\n")
        self.repo.commit()
        self.assertEqual(self.lines("cs:" + shape), {"a.txt:4"})

    def test_comments_and_blank_lines_are_ignored(self):
        done = self.repo.scan("# zed", "", "   ", "never-present-term")
        self.assertEqual(done.returncode, 0, done.stdout)


class History(unittest.TestCase):
    def test_a_term_removed_later_is_still_found_in_a_blob_of_the_history(self):
        repo = Repo(self)
        repo.write("a.txt", f"fine\n{TERM} here\n")
        repo.commit("first")
        repo.write("a.txt", "fine\n")
        repo.commit("second")
        done = repo.scan(TERM)
        self.assertEqual(done.returncode, 1)
        self.assertTrue([w for w in where(done) if re.fullmatch(r"이력 blob [0-9a-f]{8} a\.txt:2", w)], where(done))
        self.assertFalse([w for w in where(done) if w.startswith("작업 트리")])

    def test_a_path_that_existed_only_in_the_past_is_found_without_being_printed(self):
        repo = Repo(self)
        repo.write(f"{TERM}/old.txt", "fine\n")
        repo.write("b.txt", "also fine\n")
        repo.commit()
        (repo.repo / TERM / "old.txt").unlink()
        repo.commit("remove")
        done = repo.scan(TERM)
        self.assertEqual(done.returncode, 1)
        self.assertTrue([w for w in where(done) if w.startswith("이력 경로 경로#")], where(done))
        self.assertNotIn(TERM, everything(done).lower())

    def test_commit_message_author_branch_and_tag_are_scanned(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit(f"mentions {TERM} in the message")
        found = where(repo.scan(TERM))
        self.assertTrue([w for w in found if re.fullmatch(r"이력 commit [0-9a-f]{8} \(작성자·메시지\)", w)], found)
        repo.write("b.txt", "fine\n")
        repo.commit("second", env={"GIT_AUTHOR_NAME": "Quokka Person"})
        self.assertEqual(len([w for w in where(repo.scan("person")) if "commit" in w]), 1)
        repo.git("branch", "feature-wombat")
        refs = [w for w in where(repo.scan("wombat")) if w.startswith("이력 참조")]
        self.assertTrue(refs and re.fullmatch(r"이력 참조 경로#[0-9a-f]{8} \(이름\)", refs[0]), refs)
        repo.git("tag", "-a", "v1", "-m", "release for the numbat team")
        tags = where(repo.scan("numbat"))
        self.assertTrue([w for w in tags if re.fullmatch(r"이력 태그 [0-9a-f]{8} \(이름·메시지\)", w)], tags)

    def test_a_tag_pointing_at_a_tag_is_followed(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        repo.git("tag", "-a", "inner", "-m", "the numbat team signed this")
        inner = repo.git("rev-parse", "inner").strip()
        outer = subprocess.run(
            ["git", "mktag"],
            cwd=repo.repo,
            env=repo.env(),
            input=f"object {inner}\ntype tag\ntag outer\ntagger A <1+a@users.noreply.github.com> 1700000000 +0000\n\nouter message\n",
            capture_output=True,
            text=True,
        )
        self.assertEqual(outer.returncode, 0, outer.stderr)
        repo.git("update-ref", "refs/tags/outer", outer.stdout.strip())
        repo.git("tag", "-d", "inner")  # 안쪽 태그는 바깥 태그를 통해서만 닿는다
        done = repo.scan("numbat")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertTrue([w for w in where(done) if w.startswith("이력 태그")], where(done))

    def test_content_that_exists_only_in_a_conflict_resolution_is_found(self):
        repo = Repo(self)
        repo.write("a.txt", "base\n")
        repo.commit("base")
        repo.git("checkout", "-q", "-b", "side")
        repo.write("a.txt", "side\n")
        repo.commit("side")
        repo.git("checkout", "-q", "-")
        repo.write("a.txt", "main\n")
        repo.commit("main")
        repo.git("merge", "-q", "side", ok=(0, 1))
        repo.write("a.txt", f"resolved with {TERM}\n")
        repo.commit("merge")
        repo.write("a.txt", "resolved again\n")
        repo.commit("clean up")
        done = repo.scan(TERM)
        self.assertTrue([w for w in where(done) if re.fullmatch(r"이력 blob [0-9a-f]{8} a\.txt:1", w)], where(done))
        self.assertFalse([w for w in where(done) if w.startswith("작업 트리")])

    def test_refs_are_scanned_even_when_head_points_at_nothing(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        repo.git("tag", "-a", "v1", "-m", f"tagged by the {TERM} team")
        repo.git("checkout", "-q", "--orphan", "unborn")  # HEAD가 아직 없는 브랜치를 가리킨다
        repo.git("rm", "-q", "-rf", ".")
        done = repo.scan(TERM)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertTrue([w for w in where(done) if w.startswith("이력 태그")], where(done))

    def test_blob_line_numbers_follow_the_file_in_every_version(self):
        repo = Repo(self)
        lines = [f"line {n}\n" for n in range(1, 41)]
        repo.write("a.txt", "".join(lines[:29] + [f"{TERM} old\n"] + lines[30:]))
        repo.commit("base")
        repo.write("a.txt", "".join(lines[:29] + [f"{TERM} new\n"] + lines[30:]))
        repo.commit("edit")
        found = [w for w in where(repo.scan(TERM)) if w.startswith("이력 blob")]
        self.assertEqual(len(found), 2, found)
        self.assertTrue(all(w.endswith("a.txt:30") for w in found), found)

    def test_a_repository_without_commits_still_scans_the_tree(self):
        repo = Repo(self)
        repo.write("a.txt", f"{TERM}\n")
        done = repo.scan(TERM)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("커밋 0개", done.stdout)
        self.assertIn("작업 트리 a.txt:1", where(done))

    def test_a_pinned_clone_is_scanned_instead_of_the_live_repository(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        pinned = repo.outside / "pinned.git"
        repo.git("clone", "-q", "--mirror", "--no-local", str(repo.repo), str(pinned), cwd=repo.outside)
        repo.write("later.txt", f"{TERM}\n")
        repo.commit(f"later {TERM}")
        found = where(repo.scan(TERM, git_dir=pinned))
        self.assertIn("작업 트리 later.txt:1", found, "작업 트리는 늘 본다")
        self.assertFalse([w for w in found if w.startswith("이력")], "이력은 고정한 복제본 것만 본다")


class OutputHygiene(unittest.TestCase):
    def test_nothing_from_the_list_or_from_names_is_printed_on_any_channel(self):
        repo = Repo(self)
        repo.write(f"{TERM}-dir/{TERM}-file.txt", f"{TERM}\n")
        repo.write("clean.txt", f"line with {TERM}\n")
        os.symlink(f"{TERM}-target", repo.repo / "alias")
        repo.commit(
            f"{TERM} commit message",
            env={"GIT_AUTHOR_NAME": f"{TERM} Person", "GIT_AUTHOR_EMAIL": f"{TERM}@example.test"},
        )
        repo.git("branch", f"{TERM}-branch")
        repo.git("tag", "-a", f"{TERM}-tag", "-m", f"{TERM} tag message")
        (repo.repo / f"{TERM}-dir" / f"{TERM}-file.txt").unlink()
        repo.commit("remove")
        done = repo.scan(TERM, f"re:{TERM}.*message", f"word:{TERM}")
        self.assertEqual(done.returncode, 1)
        self.assertGreaterEqual(len(rows(done)), 8, done.stdout)
        self.assertNotIn(TERM, everything(done).lower(), "금칙어·이름이 출력에 나오면 안 된다")
        for needle in (str(repo.repo), str(repo.outside)):
            self.assertNotIn(needle, everything(done), "절대 경로가 출력에 나오면 안 된다")

    def test_error_text_never_carries_repository_content(self):
        repo = Repo(self)
        repo.write(f"{TERM}.txt", f"{TERM}\n")
        repo.commit(f"{TERM} message")
        blob = repo.git("rev-parse", f"HEAD:{TERM}.txt").strip()
        loose = repo.repo / ".git" / "objects" / blob[:2] / blob[2:]
        loose.chmod(0o644)
        loose.write_bytes(b"garbage")
        done = repo.scan(TERM)
        self.assertEqual(done.returncode, 2, done.stdout + done.stderr)
        self.assertNotIn(TERM, everything(done).lower())


class ListHandling(unittest.TestCase):
    def test_a_list_inside_the_repository_is_refused(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        inside = repo.write("terms.txt", f"{TERM}\n")
        repo.commit()
        done = repo.scan(terms=inside)
        self.assertEqual(done.returncode, 2)
        self.assertIn("저장소 안에 있다", done.stderr)

    def test_a_missing_empty_or_broken_list_is_an_error_without_echoing_entries(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        self.assertEqual(repo.scan(terms=repo.outside / "missing.txt").returncode, 2)
        done = repo.scan("# only a comment")
        self.assertEqual(done.returncode, 2)
        self.assertIn("항목이 없다", done.stderr)
        done = repo.scan(TERM, "re:(unclosed")
        self.assertEqual(done.returncode, 2)
        self.assertIn("2번째 줄", done.stderr)
        self.assertNotIn("unclosed", done.stderr)

    def test_usage_errors_and_a_directory_that_is_not_a_repository(self):
        done = subprocess.run([sys.executable, "-I", str(SCANNER)], capture_output=True, text=True)
        self.assertEqual(done.returncode, 2)
        outside = tempfile.mkdtemp(prefix="gate-nogit-")
        self.addCleanup(lambda: shutil.rmtree(outside, ignore_errors=True))
        listing = Path(outside) / "t.txt"
        listing.write_text(f"{TERM}\n", encoding="utf-8")
        env = {**os.environ, "GIT_CEILING_DIRECTORIES": str(Path(outside).parent)}
        done = subprocess.run(
            [sys.executable, "-I", str(SCANNER), str(listing)], cwd=outside, env=env, capture_output=True, text=True
        )
        self.assertEqual(done.returncode, 2)


class Failures(unittest.TestCase):
    """A scan that cannot see everything is a failure, never a pass."""

    def test_a_shallow_clone_is_refused(self):
        source = Repo(self)
        source.write("a.txt", "one\n")
        source.commit("one")
        source.write("a.txt", "two\n")
        source.commit("two")
        shallow = source.outside / "shallow"
        source.git("clone", "-q", "--depth", "1", f"file://{source.repo}", str(shallow), cwd=source.outside)
        done = source.scan("nothing-here", cwd=shallow)
        self.assertEqual(done.returncode, 2, done.stdout + done.stderr)
        self.assertIn("얕은 복제", done.stderr)

    def test_a_git_directory_that_is_not_a_repository_fails(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        done = repo.scan("nothing-here", git_dir=repo.outside / "nope.git")
        self.assertEqual(done.returncode, 2, done.stdout)
        self.assertEqual(done.stdout, "")

    def test_a_failing_git_listing_is_a_failure_not_an_empty_scan(self):
        for sub in ("ls-files", "diff", "rev-parse", "for-each-ref", "rev-list", "log"):
            repo = Repo(self)
            repo.write("a.txt", f"{TERM}\n")
            repo.commit()
            repo.write("a.txt", f"{TERM} changed\n")  # 인덱스 검사(diff)도 일을 하게 한다
            done = repo.scan(TERM, env=GitShim(self, sub).env())
            self.assertEqual(done.returncode, 2, f"{sub}: {done.stdout}{done.stderr}")
            self.assertEqual(rows(done), [], sub)
            self.assertNotIn("금칙어 없음", done.stdout, sub)

    def test_a_git_that_dies_while_reading_objects_is_a_failure_not_a_clean_scan(self):
        repo = Repo(self)
        repo.write("a.txt", f"{TERM}\n")
        repo.commit()
        done = repo.scan(TERM, env=GitShim(self, "cat-file").env())
        self.assertEqual(done.returncode, 2, done.stdout + done.stderr)
        self.assertNotIn("금칙어 없음", done.stdout)
        self.assertEqual(rows(done), [])

    def test_an_unreadable_object_fails_instead_of_being_skipped(self):
        repo = Repo(self)
        repo.write("a.txt", "fine\n")
        repo.commit()
        blob = repo.git("rev-parse", "HEAD:a.txt").strip()
        loose = repo.repo / ".git" / "objects" / blob[:2] / blob[2:]
        loose.chmod(0o644)
        loose.write_bytes(b"garbage")
        done = repo.scan("nothing-here")
        self.assertEqual(done.returncode, 2, done.stdout + done.stderr)
        self.assertIn("git 객체를 읽지 못했다", done.stderr, "내부 오류가 아니라 읽지 못했다는 분명한 실패여야 한다")
        self.assertNotIn("내부 오류", done.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=1)

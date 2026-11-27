#!/usr/bin/env python3
"""Tests for file hygiene, module headers, workspace layering/pins, and example manifest checks in .github/scripts/check_contracts.py."""

import contextlib
import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("check_contracts", ROOT / ".github/scripts/check_contracts.py")
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class Files(unittest.TestCase):
    def run_check(self, files, ignore=None, remove=()):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        subprocess.run(["git", "init", "-q"], cwd=folder.name, check=True)
        if ignore:
            Path(folder.name, ".gitignore").write_text(ignore, encoding="utf-8")
        for name, text in files.items():
            target = Path(folder.name, name)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8")
        if remove:
            subprocess.run(["git", "add", *remove], cwd=folder.name, check=True)
            for name in remove:
                Path(folder.name, name).unlink()
        with contextlib.chdir(folder.name):
            checker.check_files()

    def test_an_editor_or_merge_leftover_in_the_public_list_is_refused_by_name(self):
        for name in ("secret_sweep.rs.bak", "main.orig", "patch.rej", ".lib.rs.swp", "notes.txt~"):
            with self.assertRaises(ValueError) as caught:
                self.run_check({"fine.rs": "fn x() {}\n", name: "fn x() {}\n"})
            self.assertIn(name, str(caught.exception))

    def test_names_that_only_look_similar_and_ignored_or_deleted_files_pass(self):
        names = ("backup.rs", "bak.txt", "original.md", "reject.rs", "swap.rs", "a~b.txt", "x.bak.rs", "nobak", "myorig", "norej", "noswp")
        self.run_check({n: "fn x() {}\n" for n in names} | {"old.rs.bak": "x\n"}, ignore="*.bak\n")
        self.run_check({"gone.rs": "fn x() {}\n"}, remove=("gone.rs",))

    def test_non_test_crate_sources_require_a_non_empty_module_header_on_line_one(self):
        self.run_check({
            "crates/bot-demo/src/lib.rs": "//! Responsibility.\npub fn x() {}\n",
            "crates/bot-demo/src/tests.rs": "fn helper() {}\n",
            "crates/bot-demo/src/foo_tests.rs": "fn helper() {}\n",
            "crates/bot-demo/src/tests/sub.rs": "fn helper() {}\n",
            "crates/bot-demo/src/testing/world.rs": "fn helper() {}\n",
            "crates/bot-demo/tests/e2e.rs": "fn helper() {}\n",
        })
        for body in ("fn x() {}\n", "\n//! not on line one\n", "//!\n", "//!   \n", ""):
            with self.assertRaises(ValueError) as caught:
                self.run_check({"crates/bot-demo/src/sub/mod.rs": body})
            self.assertIn("crates/bot-demo/src/sub/mod.rs", str(caught.exception))
            self.assertIn("//!", str(caught.exception))

    def test_multiple_file_violations_are_collected_together(self):
        with self.assertRaises(ValueError) as caught:
            self.run_check({
                "a.orig": "x\n",
                "crates/bot-demo/src/a.rs": "fn a() {}\n",
                "crates/bot-demo/src/b.rs": "//!\n" + "fn b() {}\n" * 605,
            })
        msg = str(caught.exception)
        self.assertIn("a.orig", msg)
        self.assertIn("crates/bot-demo/src/a.rs", msg)
        self.assertIn("exceeds 600 lines", msg)
        self.assertIn("missing non-empty `//!`", msg)


class Workspace(unittest.TestCase):
    def make_ws(self, root_toml, crates):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        root = Path(folder.name)
        (root / "Cargo.toml").write_text(root_toml, encoding="utf-8")
        for rel, text in crates.items():
            path = root / rel / "Cargo.toml"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        return root

    def test_real_workspace_passes(self):
        self.assertEqual(checker.workspace_problems(ROOT), [])
        with contextlib.chdir(ROOT):
            checker.check_workspace()

    def test_unpinned_dependencies_and_middle_crate_cross_deps_are_refused(self):
        root_toml = (
            '[workspace]\nmembers = ["crates/bot-core", "crates/bot-store", "crates/bot-broker", "crates/bot-cli"]\n'
            '[workspace.dependencies]\n'
            'bot-core = { path = "crates/bot-core" }\n'
            'bot-store = { path = "crates/bot-store" }\n'
            'serde = "=1.0.229"\n'
            'loose = "1.0"\n'
        )
        crates = {
            "crates/bot-core": '[package]\nname = "bot-core"\n[dependencies]\nbot-store.workspace = true\n',
            "crates/bot-store": '[package]\nname = "bot-store"\n[dependencies]\nbot-core.workspace = true\n',
            "crates/bot-broker": '[package]\nname = "bot-broker"\n[dependencies]\nbot-core.workspace = true\nbot-store.workspace = true\n[dev-dependencies]\nfoo = "^0.2.0"\n',
            "crates/bot-cli": '[package]\nname = "bot-cli"\n[dependencies]\nbot-core.workspace = true\nbot-store.workspace = true\n',
        }
        found = checker.workspace_problems(self.make_ws(root_toml, crates))
        self.assertTrue(any("[workspace.dependencies].loose" in p for p in found), found)
        self.assertTrue(any("`bot-core` must not depend" in p for p in found), found)
        self.assertTrue(any("middle crate `bot-broker` must not depend on `bot-store`" in p for p in found), found)
        self.assertTrue(any("[dev-dependencies].foo" in p for p in found), found)


class Examples(unittest.TestCase):
    def check_example(self, extra_files):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "schemas").mkdir()
            (root / "schemas/v1").symlink_to(ROOT / "schemas/v1")
            for rel, content in extra_files.items():
                target = root / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content, encoding="utf-8")
            with contextlib.chdir(root):
                checker.check_examples()

    def test_valid_plain_skill_and_lock_yaml_pass(self):
        self.check_example({"examples/profiles/demo/skills/agenda/SKILL.md": "List today's agenda.\n"})
        self.check_example({
            "examples/profiles/demo/skills/agenda/SKILL.md": "---\nname: agenda\ndescription: List agenda\nlicense: MIT\ncompatibility: local\nmetadata:\n  author: team\n---\nAgenda\n",
            "examples/profiles/demo/skills/agenda/tools.lock.yaml": "version: 1\ntools: []\n",
        })
        with contextlib.chdir(ROOT):
            checker.check_examples()

    def test_invalid_skill_front_matter_is_refused(self):
        for bad in (
            "---\nname: agenda\nallowed-tools: Bash\n---\nAgenda\n",
            "---\nname: other\n---\nAgenda\n",
            "---\nname: agenda\nAgenda\n",
            "---",
            "---\nextra: 1\n---\nAgenda\n",
        ):
            with self.assertRaises(Exception, msg=bad):
                self.check_example({"examples/profiles/demo/skills/agenda/SKILL.md": bad})


if __name__ == "__main__":
    unittest.main()

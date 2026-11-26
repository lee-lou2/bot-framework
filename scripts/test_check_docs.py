#!/usr/bin/env python3
"""Tests for the documents-versus-code rules in .github/scripts/docs_contract.py (and the `check_docs()` hook in check_contracts.py).
A synthetic repository that keeps every rule is broken one way at a time; the last tests hold the repository's own documents to the same rules."""

import contextlib
import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


docs = load("docs_contract", ".github/scripts/docs_contract.py")
contracts = load("check_contracts", ".github/scripts/check_contracts.py")

# The two settings that exist in the code but that no document names yet (the security change documents them).
KNOWN_PENDING = ("private_pass", "private_egress")

GOOD = {
    "Cargo.toml": '[workspace]\nmembers = ["crates/bot-core", "crates/bot-cli"]\n',
    "README.md": "# demo\n",
    "AGENTS.md": "# agents\n\n| 크레이트 | 책임 |\n|---|---|\n| `bot-core` | 어휘 |\n| `bot-cli` | 바이너리 |\n\n읽기: `crates/bot-core/src/lib.rs`\n",
    "docs/architecture.md": "# 구조\n\n## 크레이트\n\n| 크레이트 | 책임 |\n|---|---|\n| bot-core | 어휘 |\n| bot-cli | 바이너리 |\n",
    "docs/admin.md": "# 관리\n\n`GET /v1/admin/me`\n",
    "docs/guide.md": "# 안내\n\n설정 `agent_name`·`nested_key`·`version`·`kind`·`model`·`gateway`, 명령 `bot validate`·`bot add skill`, 플래그 `--check-db`, 도구 `memory.share`.\n",
    "schemas/v1/profile.schema.json": '{"properties": {"agent_name": {"properties": {"nested_key": {}}}}}',
    "crates/bot-core/src/lib.rs": "//! core\n",
    "crates/bot-cli/src/host.rs": 'let version = top.remove("version");\nlet gateway = top.remove("gateway");\n#[cfg(test)]\nmod tests { let x = top.remove("only_in_tests"); }\n',
    "crates/bot-cli/src/execution.rs": 'only(fields, &["kind", "model"], path)?;\n',
    "crates/bot-admin/src/settings.rs": "// no keys\n",
    "crates/bot-admin/src/lib.rs": '.route("/v1/admin/me", get(me))\n.route("/ui", get(index))\n.route("/ui/{*path}", get(asset))\n',
    "crates/bot-profile/src/tools.rs": 'const RESERVED_TOOL_IDS: &[&str] = &[\n    "memory.share",\n];\n',
    "crates/bot-cli/src/cli.rs": """
#[derive(Subcommand)]
pub enum Command {
    /// check
    Validate,
    /// add things
    Add(AddArgs),
}

#[derive(Args)]
pub struct AddArgs {
    #[command(subcommand)]
    pub what: AddKind,
}

#[derive(Subcommand)]
pub enum AddKind {
    /// a skill
    Skill {
        #[arg(long)]
        check_db: bool,
    },
}
""",
}


class Repo:
    """A temporary repository: `GOOD` with some files replaced (None deletes one)."""

    def __init__(self, changes=None):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        files = {**GOOD, **(changes or {})}
        for name, text in files.items():
            if text is not None:
                path = self.root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")

    def problems(self):
        return docs.docs_problems(self.root)

    def messages(self):
        return [message for _, _, message in self.problems()]

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.dir.cleanup()


class Names(unittest.TestCase):
    def test_a_consistent_repository_passes(self):
        with Repo() as repo:
            self.assertEqual(repo.problems(), [])
            docs.check(repo.root)

    def test_a_name_counts_only_as_a_whole_token(self):
        self.assertTrue(docs.mentioned("pass", "`pass`는 넘긴다"))
        self.assertFalse(docs.mentioned("private", "private_pass"))
        self.assertFalse(docs.mentioned("pass", "bypass"))
        self.assertFalse(docs.mentioned("a", "a-b"))
        self.assertTrue(docs.mentioned("a.b", "x a.b y"))

    def test_an_undocumented_schema_property_is_named_with_its_schema(self):
        with Repo({"docs/guide.md": GOOD["docs/guide.md"].replace("`nested_key`·", "")}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("schema", "nested_key")])
            self.assertIn("profile.schema.json", repo.messages()[0])

    def test_host_keys_come_from_remove_calls_and_only_arrays_but_not_from_tests(self):
        with Repo() as repo:
            self.assertEqual(sorted(docs.host_keys(repo.root)), ["gateway", "kind", "model", "version"])

    def test_an_undocumented_host_key_is_reported(self):
        with Repo({"crates/bot-cli/src/execution.rs": 'only(fields, &["kind", "secret_name"], path)?;\n'}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("host", "secret_name")])

    def test_commands_and_flags_are_read_from_the_clap_definitions(self):
        with Repo() as repo:
            commands, flags = docs.cli_commands(repo.root)
            self.assertEqual(commands, {"validate", "add", "add skill"})
            self.assertEqual(flags, {"--check-db"})
        self.assertEqual(docs.kebab("ProfileBundle"), "profile-bundle")

    def test_an_undocumented_subcommand_or_flag_is_reported(self):
        cli = GOOD["crates/bot-cli/src/cli.rs"].replace("check_db: bool,", 'check_db: bool,\n        #[arg(long = "dry-run")]\n        dry: bool,')
        with Repo({"crates/bot-cli/src/cli.rs": cli.replace("Validate,", "Validate,\n    /// more\n    Serve,")}) as repo:
            self.assertEqual(sorted((k, t) for k, t, _ in repo.problems()), [("cli", "--dry-run"), ("cli", "bot serve")])

    def test_an_undocumented_admin_route_is_reported_but_the_static_ui_routes_are_not_checked(self):
        lib = GOOD["crates/bot-admin/src/lib.rs"] + '.route(\n    "/v1/admin/holds",\n    get(holds),\n)\n'
        with Repo({"crates/bot-admin/src/lib.rs": lib}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("route", "/v1/admin/holds")])

    def test_a_reserved_tool_id_must_appear_somewhere_in_the_documents(self):
        tools = 'const RESERVED_TOOL_IDS: &[&str] = &[\n    "memory.share",\n    "schedule.deliver",\n];\n'
        with Repo({"crates/bot-profile/src/tools.rs": tools}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("tool", "schedule.deliver")])

    def test_the_crate_tables_must_list_exactly_the_workspace_members(self):
        with Repo({"docs/architecture.md": GOOD["docs/architecture.md"].replace("| bot-cli | 바이너리 |\n", "| bot-ghost | 없음 |\n")}) as repo:
            self.assertEqual(sorted(t for k, t, _ in repo.problems() if k == "crate"), ["bot-cli", "bot-ghost"])
        with Repo({"AGENTS.md": GOOD["AGENTS.md"].replace("| `bot-core` | 어휘 |\n", "")}) as repo:
            self.assertEqual([t for k, t, _ in repo.problems() if k == "crate"], ["bot-core"])


class Paths(unittest.TestCase):
    def test_github_anchors(self):
        self.assertEqual(docs.slug("Claude Code가 브로커에 연결하는 방법"), "claude-code가-브로커에-연결하는-방법")
        self.assertEqual(docs.slug("원격 도구 서버(Streamable HTTP)"), "원격-도구-서버streamable-http")
        self.assertEqual(docs.anchors("# A\n## A\n```\n# not a heading\n```\n"), {"a", "a-1"})

    def test_paths_links_and_anchors_that_point_at_nothing_are_reported(self):
        text = (
            "# 안내\n\n`crates/bot-core/src/lib.rs` `crates/nope.rs` `crates/bot-core/src/{lib,gone}.rs` `crates/bot-core/src/lib.rs:12` `crates/<x>/y` `docs/*.md`\n"
            "[a](admin.md#관리) [b](missing.md) [c](admin.md#nowhere) [d](#안내) [e](https://example.com/x#y)\n"
        )
        with Repo({"docs/guide.md": GOOD["docs/guide.md"] + text}) as repo:
            found = sorted(message.split(": ", 1)[1] for _, _, message in repo.problems())
            self.assertEqual(
                found,
                ["`crates/bot-core/src/gone.rs` does not exist", "`crates/nope.rs` does not exist", "link `admin.md#nowhere` points at a missing heading", "link `missing.md` points at a missing file"],
            )


class Exceptions(unittest.TestCase):
    def test_a_line_without_a_reason_is_rejected(self):
        allowed, bad = docs.parse_allow("# comment\n\nroute | /v1/admin/x | because\nroute | /v1/admin/y |\njust text\n")
        self.assertEqual(list(allowed), [("route", "/v1/admin/x")])
        self.assertEqual(len(bad), 2)
        self.assertIn(":4:", bad[0])

    def test_an_exception_excuses_exactly_its_problem_and_an_unneeded_one_is_reported(self):
        problems = [("route", "/v1/admin/x", "x is missing"), ("host", "k", "k is missing")]
        allowed = {("route", "/v1/admin/x"): 3, ("route", "/v1/admin/old"): 4}
        kept = docs.filter_allowed(problems, allowed)
        self.assertEqual(kept[0], "k is missing")
        self.assertIn("docs-allow.txt:4:", kept[1])
        self.assertEqual(len(kept), 2)

    def test_check_raises_one_error_listing_every_problem(self):
        with Repo({"docs/guide.md": "# 안내\n"}) as repo, self.assertRaises(ValueError) as caught:
            docs.check(repo.root)
        self.assertGreater(len(str(caught.exception).splitlines()), 3)


class Repository(unittest.TestCase):
    """The repository's own documents. Only the two settings in KNOWN_PENDING may be missing; when the security change has documented them,
    delete KNOWN_PENDING, expect no problem and call `check_docs()` from `main()` in check_contracts.py."""

    def test_nothing_but_the_known_pending_items_disagrees(self):
        allowed, bad = docs.parse_allow((ROOT / docs.ALLOW_FILE).read_text(encoding="utf-8"))
        self.assertEqual(bad, [])
        left = docs.filter_allowed(docs.docs_problems(ROOT), allowed)
        self.assertEqual([m for m in left if not any(f"`{name}`" in m for name in KNOWN_PENDING)], [])

    def test_the_hook_in_check_contracts_runs_the_same_rules(self):
        with contextlib.chdir(ROOT):
            try:
                contracts.check_docs()
            except ValueError as error:
                lines = str(error).splitlines()[1:]
                self.assertTrue(lines and all(any(f"`{name}`" in line for name in KNOWN_PENDING) for line in lines), lines)


if __name__ == "__main__":
    unittest.main()

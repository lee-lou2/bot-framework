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

GOOD = {
    "Cargo.toml": '[workspace]\nmembers = ["crates/bot-core", "crates/bot-cli"]\n',
    "README.md": "# demo\n",
    "README.ko.md": "# 데모\n\n[English](README.md) | 한국어\n",
    "AGENTS.md": "# agents\n\n| Crate | Responsibility |\n|---|---|\n| `bot-core` | Vocabulary |\n| `bot-cli` | Binary |\n\nRead: `crates/bot-core/src/lib.rs`\n",
    "docs/architecture.md": "# Architecture\n\n## Crates\n\n| Crate | Responsibility |\n|---|---|\n| bot-core | Vocabulary |\n| bot-cli | Binary |\n",
    "docs/ko/architecture.md": "# 아키텍처\n\n[English](../architecture.md) | 한국어\n\n## 크레이트\n\n| Crate | Responsibility |\n|---|---|\n| bot-core | Vocabulary |\n| bot-cli | Binary |\n",
    "docs/admin.md": "# Admin\n\n`GET /v1/admin/me`\n",
    "docs/ko/admin.md": "# 관리자\n\n[English](../admin.md) | 한국어\n\n`GET /v1/admin/me`\n",
    "docs/guide.md": "# Guide\n\nSettings `agent_name`, `nested_key`, `version`, `kind`, `model`, `gateway`, `root`, `tool_call_max_secs`, `team_id`, `renamed_key`, `endpoint_kind`, commands `bot validate`, `bot add skill`, flag `--check-db`, tool `memory.share`.\n",
    "docs/ko/guide.md": "# 가이드\n\n[English](../guide.md) | 한국어\n\n설정 `agent_name`, `nested_key`, `version`, `kind`, `model`, `gateway`, `root`, `tool_call_max_secs`, `team_id`, `renamed_key`, `endpoint_kind`, 명령 `bot validate`, `bot add skill`, 플래그 `--check-db`, 도구 `memory.share`.\n",
    "schemas/v1/profile.schema.json": '{"properties": {"agent_name": {"properties": {"nested_key": {}}}}}',
    "crates/bot-core/src/lib.rs": "//! core\n",
    "crates/bot-cli/src/host.rs": 'let version = top.remove("version");\nlet gateway = top.remove("gateway");\n#[cfg(test)]\nmod tests { let x = top.remove("only_in_tests"); }\n',
    "crates/bot-cli/src/execution.rs": 'only(fields, &["kind", "model"], path)?;\n',
    "crates/bot-cli/src/execution/provider.rs": "// no keys\n",
    "crates/bot-cli/src/embeddings.rs": "// no keys\n",
    "crates/bot-cli/src/memory_judge.rs": "// no keys\n",
    "crates/bot-cli/src/install.rs": '#[cfg(test)]\nuse place::remove_tree;\nfn parse() { fields.keys().find(|k| !matches!(k.as_str(), | "root",)); matches!(part, "" | ".."); }\n#[cfg(test)]\nmod tests;\n',
    "crates/bot-cli/src/tool_limits.rs": 'match key.as_str() {\n    "tool_call_max_secs" => {}\n    other => {}\n}\n',
    "crates/bot-gateway/src/host.rs": "#[derive(Clone, Debug, Deserialize)]\n#[serde(deny_unknown_fields)]\npub struct SlackWorkspace {\n    /// the team\n    pub team_id: String,\n    #[serde(default, rename = \"renamed_key\")]\n    hidden_name: u8,\n}\n\n#[derive(Debug)]\nstruct NotConfig {\n    not_a_key: u8,\n}\n",
    "crates/bot-gateway/src/host/openai.rs": 'fn parse(mut map: Map) { let _ = map.remove("endpoint_kind"); }\n',
    "crates/bot-admin/src/settings.rs": "// no keys\n",
    "crates/bot-admin/src/lib.rs": '.route("/v1/admin/me", get(me))\n.route("/ui", get(index))\n.route("/ui/{*path}", get(asset))\n#[cfg(test)]\nmod tests { fn t() { r.route("/v1/admin/test_only", get(me)); } }\n',
    "crates/bot-core/src/builtin.rs": 'pub const SHARE: ModelTool = ModelTool {\n    id: "memory.share",\n};\n',
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

#[cfg(test)]
mod tests {
    #[derive(Args)]
    pub struct Dummy {
        #[arg(long)]
        test_only_flag: bool,
    }
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
        self.assertTrue(docs.mentioned("pass", "`pass`는 전달"))
        self.assertFalse(docs.mentioned("private", "private_pass"))
        self.assertFalse(docs.mentioned("pass", "bypass"))
        self.assertFalse(docs.mentioned("a", "a-b"))
        self.assertTrue(docs.mentioned("a.b", "x a.b y"))

    def test_an_undocumented_schema_property_is_named_with_its_schema(self):
        with Repo({"docs/guide.md": GOOD["docs/guide.md"].replace("`nested_key`, ", "")}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("schema", "nested_key")])
            self.assertIn("profile.schema.json", repo.messages()[0])

    def test_host_keys_are_read_by_the_way_each_reader_rejects_the_rest_but_not_from_tests(self):
        with Repo() as repo:
            self.assertEqual(
                sorted(docs.host_keys(repo.root)),
                ["endpoint_kind", "gateway", "kind", "model", "renamed_key", "root", "team_id", "tool_call_max_secs", "version"],
            )

    def test_only_the_arrays_an_only_call_or_an_allowed_list_holds_are_keys(self):
        reader = (
            'const KINDS: &[&str] = &["not_a_key"];\n'
            'only(\n    fields,\n    // comment with unbalanced ) and char \')\'\n    &[\n        "multi",\n        "line",\n    ],\n    &format!("({})", path),\n)?;\n'
            'let allowed: &[&str] = match kind {\n    A => &["bound_a"],\n    B => &["bound_b", "bound_a"],\n};\nonly(fields, allowed, path)?;\n'
            'let other = &["not_a_key_either"];\n'
        )
        self.assertEqual(docs.only_lists(reader), ["multi", "line", "bound_a", "bound_b", "bound_a"])

    def test_the_source_is_cut_at_the_test_module_not_at_a_test_only_item(self):
        text = '#[cfg(test)]\nuse helper;\nlet kept = top.remove("kept");\n#[cfg(test)]\nmod tests;\nlet gone = top.remove("gone");\n'
        self.assertEqual(docs.removed_keys(docs.non_test_text(text)), ["kept"])
        self.assertEqual(docs.removed_keys(docs.non_test_text('x\n#[cfg(test)]\npub(crate) mod t { top.remove("gone"); }')), [])

    def test_a_registered_reader_that_has_moved_stops_the_check(self):
        with Repo({"crates/bot-cli/src/install.rs": None}) as repo, self.assertRaises(FileNotFoundError):
            docs.host_keys(repo.root)

    def test_an_undocumented_host_key_is_reported(self):
        with Repo({"crates/bot-cli/src/execution.rs": 'only(fields, &["kind", "secret_name"], path)?;\n'}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("host", "secret_name"), ("ko-host", "secret_name")])

    def test_a_host_key_must_be_written_as_code_not_only_as_a_word_in_prose(self):
        prose = GOOD["docs/guide.md"].replace("`gateway`", "gateway") + "\n(gateway appears only in prose)\n"
        with Repo({"docs/guide.md": prose}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("host", "gateway")])
            self.assertIn("written as code", repo.messages()[0])
        for fixed in ("`gateway`", "\n```json\n{\"gateway\": {}}\n```\n"):
            with Repo({"docs/guide.md": prose + fixed}) as repo:
                self.assertEqual(repo.problems(), [])
        self.assertEqual(docs.code_text("a `b` c\n```\nd\n```\n`e`"), "```\nd\n```\n`b`\n`e`")

    def test_commands_and_flags_are_read_from_the_clap_definitions(self):
        with Repo() as repo:
            commands, flags = docs.cli_commands(repo.root)
            self.assertEqual(commands, {"validate", "add", "add skill"})
            self.assertEqual(flags, {"--check-db"})
        self.assertEqual(docs.kebab("ProfileBundle"), "profile-bundle")

    def test_an_undocumented_subcommand_or_flag_is_reported(self):
        cli = GOOD["crates/bot-cli/src/cli.rs"].replace("check_db: bool,", 'check_db: bool,\n        #[arg(long = "dry-run")]\n        dry: bool,')
        with Repo({"crates/bot-cli/src/cli.rs": cli.replace("Validate,", "Validate,\n    /// more\n    Serve,")}) as repo:
            self.assertEqual(
                sorted((k, t) for k, t, _ in repo.problems()),
                [("cli", "--dry-run"), ("cli", "bot serve"), ("ko-cli", "--dry-run"), ("ko-cli", "bot serve")],
            )

    def test_an_undocumented_admin_route_is_reported_but_the_static_ui_routes_are_not_checked(self):
        lib = GOOD["crates/bot-admin/src/lib.rs"].replace('#[cfg(test)]', '.route(\n    "/v1/admin/holds",\n    get(holds),\n)\n#[cfg(test)]')
        with Repo({"crates/bot-admin/src/lib.rs": lib}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("route", "/v1/admin/holds"), ("ko-route", "/v1/admin/holds")])

    def test_a_reserved_tool_id_must_appear_somewhere_in_the_documents(self):
        tools = 'pub const SHARE: ModelTool = ModelTool {\n    id: "memory.share",\n};\npub const DELIVER_TOOL: &str = "schedule.deliver";\n'
        with Repo({"crates/bot-core/src/builtin.rs": tools}) as repo:
            self.assertEqual([(k, t) for k, t, _ in repo.problems()], [("tool", "schedule.deliver"), ("ko-tool", "schedule.deliver")])

    def test_the_crate_tables_must_list_exactly_the_workspace_members(self):
        with Repo({"docs/architecture.md": GOOD["docs/architecture.md"].replace("| bot-cli | Binary |\n", "| bot-ghost | Missing |\n")}) as repo:
            self.assertEqual(sorted(t for k, t, _ in repo.problems() if k == "crate"), ["bot-cli", "bot-ghost"])
        with Repo({"AGENTS.md": GOOD["AGENTS.md"].replace("| `bot-core` | Vocabulary |\n", "")}) as repo:
            self.assertEqual([t for k, t, _ in repo.problems() if k == "crate"], ["bot-core"])
        with Repo({"AGENTS.md": GOOD["AGENTS.md"] + "| `bot-two-words2` | x |\n"}) as repo:
            self.assertEqual([t for k, t, _ in repo.problems() if k == "crate"], ["bot-two-words2"])


class Paths(unittest.TestCase):
    def test_github_anchors_and_indented_fences(self):
        self.assertEqual(docs.slug("How Claude Code connects to the broker"), "how-claude-code-connects-to-the-broker")
        self.assertEqual(docs.slug("Remote tool server(Streamable HTTP)"), "remote-tool-serverstreamable-http")
        self.assertEqual(docs.slug("Café résumé (UTF-8)"), "café-résumé-utf-8")
        self.assertEqual(docs.anchors("# A\n## A\n  ```\n  # indented fence heading\n  ```\n"), {"a", "a-1"})

    def test_paths_links_anchors_and_html_media_that_point_at_nothing_are_reported(self):
        text = (
            "\n`crates/bot-core/src/lib.rs` `crates/nope.rs` `crates/bot-core/src/{lib,gone}.rs` `crates/bot-core/src/lib.rs:12` `crates/<x>/y` `docs/*.md`\n"
            "[a](admin.md#admin) [b](missing.md) [c](admin.md#nowhere) [d](#guide) [e](https://example.com/x#y)\n"
            '<picture><source srcset="diagrams/missing-dark.svg 1x"><img src="diagrams/missing-light.svg"></picture>\n'
        )
        ko_text = "\n[a](admin.md#%EA%B4%80%EB%A6%AC%EC%9E%90) [c](admin.md#%EA%B4%80%EB%A6%AC%EC%9E%90) [d](#%EA%B0%80%EC%9D%B4%EB%93%9C)\n"
        with Repo({"docs/guide.md": GOOD["docs/guide.md"] + text, "docs/ko/guide.md": GOOD["docs/ko/guide.md"] + ko_text}) as repo:
            found = sorted(message.split(": ", 1)[1] for k, _, message in repo.problems() if k == "path")
            self.assertEqual(
                found,
                [
                    "`crates/bot-core/src/gone.rs` does not exist",
                    "`crates/nope.rs` does not exist",
                    "`src` `diagrams/missing-light.svg` points at a missing file",
                    "`srcset` `diagrams/missing-dark.svg` points at a missing file",
                    "link `admin.md#nowhere` points at a missing heading",
                    "link `missing.md` points at a missing file",
                ],
            )

    def test_line_ranges_are_stripped_and_code_blocks_are_not_read(self):
        text = (
            "`crates/bot-core/src/lib.rs:1` `crates/bot-core/src/lib.rs:1-2` `crates/gone.rs:10-20`\n"
            "```text\n`crates/in-a-block.rs` [x](nowhere.md) <img src=\"nope.svg\">\n```\n"
        )
        with Repo({"docs/guide.md": GOOD["docs/guide.md"] + text}) as repo:
            self.assertEqual([m.split(": ", 1)[1] for _, _, m in repo.problems()], ["`crates/gone.rs` does not exist"])


class KoreanParity(unittest.TestCase):
    def test_missing_or_extra_korean_files_are_reported(self):
        with Repo({"README.ko.md": None, "docs/ko/guide.md": None, "docs/ko/extra.md": "# 여분\n"}) as repo:
            sync = {(k, t) for k, t, _ in repo.problems() if k == "ko-sync"}
            self.assertIn(("ko-sync", "README.ko.md"), sync)
            self.assertIn(("ko-sync", "docs/ko/guide.md"), sync)
            self.assertIn(("ko-sync", "docs/ko/extra.md"), sync)

    def test_heading_anchor_and_english_link_drift_in_korean_docs_are_reported(self):
        with Repo({
            "docs/guide.md": GOOD["docs/guide.md"] + "\n## Details\nSee [Admin](admin.md#admin).\n",
            "docs/ko/guide.md": GOOD["docs/ko/guide.md"] + "\n### 세부\n[Admin](../admin.md) 참고.\n",
        }) as repo:
            messages = [m for k, _, m in repo.problems() if k == "ko-sync"]
            self.assertTrue(any("heading structure differs" in m for m in messages), messages)
            self.assertTrue(any("anchored link count differs" in m for m in messages), messages)
            self.assertTrue(any("links to English doc" in m for m in messages), messages)

    def test_missing_korean_contract_tokens_and_crates_are_reported(self):
        with Repo({
            "docs/ko/guide.md": GOOD["docs/ko/guide.md"].replace("`gateway`, ", ""),
            "docs/ko/architecture.md": GOOD["docs/ko/architecture.md"].replace("| bot-cli | Binary |\n", ""),
        }) as repo:
            found = {(k, t) for k, t, _ in repo.problems()}
            self.assertIn(("ko-host", "gateway"), found)
            self.assertIn(("ko-crate", "bot-cli"), found)


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
        with Repo({"docs/guide.md": "# Guide\n"}) as repo, self.assertRaises(ValueError) as caught:
            docs.check(repo.root)
        self.assertGreater(len(str(caught.exception).splitlines()), 3)


class Repository(unittest.TestCase):
    """The repository's own documents must agree with the code with zero unexcused discrepancies."""

    def test_repository_documents_and_code_agree(self):
        self.assertIn("memory.share", docs.reserved_tools(ROOT))
        self.assertIn("schedule.deliver", docs.reserved_tools(ROOT))
        allowed, bad = docs.parse_allow((ROOT / docs.ALLOW_FILE).read_text(encoding="utf-8"))
        self.assertEqual(bad, [])
        self.assertEqual(docs.filter_allowed(docs.docs_problems(ROOT), allowed), [])

    def test_every_file_that_rejects_unknown_host_keys_is_a_registered_reader(self):
        """A host section read in a file the check does not know would be a silent blind spot again."""
        registered = {relative for _, files in docs.HOST_READERS for relative in files}
        candidates = [
            *(ROOT / "crates/bot-cli/src").rglob("*.rs"),
            *(ROOT / "crates/bot-gateway/src/host").rglob("*.rs"),
            ROOT / "crates/bot-gateway/src/host.rs",
            *(ROOT / "crates/bot-admin/src").rglob("*.rs"),
        ]
        rejecting = set()
        for path in candidates:
            if path.name == "tests.rs" or path.name.endswith("_tests.rs") or "tests" in path.parts:
                continue
            text = docs.non_test_text(path.read_text(encoding="utf-8"))
            if "unknown key" in text.lower() or "unknown top-level key" in text.lower() or "deny_unknown_fields" in text:
                rejecting.add(str(path.relative_to(ROOT)))
        self.assertEqual(sorted(rejecting - registered), [])
        for relative in registered:
            self.assertTrue((ROOT / relative).is_file(), relative)

    def test_the_hook_in_check_contracts_runs_the_same_rules(self):
        with contextlib.chdir(ROOT):
            contracts.check_docs()


if __name__ == "__main__":
    unittest.main()

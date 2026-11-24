#!/usr/bin/env python3
"""The gate's `INTEGRATION_TESTS` list must hold every test file that can silently skip for a missing real tool.

A test that cannot find claude, srt, python3 or the container VM prints `skipped: …` and passes, so only the `integration` step — which runs the
listed files in required mode (`*_CANARY=required`) and fails on any skip marker — can tell whether it really ran. A test file that skips but is
missing from the list would pass the gate without ever running. This checks the source: a test file is «skip-capable» when it uses `*_CANARY`,
`have_srt` or prints `skipped:` (except for the database skip, which `BOT_TEST_REQUIRE_DB=1` already turns into a failure) — directly or through a helper
function that does (resolved by name over the package's `tests/` tree, wrapper functions included). Only a `#[test]` function that can skip makes its
test file «skip-capable» — a helper module that merely defines the skipping function is not a use.
"""

import re
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "release-check.sh"

MARKER = re.compile(r"BOT_[A-Z0-9_]*CANARY|\bhave_srt|skipped:")
# The database skip is forced into a failure by `BOT_TEST_REQUIRE_DB=1` (the `test` step) — it is not what this guards.
DATABASE = re.compile(r"BOT_TEST_DATABASE_URL|BOT_TEST_REQUIRE_DB")
FN = re.compile(r"^(?P<indent> *)(?:pub(?:\([a-z]+\))? +)?(?:async +)?(?:unsafe +)?fn +(?P<name>[A-Za-z_]\w*)")
IMPL = re.compile(r"^impl\b(?:<[^>]*>)? *(?:[\w:<>, &']+ +for +)?(?P<ty>[A-Za-z_]\w*)")
CALL = re.compile(r"(?<![.\w])(?:(?P<ty>[A-Z]\w*)::)?(?P<name>[a-z_]\w*) *\(")
MOD = re.compile(r"^\s*(?:pub(?:\([a-z]+\))? +)?mod +(?P<name>[A-Za-z_]\w*) *;")
PATH = re.compile(r'^\s*#\[path *= *"(?P<path>[^"]+)"\]')


def listed(script_text):
    """`{(package, test file)}` of the gate's `INTEGRATION_TESTS` block."""
    block = re.search(r"^INTEGRATION_TESTS='\n(.*?)^'$", script_text, re.S | re.M)
    assert block, "INTEGRATION_TESTS list not found"
    return {tuple(line.split()[:2]) for line in block.group(1).splitlines() if line.strip()}


def strip_comments(text):
    """Source lines without comments (line numbers are kept)."""
    out = []
    for line in text.splitlines():
        if line.lstrip().startswith("//"):
            out.append("")
        else:
            out.append(re.sub(r" //[^\"]*$", "", line))
    return out


TEST_ATTRIBUTE = re.compile(r"^\s*#\[(?:\w+::)*test\b")


class Fn:
    def __init__(self, path, qual, name, owner, first, last, lines):
        self.path, self.qual, self.name, self.owner = path, qual, name, owner
        self.first, self.last = first, last  # 0-based, inclusive
        self.body = lines[first : last + 1]
        above = first - 1
        self.is_test = False
        while above >= 0 and (not lines[above].strip() or lines[above].lstrip().startswith("#[")):  # the attributes above the `fn` line (comments are blank here)
            self.is_test = self.is_test or bool(TEST_ATTRIBUTE.match(lines[above]))
            above -= 1


def functions(path, lines):
    """Function definitions with their body line range. Bodies end at the closing brace with the `fn` line's indentation (rustfmt layout)."""
    found, owner, owner_end = [], None, None
    for number, line in enumerate(lines):
        impl = IMPL.match(line)
        if impl:
            owner = impl.group("ty")
        elif owner and line.startswith("}"):
            owner = None
        match = FN.match(line)
        if not match:
            continue
        indent = len(match.group("indent"))
        last = None
        if line.rstrip().endswith("}") and line.count("{") == line.count("}") and "{" in line:
            last = number
        else:
            for later in range(number + 1, len(lines)):
                if lines[later] == " " * indent + "}" or lines[later].startswith(" " * indent + "}") and lines[later].strip() in ("}", "};"):
                    last = later
                    break
                follow = FN.match(lines[later])
                if follow and len(follow.group("indent")) <= indent:
                    break  # a declaration without a body
        if last is None:
            continue
        name = match.group("name")
        inside = owner if indent > 0 else None
        found.append(Fn(path, f"{inside}::{name}" if inside else name, name, inside, number, last, lines))
    return found


def marks(lines):
    """Lines that name a skip marker (the database skip does not count)."""
    return [n for n, line in enumerate(lines) if MARKER.search(line) and not DATABASE.search(line)]


def calls(lines, owner):
    """Names called in `lines`: free functions `name` and `Type::name` (`Self::` means the enclosing impl). Method calls `.name(` cannot be resolved."""
    names = set()
    for line in lines:
        for hit in CALL.finditer(line):
            ty, name = hit.group("ty"), hit.group("name")
            if ty == "Self":
                ty = owner
            names.add(f"{ty}::{name}" if ty else name)
    return names


def modules(path, lines, root):
    """Files a test file pulls in with `mod x;` (Rust's lookup rules, `#[path]` included)."""
    base = path.parent if root or path.name == "mod.rs" else path.parent / path.stem
    found, explicit = [], None
    for line in lines:
        given = PATH.match(line)
        if given:
            explicit = given.group("path")
            continue
        match = MOD.match(line)
        if not match:
            if line.strip():
                explicit = None
            continue
        if explicit:
            candidates = [path.parent / explicit]
        else:
            candidates = [base / f"{match.group('name')}.rs", base / match.group("name") / "mod.rs"]
        found += [c for c in candidates if c.is_file()][:1]
        explicit = None
    return found


def closure(path):
    seen, todo = [], [(path, True)]
    while todo:
        current, root = todo.pop()
        if current in seen:
            continue
        seen.append(current)
        todo += [(m, False) for m in modules(current, strip_comments(current.read_text(encoding="utf-8")), root)]
    return seen


def skip_capable(repo):
    """`{(package, test file): [why…]}` for the test files holding a `#[test]` function that can skip for a missing real tool."""
    verdict = {}
    for tests in sorted((repo / "crates").glob("*/tests")):
        package = tests.parent.name
        lines = {p: strip_comments(p.read_text(encoding="utf-8")) for p in tests.rglob("*.rs")}
        defs = [d for p, text in lines.items() for d in functions(p, text)]
        why = {}  # id(def) -> why it can skip
        for d in defs:
            hit = marks(d.body)
            if hit:
                why[id(d)] = f"{d.path.name}:{d.first + hit[0] + 1} `{d.qual}` uses a skip marker"
        changed = True
        while changed:
            changed = False
            names = {d.qual for d in defs if id(d) in why}
            for d in defs:
                if id(d) not in why and (hit := sorted(calls(d.body, d.owner) & names)):
                    why[id(d)] = f"{d.path.name}:{d.first + 1} `{d.qual}` calls `{hit[0]}`"
                    changed = True
        for target in sorted(tests.glob("*.rs")):
            files = closure(target)
            reasons = [why[id(d)] for d in defs if d.path in files and d.is_test and id(d) in why]
            if reasons:
                verdict[(package, target.stem)] = reasons
    return verdict


def missing(repo, script_text):
    """The skip-capable test files the list does not hold, each with the first reasons."""
    have = listed(script_text)
    return {key: why[:2] for key, why in skip_capable(repo).items() if key not in have}


SUPPORT = """\
pub fn tool() -> bool {
    let ok = which("tool").is_some();
    if !ok {
        assert!(std::env::var("BOT_TOOL_CANARY").as_deref() != Ok("required"));
        eprintln!("skipped: tool not found");
    }
    ok
}

pub struct Rig;

impl Rig {
    pub fn start() -> Option<Rig> {
        if !tool() {
            return None;
        }
        Some(Rig)
    }

    pub fn again() -> Option<Rig> {
        Self::start()
    }
}

pub fn database() -> Option<String> {
    let url = std::env::var("BOT_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
    }
    url
}
"""


class Synthetic(unittest.TestCase):
    def repo(self, files):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        base = Path(folder.name)
        for name, text in {"pkg/tests/support/mod.rs": SUPPORT, **files}.items():
            path = base / "crates" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        return base

    def flagged(self, files):
        return sorted(name for _, name in skip_capable(self.repo(files)))

    def test_a_test_calling_a_skipping_helper_is_flagged_and_one_that_does_not_is_not(self):
        files = {
            "pkg/tests/uses.rs": "mod support;\n\n#[test]\nfn works() {\n    if !support::tool() {\n        return;\n    }\n}\n",
            "pkg/tests/plain.rs": "mod support;\n\n#[test]\nfn other() {}\n",
        }
        self.assertEqual(self.flagged(files), ["uses"])

    def test_a_wrapper_that_only_calls_the_skipping_helper_is_followed(self):
        for call in ("support::Rig::start()", "support::Rig::again()"):
            files = {"pkg/tests/wrapped.rs": f"mod support;\n\n#[test]\nfn works() {{\n    let _ = {call};\n}}\n"}
            self.assertEqual(self.flagged(files), ["wrapped"], call)
        files = {  # a wrapper of the test file's own
            "pkg/tests/own.rs": "mod support;\n\nfn setup() -> bool {\n    support::tool()\n}\n\n#[test]\nfn works() {\n    setup();\n}\n"
        }
        self.assertEqual(self.flagged(files), ["own"])

    def test_a_method_call_with_the_same_name_is_not_a_call_of_the_helper(self):
        files = {"pkg/tests/method.rs": "mod support;\n\n#[test]\nfn works(other: Other) {\n    other.tool();\n}\n"}
        self.assertEqual(self.flagged(files), [])

    def test_a_marker_in_the_test_itself_flags_it_whatever_the_marker(self):
        for marker in ('eprintln!("skipped: no srt");', 'let _ = std::env::var("BOT_X_CANARY");', "if !have_srt() {\n        return;\n    }"):
            files = {"pkg/tests/direct.rs": f"#[test]\nfn works() {{\n    {marker}\n}}\n"}
            self.assertEqual(self.flagged(files), ["direct"], marker)

    def test_the_database_skip_is_not_a_reason(self):
        files = {"pkg/tests/db.rs": "mod support;\n\n#[test]\nfn works() {\n    let Some(_) = support::database() else {\n        return;\n    };\n}\n"}
        self.assertEqual(self.flagged(files), [])

    def test_comments_that_name_a_marker_do_not_count(self):
        text = '//! run with BOT_TOOL_CANARY=required; prints `skipped:` without it\n\n#[test]\n/// uses have_srt() in the doc only\nfn works() {\n    // skipped: nothing\n    let _ = 1; // BOT_X_CANARY\n}\n'
        self.assertEqual(self.flagged({"pkg/tests/talk.rs": text}), [])

    def test_attributes_decide_what_is_a_test(self):
        call = "    let _ = support::tool();\n"
        for attribute, expected in (
            ("#[test]", True),
            ("#[test]\n#[ignore = \"needs a minute\"]", True),
            ('#[tokio::test(flavor = "multi_thread", worker_threads = 4)]', True),
            ("#[test]\n/// doc between the attribute and the function", True),
            ("#[cfg(test)]", False),
            ("", False),
        ):
            files = {"pkg/tests/attr.rs": f"mod support;\n\n{attribute}\nfn works() {{\n{call}}}\n"}
            self.assertEqual(self.flagged(files), ["attr"] if expected else [], attribute)

    def test_test_functions_in_submodules_count_for_the_file_that_pulls_them_in_and_only_for_it(self):
        cases = "#[test]\nfn works() {\n    let _ = crate::support::tool();\n}\n"
        files = {
            "pkg/tests/ui.rs": 'mod support;\n#[path = "ui/cases.rs"]\nmod cases;\n',
            "pkg/tests/ui/cases.rs": cases,
            "pkg/tests/dirs.rs": "mod support;\nmod nest;\n",
            "pkg/tests/nest/mod.rs": "mod deeper;\n",
            "pkg/tests/nest/deeper.rs": cases,
            "pkg/tests/files.rs": "mod support;\nmod one;\n",
            "pkg/tests/one.rs": "mod two;\n",
            "pkg/tests/one/two.rs": cases,
            "pkg/tests/alone.rs": "mod support;\n\n#[test]\nfn nothing() {}\n",
        }
        self.assertEqual(self.flagged(files), ["dirs", "files", "ui"])

    def test_the_same_helper_name_in_another_package_is_not_mixed_up(self):
        files = {"other/tests/uses.rs": "#[test]\nfn works() {\n    tool();\n}\n"}  # `tool` is defined in `pkg`, not here
        self.assertEqual(self.flagged(files), [])

    def test_a_missing_list_entry_is_reported_with_the_reason_and_a_listed_one_is_not(self):
        repo = self.repo(
            {
                "pkg/tests/in_list.rs": "#[test]\nfn works() {\n    have_srt();\n}\n",
                "pkg/tests/left_out.rs": "mod support;\n\n#[test]\nfn works() {\n    support::tool();\n}\n",
            }
        )
        script = "A=1\nINTEGRATION_TESTS='\nbot-broker claude_canary testing\npkg in_list\n'\n"
        found = missing(repo, script)
        self.assertEqual(list(found), [("pkg", "left_out")])
        self.assertIn("calls `tool`", found[("pkg", "left_out")][0])

    def test_the_list_is_read_with_or_without_a_feature_column(self):
        script = "INTEGRATION_TESTS='\nbot-broker claude_canary testing\nbot-cli vm_serve\n\n'\n"
        self.assertEqual(listed(script), {("bot-broker", "claude_canary"), ("bot-cli", "vm_serve")})


class Gate(unittest.TestCase):
    def test_every_skip_capable_test_file_is_in_the_integration_list(self):
        found = missing(ROOT, SCRIPT.read_text(encoding="utf-8"))
        self.assertEqual(
            found,
            {},
            "다음 시험 파일은 도구가 없으면 «건너뛰고 통과»할 수 있는데 scripts/release-check.sh의 INTEGRATION_TESTS에 없다 "
            "(그러면 관문이 돌았는지 알 수 없다). 목록에 `패키지 시험파일` 줄을 더한다.",
        )

    def test_the_real_tree_has_the_files_the_check_was_written_for(self):
        wanted = skip_capable(ROOT)
        for key in (("bot-cli", "serve_restore"), ("bot-runtime", "container_cancel"), ("bot-cli", "claude_card_dm"), ("bot-broker", "srt_canary")):
            self.assertIn(key, wanted, "the analysis stopped seeing a file that can skip")
        for key in (("bot-cli", "serve_activation"), ("bot-store", "audit"), ("bot-admin", "trace_gate")):
            self.assertNotIn(key, wanted, "a file that cannot skip for a tool is flagged")

    def test_every_listed_file_exists(self):
        for package, name in sorted(listed(SCRIPT.read_text(encoding="utf-8"))):
            self.assertTrue((ROOT / "crates" / package / "tests" / f"{name}.rs").is_file(), f"{package} {name}")


if __name__ == "__main__":
    unittest.main()

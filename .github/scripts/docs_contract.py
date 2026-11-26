"""Documents must name what the code defines, and what they cite must exist.

Checked from the repository root (`python3 -I .github/scripts/docs_contract.py`, or `check_docs()` in check_contracts.py):
  schema   every property of schemas/v1/*.json appears in the documents
  host     every top-level and nested host-settings key the Rust code accepts appears in the documents
  cli      every `bot` subcommand and `--flag` defined in crates/bot-cli/src/cli.rs appears in the documents
  route    every admin API route (`/v1/admin/…`) registered in crates/bot-admin/src/lib.rs appears in docs/admin.md
  tool     every reserved built-in tool id appears in the documents
  crate    the crate tables of docs/architecture.md and AGENTS.md list exactly the workspace members
  path     every repository path in backticks, every relative link and every `#anchor` in the documents exists
What it cannot see: whether a sentence is still true. A reviewer checks the sentences.

An exception goes into .github/docs-allow.txt as `kind | token | reason`; an exception nothing needs any more is reported, so the list cannot rot."""
import json
import re
import tomllib
from pathlib import Path

ALLOW_FILE = Path(".github/docs-allow.txt")
PATH_ROOTS = ("crates/", "docs/", "schemas/", "scripts/", "examples/", ".github/", "images/")
ADMIN_ROUTES_FILE = "crates/bot-admin/src/lib.rs"
HOST_SOURCES = ("crates/bot-cli/src", "crates/bot-admin/src/settings.rs")


def read(root, relative):
    return (root / relative).read_text(encoding="utf-8")


def doc_files(root):
    return sorted((root / "docs").glob("*.md")) + [p for p in (root / "README.md", root / "AGENTS.md") if p.is_file()]


def corpus(root):
    """The text the names are looked up in: docs/*.md and README.md."""
    return "\n".join(p.read_text(encoding="utf-8") for p in doc_files(root) if p.name != "AGENTS.md")


def mentioned(name, text):
    """`name` as a whole token: ASCII letters, digits, `_` and `-` may not touch it (Korean particles may)."""
    return re.search(rf"(?<![A-Za-z0-9_-]){re.escape(name)}(?![A-Za-z0-9_-])", text) is not None


def missing(kind, names, text, where):
    """One problem per name the text does not mention; `where` says what defines it."""
    return [(kind, name, f"`{name}` ({where(name)}) is not mentioned in the documents") for name in sorted(names) if not mentioned(name, text)]


# ---- what the code defines ---------------------------------------------------------------------------------------------------------------------------------
def schema_properties(root):
    """property name -> schema files that define it."""
    found = {}

    def walk(node, source):
        if isinstance(node, dict):
            for key, value in node.items():
                if key == "properties" and isinstance(value, dict):
                    for name in value:
                        found.setdefault(name, set()).add(source)
                walk(value, source)
        elif isinstance(node, list):
            for item in node:
                walk(item, source)

    for path in sorted((root / "schemas/v1").glob("*.schema.json")):
        walk(json.loads(path.read_text(encoding="utf-8")), path.name)
    return found


def non_test_text(text):
    """The part of a Rust source before its `#[cfg(test)]` module."""
    return text.split("#[cfg(test)]", 1)[0]


def host_keys(root):
    """key -> where the Rust code accepts it: `top.remove("key")` (top level) and the string arrays handed to `only(…)` (nested)."""
    found = {}
    top = read(root, "crates/bot-cli/src/host.rs")
    for key in re.findall(r'top\.remove\("([^"]+)"\)', non_test_text(top)):
        found.setdefault(key, "crates/bot-cli/src/host.rs")
    files = []
    for source in HOST_SOURCES:
        base = root / source
        files += sorted(base.rglob("*.rs")) if base.is_dir() else [base]
    for path in files:
        if path.name == "tests.rs" or path.name.endswith("_tests.rs") or "tests" in path.parts:
            continue
        text = non_test_text(path.read_text(encoding="utf-8"))
        if "only(" not in text:
            continue
        for array in re.findall(r"&\[\s*((?:\"[^\"\n]*\"\s*,?\s*)+)\]", text):
            for key in re.findall(r'"([^"\n]*)"', array):
                found.setdefault(key, str(path.relative_to(root)))
    return found


def kebab(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "-", name).lower()


def cli_commands(root):
    """(`bot` subcommand paths like `add skill`, flags like `--check-db`) from the clap definitions."""
    text = read(root, "crates/bot-cli/src/cli.rs")
    enums = {m.group(1): m.group(2) for m in re.finditer(r"#\[derive\([^)]*Subcommand[^)]*\)\]\s*pub enum (\w+) \{(.*?)\n\}", text, re.S)}
    structs = {m.group(1): m.group(2) for m in re.finditer(r"#\[derive\([^)]*Args[^)]*\)\]\s*pub struct (\w+) \{(.*?)\n\}", text, re.S)}
    commands = set()

    def variants(body):
        # a variant is a line at one indent level that starts with a capital letter (doc comments and attributes come before it)
        return re.findall(r"^    ([A-Z]\w*)\s*(?:\((\w+)\))?\s*(?:\{|,)", body, re.M)

    def visit(enum, prefix):
        for name, args in variants(enums[enum]):
            path = f"{prefix} {kebab(name)}".strip()
            commands.add(path)
            sub = re.search(r"#\[command\(subcommand\)\]\s*pub \w+: (\w+)", structs.get(args, ""))
            if sub and sub.group(1) in enums:
                visit(sub.group(1), path)

    visit("Command", "")
    flags = set()
    # fields of an `Args` struct are `pub`, fields of an enum variant are not: take both
    for decl in re.finditer(r"#\[arg\(([^\]]*)\)\]\s*(?:pub\s+)?(\w+):", text):
        if re.search(r"\blong\b", decl.group(1)):
            custom = re.search(r'long\s*=\s*"([^"]+)"', decl.group(1))
            flags.add("--" + (custom.group(1) if custom else decl.group(2).replace("_", "-")))
    return commands, flags


def admin_routes(root):
    """The API routes; the static `/ui` routes serve files, not a contract."""
    return sorted({route for route in re.findall(r'\.route\(\s*"([^"]+)"', read(root, ADMIN_ROUTES_FILE)) if route.startswith("/v1/admin/")})


def reserved_tools(root):
    block = re.search(r"const RESERVED_TOOL_IDS: &\[&str\] = &\[(.*?)\];", read(root, "crates/bot-profile/src/tools.rs"), re.S)
    return re.findall(r'"([^"]+)"', block.group(1)) if block else []


def workspace_crates(root):
    members = tomllib.loads(read(root, "Cargo.toml"))["workspace"]["members"]
    return {Path(m).name for m in members}


def table_crates(text):
    """Crate names in the first column of a markdown table (`| bot-x |` or `` | `bot-x` | ``)."""
    return set(re.findall(r"^\|\s*`?(bot-[a-z]+)`?\s*\|", text, re.M))


# ---- paths, links and anchors ------------------------------------------------------------------------------------------------------------------------------
def slug(heading):
    """GitHub's anchor for a heading: lower case, punctuation dropped, spaces to hyphens."""
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    return re.sub(r"[^\w\- ]", "", text.strip().lower()).replace(" ", "-")


def anchors(text):
    seen, found = {}, set()
    in_fence = False
    for line in text.splitlines():
        if line.startswith("```"):
            in_fence = not in_fence
        heading = None if in_fence else re.match(r"#{1,6}\s+(.*?)\s*#*\s*$", line)
        if heading:
            base = slug(heading.group(1))
            count = seen.get(base, 0)
            seen[base] = count + 1
            found.add(base if count == 0 else f"{base}-{count}")
    return found


def expand_braces(token):
    match = re.search(r"\{([^{}]*)\}", token)
    if not match:
        return [token]
    return [r for alt in match.group(1).split(",") for r in expand_braces(token[: match.start()] + alt + token[match.end():])]


def path_problems(root, doc):
    """Repository paths in backticks, relative links and `#anchor`s of one document that point at nothing."""
    text = doc.read_text(encoding="utf-8")
    name = str(doc.relative_to(root))
    problems = []
    for token in sorted(set(re.findall(r"`([^`\n]+)`", text))):
        if not token.startswith(PATH_ROOTS) or re.search(r"[\s<>*…$|]", token):
            continue
        for candidate in expand_braces(token):
            path = re.sub(r"(?::\d+)?(?:#.*)?$", "", candidate).rstrip("/")
            if path and not (root / path).exists():
                problems.append(("path", path, f"{name}: `{path}` does not exist"))
    for target in sorted(set(re.findall(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)", text))):
        if re.match(r"[a-z][a-z0-9+.-]*:", target):
            continue
        file, _, anchor = target.partition("#")
        linked = doc if not file else (doc.parent / file).resolve()
        if file and not linked.exists():
            problems.append(("path", file, f"{name}: link `{target}` points at a missing file"))
        elif anchor and linked.suffix == ".md" and anchor not in anchors(linked.read_text(encoding="utf-8")):
            problems.append(("path", target, f"{name}: link `{target}` points at a missing heading"))
    return problems


# ---- exceptions --------------------------------------------------------------------------------------------------------------------------------------------
def parse_allow(text):
    """`kind | token | reason` per line (blank lines and `#` comments are free). Returns ({(kind, token)}, [format problems])."""
    allowed, bad = {}, []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = [part.strip() for part in line.split("|", 2)]
        if len(parts) != 3 or not all(parts):
            bad.append(f"{ALLOW_FILE}:{number}: write `kind | token | reason` (the reason is required)")
        else:
            allowed[(parts[0], parts[1])] = number
    return allowed, bad


def filter_allowed(problems, allowed):
    """Drop the excused problems; report an exception that excused nothing."""
    used = {(kind, token) for kind, token, _ in problems} & set(allowed)
    kept = [message for kind, token, message in problems if (kind, token) not in allowed]
    kept += [f"{ALLOW_FILE}:{allowed[key]}: `{key[0]} | {key[1]}` is not needed any more — delete the line" for key in sorted(set(allowed) - used)]
    return kept


# ---- the check ---------------------------------------------------------------------------------------------------------------------------------------------
def docs_problems(root):
    """(kind, token, message) for everything the documents lack or cite wrongly."""
    text = corpus(root)
    problems = []
    props = schema_properties(root)
    problems += missing("schema", props, text, lambda name: "schemas/v1/" + ", ".join(sorted(props[name])))
    keys = host_keys(root)
    problems += missing("host", keys, text, lambda name: keys[name])
    commands, flags = cli_commands(root)
    problems += missing("cli", {f"bot {c}" if " " not in c else c for c in commands}, text, lambda _: "crates/bot-cli/src/cli.rs")
    problems += missing("cli", flags, text, lambda _: "crates/bot-cli/src/cli.rs")
    admin = read(root, "docs/admin.md")
    problems += missing("route", admin_routes(root), admin, lambda _: ADMIN_ROUTES_FILE)
    problems += missing("tool", reserved_tools(root), text, lambda _: "crates/bot-profile/src/tools.rs")
    members = workspace_crates(root)
    for name in ("docs/architecture.md", "AGENTS.md"):
        listed = table_crates(read(root, name))
        problems += [("crate", crate, f"{name}: crate table and Cargo.toml members differ at `{crate}`") for crate in sorted(listed ^ members)]
    for doc in doc_files(root):
        problems += path_problems(root, doc)
    return problems


def check(root=Path(".")):
    allowed, bad = parse_allow((root / ALLOW_FILE).read_text(encoding="utf-8") if (root / ALLOW_FILE).is_file() else "")
    problems = bad + filter_allowed(docs_problems(root), allowed)
    if problems:
        raise ValueError("documents and code disagree:\n" + "\n".join(problems))


if __name__ == "__main__":
    check()

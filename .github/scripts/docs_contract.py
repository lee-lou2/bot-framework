"""Documents must name what the code defines, and what they cite must exist.

Checked from the repository root (`python3 -I .github/scripts/docs_contract.py`, or `check_docs()` in check_contracts.py):
  schema   every property of schemas/v1/*.json appears in the documents
  host     every host-settings key the Rust code accepts is written as code (in backticks or a code block) in the documents, so a common word in prose
           (`state`, `name`) does not count. The keys are read from the files of HOST_READERS, each by the way it rejects the rest: `top.remove("key")`,
           `only(map, &["key"], path)`, `matches!(key.as_str(), "key")`, `match key.as_str() { "key" => … }` and the fields of `#[derive(Deserialize)]`
           structs. A reader in another file is not seen until it is added there (the selftest fails when a file that rejects unknown keys is not listed)
  cli      every `bot` subcommand and `--flag` defined in crates/bot-cli/src/cli.rs appears in the documents
  route    every admin API route (`/v1/admin/…`) registered in crates/bot-admin/src/lib.rs appears in docs/admin.md
  tool     every reserved built-in tool id appears in the documents
  crate    the crate tables of docs/architecture.md and AGENTS.md list exactly the workspace members
  path     every repository path in backticks (`file.rs`, `file.rs:12`, `file.rs:12-20`), every relative link and every `#anchor` in the documents exists;
           code blocks are not read
What it cannot see: whether a sentence is still true, and (for schema, cli and tool names) whether a common name is documented for the right thing,
because those names are looked up anywhere in the documents. A reviewer checks the sentences.

An exception goes into .github/docs-allow.txt as `kind | token | reason`; an exception nothing needs any more is reported, so the list cannot rot."""
import json
import re
import tomllib
from pathlib import Path

ALLOW_FILE = Path(".github/docs-allow.txt")
PATH_ROOTS = ("crates/", "docs/", "schemas/", "scripts/", "examples/", ".github/", "images/")
ADMIN_ROUTES_FILE = "crates/bot-admin/src/lib.rs"
BUILTIN_TOOLS_FILE = "crates/bot-core/src/builtin.rs"
FENCE = re.compile(r"^[ \t]*```.*?^[ \t]*```[ \t]*$", re.S | re.M)


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


def code_text(text):
    """The code in the documents: fenced blocks and inline code spans."""
    return "\n".join(FENCE.findall(text) + re.findall(r"`[^`\n]+`", FENCE.sub("", text)))


def missing(kind, names, text, where, how="mentioned"):
    """One problem per name the text does not mention (`how` says in what form); `where` says what defines it."""
    return [(kind, name, f"`{name}` ({where(name)}) is not {how} in the documents") for name in sorted(names) if not mentioned(name, text)]


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
    """The part of a Rust source before its test module (`#[cfg(test)]` followed by `mod`). A test-only `use` or `fn` ahead of it does not end the code."""
    return re.split(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\b", text, maxsplit=1)[0]


def code_chars(text, start=0):
    """(index, character) of the characters outside string literals."""
    i = start
    while i < len(text):
        if text[i] == '"':
            i += 1
            while i < len(text) and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
        else:
            yield i, text[i]
        i += 1


def closing(text, start):
    """Index of the bracket that closes the one at `start` (the end of the text when it never closes)."""
    depth = 0
    for i, char in code_chars(text, start):
        depth += char in "([{"
        depth -= char in ")]}"
        if depth == 0:
            return i
    return len(text)


def statement_end(text, start):
    """Index of the `;` that ends the statement the text continues at `start`."""
    depth = 0
    for i, char in code_chars(text, start):
        depth += char in "([{"
        depth -= char in ")]}"
        if char == ";" and depth == 0:
            return i
    return len(text)


def literals(text):
    return re.findall(r'"([^"\n]*)"', text)


def removed_keys(text):
    """Top-level keys: `top.remove("key")`."""
    return re.findall(r'top\.remove\("([^"]+)"\)', text)


def only_lists(text):
    """Keys handed to `only(map, &["a", "b"], path)`, and the list bound first by `let allowed … = …;`. Other string arrays are not keys."""
    spans = [text[call.end():closing(text, call.end() - 1)] for call in re.finditer(r"\bonly\(", text)]
    spans += [text[bound.end():statement_end(text, bound.end())] for bound in re.finditer(r"\blet allowed\b[^=;]*=", text)]
    return [key for span in spans for array in re.findall(r"&\[([^\]]*)\]", span) for key in literals(array)]


def matched_keys(text):
    """Keys tested with `matches!(key.as_str(), "a" | "b")`."""
    return [key for found in re.finditer(r'matches!\(\s*\w+\.as_str\(\),\s*((?:"[^"\n]*"\s*\|?\s*)+)\)', text) for key in literals(found.group(1))]


def match_arms(text):
    """Keys sorted by `match key.as_str() { "a" => …, other => error }`: the string arms."""
    return re.findall(r'^\s*"([^"\n]+)"\s*=>', text, re.M)


def serde_fields(text):
    """Fields of the `#[derive(Deserialize)]` structs (`deny_unknown_fields` rejects the rest); `#[serde(rename = "x")]` names a field `x`."""
    keys = []
    structs = r"#\[derive\([^)]*\bDeserialize\b[^)]*\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?struct\s+\w+\s*\{(.*?)\n\s*\}"
    for body in re.findall(structs, text, re.S):
        renamed = None
        for line in body.splitlines():
            attribute = re.match(r'#\[serde\([^\]]*\brename\s*=\s*"([^"]+)"', line.strip())
            field = re.match(r"(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:", line.strip())
            if attribute:
                renamed = attribute.group(1)
            elif field:
                keys.append(renamed or field.group(1))
                renamed = None
    return keys


# The readers of the host settings: how a file lists the keys it accepts, and the files. A new reader is added here (its file by its idiom).
HOST_READERS = (
    (removed_keys, ("crates/bot-cli/src/host.rs",)),
    (only_lists, ("crates/bot-cli/src/execution.rs", "crates/bot-cli/src/execution/provider.rs", "crates/bot-cli/src/embeddings.rs", "crates/bot-admin/src/settings.rs")),
    (matched_keys, ("crates/bot-cli/src/install.rs",)),
    (match_arms, ("crates/bot-cli/src/tool_limits.rs",)),
    (serde_fields, ("crates/bot-gateway/src/host.rs", "crates/bot-gateway/src/host/openai.rs")),
)


def host_keys(root):
    """key -> the first reader file that accepts it."""
    found = {}
    for extract, files in HOST_READERS:
        for relative in files:
            for key in extract(non_test_text(read(root, relative))):
                found.setdefault(key, relative)
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
    text = non_test_text(read(root, BUILTIN_TOOLS_FILE))
    model_ids = re.findall(r'pub const \w+:\s*ModelTool\s*=\s*ModelTool\s*\{[^}]*\bid:\s*"([^"]+)"', text, re.S)
    extra_ids = re.findall(r'pub const \w+_TOOL:\s*&str\s*=\s*"([^"]+)"', text)
    return sorted(set(model_ids + extra_ids))


def workspace_crates(root):
    members = tomllib.loads(read(root, "Cargo.toml"))["workspace"]["members"]
    return {Path(m).name for m in members}


def table_crates(text):
    """Crate names in the first column of a markdown table (`| bot-x |` or `` | `bot-x` | ``)."""
    return set(re.findall(r"^\|\s*`?(bot-[a-z0-9-]+)`?\s*\|", text, re.M))


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
    text = FENCE.sub("", doc.read_text(encoding="utf-8"))
    name = str(doc.relative_to(root))
    problems = []
    for token in sorted(set(re.findall(r"`([^`\n]+)`", text))):
        if not token.startswith(PATH_ROOTS) or re.search(r"[\s<>*…$|]", token):
            continue
        for candidate in expand_braces(token):
            path = re.sub(r"(?::\d+(?:-\d+)?)?(?:#.*)?$", "", candidate).rstrip("/")
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
    problems += missing("host", keys, code_text(text), lambda name: keys[name], "written as code")
    commands, flags = cli_commands(root)
    problems += missing("cli", {f"bot {c}" if " " not in c else c for c in commands}, text, lambda _: "crates/bot-cli/src/cli.rs")
    problems += missing("cli", flags, text, lambda _: "crates/bot-cli/src/cli.rs")
    admin = read(root, "docs/admin.md")
    problems += missing("route", admin_routes(root), admin, lambda _: ADMIN_ROUTES_FILE)
    problems += missing("tool", reserved_tools(root), text, lambda _: BUILTIN_TOOLS_FILE)
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

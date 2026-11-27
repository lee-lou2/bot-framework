#!/usr/bin/env python3
"""A checklist for adding a sibling of something that already exists: a second chat channel, a second model provider, a second secret store.

    python3 scripts/parity_audit.py slack discord --skip crates/bot-gateway/src/slack/

Lists the files that mention the reference name (`slack`) but not the new name (`discord`): the places the reference needed that the new one may need too.
It finds the places the compiler cannot (a schema, a SQL constraint, a document, a name matched by a string). It is not an oracle: a file can mention `slack`
for a feature only Slack has, so read the list as a checklist and cross lines out. Files that mention the new name but not the reference are the new code you
already wrote.

Matching is a case-insensitive substring (`Slack` also finds `SlackChannel` and `slack_events`). Tests, test doubles, `target/` and VCS folders are left out
(`--tests` keeps the tests). `--skip` drops paths that contain a text, for example the reference's own module. With `--strict` the exit status is 1 when a file is
missing, so a change can be held to an empty list. Usage and I/O errors exit 2."""

import argparse
import os
import sys
from pathlib import Path

SUFFIXES = {".rs", ".sql", ".md", ".json", ".yaml", ".yml", ".js", ".mjs", ".css", ".html"}
PRUNE = {"target", ".git", "node_modules", "__pycache__"}
TEST_DIRS = {"tests", "testing"}


def is_test_path(relative):
    parts = Path(relative).parts
    name = parts[-1]
    return bool(TEST_DIRS & set(parts[:-1])) or name in {"tests.rs", "testing.rs"} or name.endswith(("_tests.rs", ".test.mjs", ".test.js"))


def source_files(root, skip=(), tests=False):
    """Repository-relative paths (with `/`) of the files an audit looks at."""
    found = []
    for folder, names, files in os.walk(root):
        names[:] = sorted(name for name in names if name not in PRUNE)
        for name in sorted(files):
            relative = (Path(folder) / name).relative_to(root).as_posix()
            if Path(name).suffix not in SUFFIXES or any(text in relative for text in skip):
                continue
            if tests or not is_test_path(relative):
                found.append(relative)
    return found


def mentions(root, relative, name):
    try:
        return name.lower() in (Path(root) / relative).read_text(encoding="utf-8", errors="replace").lower()
    except OSError:
        return False


def audit(root, reference, new, skip=(), tests=False):
    """{'missing': files with the reference only, 'both': ..., 'only_new': files with the new name only}."""
    result = {"missing": [], "both": [], "only_new": []}
    for relative in source_files(root, skip, tests):
        has_reference, has_new = mentions(root, relative, reference), mentions(root, relative, new)
        if has_reference and has_new:
            result["both"].append(relative)
        elif has_reference:
            result["missing"].append(relative)
        elif has_new:
            result["only_new"].append(relative)
    return result


def group(paths):
    """Paths grouped by `crates/<crate>` (or the first folder), in order."""
    groups = {}
    for path in paths:
        parts = path.split("/")
        groups.setdefault("/".join(parts[:2]) if parts[0] == "crates" and len(parts) > 2 else parts[0], []).append(path)
    return groups


def report(result, reference, new):
    total = len(result["missing"]) + len(result["both"])
    lines = [f"{len(result['both'])} of {total} files mentioning `{reference}` also mention `{new}`."]
    lines.append(f"{len(result['missing'])} missing candidate(s) (mention `{reference}` only; ignore reference-only features):")
    for name, paths in group(result["missing"]).items():
        lines.append(f"  {name} ({len(paths)})")
        lines += [f"    {path}" for path in paths]
    if result["only_new"]:
        lines.append(f"{len(result['only_new'])} file(s) mentioning `{new}` only (new code):")
        lines += [f"    {path}" for path in result["only_new"]]
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description="List the files that mention a reference name but not the new name.")
    parser.add_argument("reference", help="a name that already exists, for example slack")
    parser.add_argument("new", help="the name being added, for example discord")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent, help="repository root (default: this repository)")
    parser.add_argument("--skip", action="append", default=[], help="leave out paths containing this text (repeatable)")
    parser.add_argument("--tests", action="store_true", help="include tests and test doubles")
    parser.add_argument("--strict", action="store_true", help="exit 1 when a file mentions only the reference")
    args = parser.parse_args(argv)
    if not args.reference.strip() or not args.new.strip() or args.reference.lower() == args.new.lower():
        parser.error("reference and new must be two different, non-empty names")
    if not args.root.is_dir():
        print(f"{args.root}: not a directory", file=sys.stderr)
        return 2
    result = audit(args.root, args.reference, args.new, args.skip, args.tests)
    print(report(result, args.reference, args.new))
    return 1 if args.strict and result["missing"] else 0


if __name__ == "__main__":
    sys.exit(main())

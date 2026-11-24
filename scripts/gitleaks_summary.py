#!/usr/bin/env python3
"""Print a gitleaks JSON report without anything that could carry a private name.

Shown per finding: rule id, repository-relative file, line and the short commit id. Authors, e-mail addresses,
commit messages, dates and the matched text are never printed (gitleaks itself prints all of them in verbose mode).
When BOT_PRIVATE_TERMS_FILE is set, a file name that holds a listed term is replaced by a short hash.

Usage: gitleaks_summary.py <report.json> [<path prefix to strip>]
Exit status: 0 printed, 2 usage or unreadable report.
"""

import hashlib
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
LIMIT = 100


def terms():
    listing = os.environ.get("BOT_PRIVATE_TERMS_FILE")
    if not listing:
        return None
    try:
        from private_terms_check import Terms

        return Terms(listing)
    except Exception:
        return None


def main(argv):
    if len(argv) not in (2, 3):
        print("사용법: gitleaks_summary.py <report.json> [<제거할 경로 접두사>]", file=sys.stderr)
        return 2
    try:
        found = json.loads(Path(argv[1]).read_text(encoding="utf-8")) or []
    except (OSError, ValueError):
        print("gitleaks 보고서를 읽지 못했다", file=sys.stderr)
        return 2
    strip = argv[2].rstrip("/") + "/" if len(argv) == 3 and argv[2] else ""
    private = terms()
    rows = set()
    for item in found:
        name = str(item.get("File", ""))
        name = name[len(strip):] if strip and name.startswith(strip) else name
        if private is not None and private.hits(name):
            name = "경로#" + hashlib.sha256(name.encode()).hexdigest()[:8]
        commit = str(item.get("Commit", ""))[:8]
        rows.add((str(item.get("RuleID", "?")), name, int(item.get("StartLine", 0) or 0), commit))
    for rule, name, line, commit in sorted(rows)[:LIMIT]:
        where = f"{name}:{line}" if line else name
        print(f"  규칙 {rule} · {where}" + (f" · 커밋 {commit}" if commit else ""))
    if len(rows) > LIMIT:
        print(f"  … 외 {len(rows) - LIMIT}건")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

"""Shrink-only channel-neutrality drift guard for bot-core and additive SQL migrations.

Checked from the repository root (`python3 -I .github/scripts/drift_guard.py`, or called from
`.github/scripts/check_contracts.py`):
  1. Freezes per-file production references to `slack` in `crates/bot-core/src/` (excluding test
     files and inline `#[cfg(test)] mod` sections): counts may shrink, never increase, and no new
     production file in `bot-core` may introduce `slack`.
  2. Rejects `slack` in any SQL migration numbered >= 0030 under `crates/bot-store/migrations/`.
"""
from pathlib import Path
import re
import sys

# Frozen shrink-only baseline of production lines mentioning `slack` (case-insensitive) in bot-core.
BOT_CORE_SLACK_BASELINE = {
    "api.rs": 1,
    "cards.rs": 6,
    "command.rs": 1,
    "grant.rs": 1,
    "identity.rs": 4,
    "intake.rs": 2,
    "ledger.rs": 1,
    "lib.rs": 2,
    "profile.rs": 9,
    "schedule.rs": 1,
    "slack.rs": 13,
    "state.rs": 3,
}

MIN_NEUTRAL_MIGRATION = 30
MIGRATION_NUM = re.compile(r"^(\d+)_.*\.sql$")


def non_test_text(text: str) -> str:
    return re.split(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\b", text, maxsplit=1)[0]


def is_test_rs(rel: Path) -> bool:
    name = rel.name
    return name == "tests.rs" or name.endswith("_tests.rs") or "tests" in rel.parts


def check_root(root: Path) -> list[str]:
    problems: list[str] = []
    core_src = root / "crates/bot-core/src"
    if core_src.is_dir():
        for path in sorted(core_src.rglob("*.rs")):
            rel = path.relative_to(core_src)
            if is_test_rs(rel):
                continue
            key = rel.as_posix()
            prod = non_test_text(path.read_text(encoding="utf-8"))
            count = sum(1 for line in prod.splitlines() if "slack" in line.lower())
            limit = BOT_CORE_SLACK_BASELINE.get(key, 0)
            if count > limit:
                problems.append(
                    f"crates/bot-core/src/{key}: production `slack` references ({count}) exceed shrink-only baseline ({limit})"
                )
    migrations = root / "crates/bot-store/migrations"
    if migrations.is_dir():
        for path in sorted(migrations.glob("*.sql")):
            m = MIGRATION_NUM.match(path.name)
            if not m or int(m.group(1)) < MIN_NEUTRAL_MIGRATION:
                continue
            text = path.read_text(encoding="utf-8")
            if "slack" in text.lower():
                problems.append(
                    f"crates/bot-store/migrations/{path.name}: additive migrations >= {MIN_NEUTRAL_MIGRATION:04d} must remain channel-neutral (found `slack`)"
                )
    return problems


def check() -> None:
    problems = check_root(Path("."))
    if problems:
        for problem in problems:
            print(f"drift-guard: {problem}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    check()

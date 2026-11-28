"""Self-tests for `.github/scripts/drift_guard.py`."""
import importlib.util
from pathlib import Path
import tempfile

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("drift_guard", ROOT / ".github/scripts/drift_guard.py")
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


def write(base: Path, rel: str, text: str) -> None:
    path = base / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def main() -> None:
    # Real repository must pass cleanly.
    real_problems = GUARD.check_root(ROOT)
    assert not real_problems, f"repository failed drift guard: {real_problems}"

    with tempfile.TemporaryDirectory() as tmp:
        base = Path(tmp)
        # Shrinking or staying at baseline passes; inline #[cfg(test)] mod slack is ignored.
        write(base, "crates/bot-core/src/api.rs", "pub fn slack_ok() {}\n#[cfg(test)]\nmod tests { const X: &str = \"slack\"; }\n")
        write(base, "crates/bot-store/migrations/0030_credentials.sql", "create table bot_credentials (id int);\n")
        assert GUARD.check_root(base) == []

        # New bot-core file introducing `slack` fails.
        write(base, "crates/bot-core/src/new_mod.rs", "pub const CH: &str = \"slack\";\n")
        found = GUARD.check_root(base)
        assert len(found) == 1 and "new_mod.rs" in found[0], found
        (base / "crates/bot-core/src/new_mod.rs").unlink()

        # Exceeding baseline in existing file fails.
        write(base, "crates/bot-core/src/api.rs", "// slack 1\n// slack 2\n")
        found = GUARD.check_root(base)
        assert len(found) == 1 and "api.rs" in found[0], found
        write(base, "crates/bot-core/src/api.rs", "// clean\n")

        # Migration >= 0030 mentioning `slack` fails; < 0030 is ignored.
        write(base, "crates/bot-store/migrations/0029_old.sql", "-- slack legacy\n")
        assert GUARD.check_root(base) == []
        write(base, "crates/bot-store/migrations/0031_bad.sql", "alter table x add column slack_id text;\n")
        found = GUARD.check_root(base)
        assert len(found) == 1 and "0031_bad.sql" in found[0], found

    print("drift_guard tests passed")


if __name__ == "__main__":
    main()

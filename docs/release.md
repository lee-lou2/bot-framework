# Release Gate

English | [한국어](ko/release.md)

Run `scripts/release-check.sh` before publishing (CI runs the same script). Pre-release verification adds commit-identity and private-terms checks across the exportable repository.

For v2.0, model VM deliverables are build definitions and scripts only — prebuilt images are not distributed ([Model VM image](model-image.md)). The `deny.toml` license allowlist covers Rust dependencies (`cargo deny`) and committed repository files only.

```sh
scripts/release-check.sh                    # Default checks (runs PG tests if BOT_TEST_DATABASE_URL is set)
scripts/release-check.sh --strict           # Pre-release gate: PostgreSQL required, skips fail
scripts/release-check.sh --only "fmt deny"  # Run selected steps only
scripts/release-check.sh --list             # Print step names
```

## Gate Steps

| Step | What it verifies | CI counterpart |
|---|---|---|
| `clean` | Working tree and index match `HEAD`. Dirty state is `SKIP` (fails under `--strict`). | Local only (excluded from CI `--only`) |
| `fmt` | `cargo fmt --all -- --check` | Same |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` (plus `--bin bot`) | Same |
| `test` | `cargo test --workspace --locked --no-fail-fast` (surfaces all failures in one run). Runs PG tests when `BOT_TEST_DATABASE_URL` is set (else self-skips). Under `--strict`, fails pre-flight if DB is unset/unreachable and sets `BOT_TEST_REQUIRE_DB=1` and `BOT_REQUIRE_NODE=1`. | Same (PostgreSQL 16 service + required DB) |
| `integration` | Mandatory end-to-end integration tests: real `claude` CLI, `srt` sandbox, Apple container model VM, synthetic-secret sweep (`secret_sweep`: 0 leaks in stderr/audit/store/collector), and official OpenAI SDK (`openai_compat`) with `BOT_CLAUDE_CANARY`, `BOT_SRT_CANARY`, `BOT_CONTAINER_CANARY`, `BOT_OPENAI_CANARY=required`, `BOT_TEST_REQUIRE_DB=1`, `BOT_REQUIRE_NODE=1`. **Zero-skip contract:** fails pre-flight if any required tool, `container` service, `BOT_MODEL_IMAGE`, DB, or SDK env is missing; every target must pass ≥1 test with 0 skip markers (`skipped:`/`skipping:`); `#[ignore]` count must equal `ignored=N` in `INTEGRATION_TESTS` (default `0`; currently only CI-only `split_roles` has `ignored=1`, verified against source by `selftest`). Runs in `--strict` or `--only integration` (visible `SKIP` in default runs). | Not in CI (no Apple container or real `claude`; CI's `Linux tool sandbox` job runs `srt_canary` in required mode) |
| `contracts` | `.github/scripts/check_contracts.py` (requires Python 3.11+ `tomllib`, `jsonschema`, `PyYAML`): ≤600 lines/file (except `Cargo.lock`), line-1 `//!` module headers, no editor/merge leftovers (`*.bak`, `*.orig`, `*.rej`, `*.swp`, `*~`), schema/example validation (`AGENT.md`/`SKILL.md` front matter schema, folder-name match, unclosed `---`; full strict YAML 1.2 parsing, duplicate-key/anchor bans, size caps, and cross-file refs run in Rust `bot-profile` tests under `test`), docs contract, CI workflow hardening, `rust-toolchain.toml` pin, and `crates/bot-cli/tests/openai_sdk` pins + CI job guards (details below). | Same |
| `deny` | `cargo deny --all-features --locked check licenses bans advisories sources` | Same |
| `secrets-tree` | Copies publishable files (tracked + untracked excluding `.gitignore`) to a temp dir for `gitleaks dir`, plus `gitleaks git --staged`. Fails on listing errors or 0 files. | Same |
| `secrets-history` | `gitleaks git --log-opts="--all --full-history -m"` on the **pinned full mirror** (all refs and merge parents). | Same |
| `identity` | All commits (author + committer) and annotated tags in the pinned mirror use `@users.noreply.github.com`. Fails on git errors. | Local only |
| `private-terms` | Scans worktree, index, and pinned mirror history (including `Cargo.lock` and hidden files) with `scripts/private_terms_check.py`. | Local only (list is private) |
| `selftest` | Runs `scripts/test_*.py` self-tests against synthetic lists and temp repos. | Same |
| *(CI only)* | Linux `tool-sandbox` (`srt_canary`) and `runs_personal_demo` (real tool, approval, `cron`, PostgreSQL via `split_roles`). | CI only |

**Key invariants:**
- **Zero silent skips:** Missing tools fail, and skips never count as passes. Environment-gated tests must print `skipped:` or `skipping:`. Register every new integration test in `INTEGRATION_TESTS` in `scripts/release-check.sh` — `scripts/test_integration_list.py` (in `selftest`) scans all `#[test]` files using `*_CANARY`, `have_srt`, or `skipped:` to catch omissions (DB skips are already converted to failures by `BOT_TEST_REQUIRE_DB=1`).
- **Contract checks on CI & toolchain:** `.github/workflows` must pin every external action to a 40-char commit SHA (or `docker://...@sha256:...`) + version comment, every `image:` to `@sha256:` + version comment, and `setup-node` to exact `x.y.z`; set top-level `permissions` strictly to `contents: read` (jobs may only narrow); set `timeout-minutes` ≤ 120 on every job and `persist-credentials: false` on `actions/checkout`; forbid `secrets.`, `GITHUB_TOKEN`, `pull_request_target`, `*-latest` runners, unpinned `pip install` (require exact versions or `--require-hashes`), `npm install`/`ci` without `--ignore-scripts` and exact versions, and piping downloads to a shell. Root `rust-toolchain.toml` must specify an exact `x.y.z` `channel` with `rustfmt` and `clippy`.
- **Commit pinning & publishable mirror:** At startup the gate records `HEAD` and `RELEASE_CHECK_REFS` (`Commit to check: <hash>`) and re-checks them at mirror creation and before `Checked commit: <hash>` (`--strict` fails on drift under `pin`; non-strict marks a visible `SKIP`). History steps (`secrets-history`, `identity`, `private-terms`) fetch `RELEASE_CHECK_REFS` + `HEAD` into a temporary bare full mirror (shallow clones fail) and scan every reachable commit, nested tag object, blob, and path. Point-in-time comparison assumes an exclusive checkout (`clean` verifies worktree state at start).
- **Redacted reporting:** `private_terms_check.py` prints only opaque locations (object hash, line number, `path#xxxxxxxx` if a path matches a term) and `[list line N]`. `gitleaks` prints only a JSON summary of rule, file, line, and short commit hash (no emails, messages, dates, or secret values). Temp/list/mirror paths appear as labels (`fmt`/`clippy`/`test` cargo output may include workspace paths).

## Official OpenAI SDK End-to-End (`openai_compat`)

Verifies the OpenAI-compatible HTTP entrance end-to-end with the official `openai` SDKs (Python `2.29.0`, Node `6.29.0`) over real PostgreSQL, real gateway, real `claude` CLI, and a synthetic sandbox (only the model upstream is faked; SDKs override only `base_url` and `api_key`; no real OpenAI API or keys).

- **Wire & schema checks:** A transparent proxy counts actual requests (including SDK retries) and validates every JSON and SSE response against the pinned official OpenAPI spec (`openai/openai-openapi`, commit `498c71ddf6f1c45b983f972ccabca795da211a3e`, pinned SHA-256, including 3.0 `nullable`/`$ref`). The PG call ledger and fake upstream count runs, model calls, and tool executions.
- **Pinned test environment:** `scripts/openai-sdk-setup.sh` installs hash-pinned dependencies under `target/` (never inside the repo; includes 2 test-only MPL-2.0 wheels `certifi` and `tqdm`), verifies SDK versions and schema commit/SHA-256, and reuses intact installs offline. Passing `BOT_OPENAI_SPEC` also checks that `schema/openai-subset.json` matches the pinned `openapi.yaml`.
- **Anti-bypass enforcement:** `crates/bot-cli/tests/openai_sdk/sdk_run.py` requires every test in `expected.json` to pass exactly once — failing on any skip, expected failure, unexpected success, todo, cancel, missing/extra test, zero passes, zero proxied requests, or unvalidated expected schema. Pre-run source checks reject `skip`, `todo`, `only`, `expectedFailure`, and `skipTest`; `contracts` verifies `pin.json`, Python `--hash=sha256`, npm `sha512`/registry/no-install-scripts, and that CI's `openai-sdk` job invokes `sdk_run.py manifest`, `run_stub.sh`, `run_node_stub.sh`, and `check_runners.sh` without `if`, `continue-on-error`, `shell`, pipes, `|| true`, or raw `unittest`/`node --test`/`pytest`. Broken-example fixtures (`check_runners.sh` and `scripts/test_openai_sdk_results.py`) lock these rules.
- **CI vs local `--strict`:** CI's `OpenAI SDK harness` job tests pins, negative runners, and a contract stub only — **green CI does not exercise the real gateway**. Running local `--strict` (`integration` / `openai_compat`) is required before release.

## Pre-Release Checklist

1. On a clean full clone, run `scripts/release-check.sh --strict` with the required environment variables below, record the passing commit, and push that exact commit (never re-read a branch name or `HEAD`).
2. Refresh the RustSec advisory DB by running `scripts/release-check.sh --only deny` **without** `RELEASE_CHECK_OFFLINE=1` (the only gate step needing network; CI always runs online). Upgrade affected dependencies or record dated `ignore` entries with rationale in `deny.toml`.
3. Keep `deny.toml` `allow` minimal: remove any license triggering `license-not-encountered` warnings (`multiple-versions = "warn"` is non-fatal).

## Environment Variables

| Variable | Purpose |
|---|---|
| `BOT_PRIVATE_TERMS_FILE` | Path to private terms list outside the repo (missing → `SKIP`; rejected if inside repo). |
| `BOT_TEST_DATABASE_URL` | Disposable PostgreSQL test DB (tests create/drop temp schemas). Required in `--strict` with `psql`; URL is never printed. |
| `RELEASE_CHECK_OFFLINE=1` | Disables network access for `cargo` and `cargo deny` (uses cached `~/.cargo/advisory-dbs`; run online `deny` before release). |
| `RELEASE_CHECK_EMAIL_DOMAIN` | Allowed email domain for `identity` (default `users.noreply.github.com`). |
| `RELEASE_CHECK_REFS` | Publishable ref patterns fetched into the pinned mirror (default `refs/heads/* refs/tags/*`). |
| `BOT_MODEL_IMAGE` | Local digest-pinned model VM ref (`name@sha256:...` from `images/model/build.sh`). Required in `integration` / `--strict`; never printed. |
| `BOT_CLAUDE_BIN` / `BOT_SRT_BIN` | Explicit paths to real `claude` CLI / `srt` (defaults to `PATH`). |
| `BOT_OPENAI_SDK_DIR` | Absolute path for official SDK test env (default `$CARGO_TARGET_DIR/openai-sdk`). |
| `BOT_OPENAI_SPEC` | Optional path to original `openapi.yaml` to verify committed schema subset extraction. |
| `RELEASE_CHECK_SDK_SETUP` | Self-test hook overriding the SDK setup script (not for normal use). |
| `CARGO_TARGET_DIR` | Absolute build directory to isolate concurrent runs. |

## Private Terms List

Keep private names (company, product, bot names) outside this repository and pass the file path via `BOT_PRIVATE_TERMS_FILE` (rejected if inside the repo). One entry per line; blank lines and `#` comments are ignored:

| Syntax | Match rule |
|---|---|
| `<text>` | Case-insensitive substring |
| `word:<text>` | Case-insensitive whole word (non-alphanumeric boundaries) |
| `re:<regex>` | Case-insensitive Python regex |
| `cs:<regex>` | Case-sensitive Python regex (e.g., identifier shapes) |

Scans every publishable working-tree file (`git add -A`, including `Cargo.lock` and hidden files — names, symlink targets, contents), staged-only diffs, and full pinned-mirror history (all ref names, nested tags, commit authors/committers/messages, and every historical blob and path including deleted files and merge conflict resolutions). Output omits matched terms so reports are safe to share.

## Verified Tool Versions and CI Pinning

Verified 2026-11-24 on macOS (Homebrew: `brew install gitleaks cargo-deny actionlint`).

| Tool | Version | Notes |
|---|---|---|
| `gitleaks` | `8.30.1` | CI pulls the digest-pinned [official image](https://github.com/gitleaks/gitleaks/pkgs/container/gitleaks) `v8.30.1` and checks binary version. |
| `cargo-deny` | `0.21.0` | CI installs `0.21.0` and checks binary version. Online runs refresh RustSec DB in `~/.cargo/advisory-dbs`. |
| Python modules | `jsonschema 4.25.1`, `PyYAML 6.0.2` | Pinned in CI; other versions may run without identical guarantees. |
| Official `openai` SDK (test only) | Python `2.29.0`, Node `6.29.0` | Hash-pinned outside repo; includes 2 MPL-2.0 wheels (`certifi`, `tqdm`) outside the distribution/Rust allowlist. Python lock allows CPython 3.12 Linux x86_64 and macOS arm64 wheels only. |

**CI tool pinning** (where to bump):

| Target | Pin method | How to bump |
|---|---|---|
| Rust | Root `rust-toolchain.toml` `channel` (`1.98.1`, with `rustfmt` + `clippy`); CI runs `rustup toolchain install --no-self-update`. Enforced by `contracts`. | Update `channel` and verify `scripts/release-check.sh --strict`. |
| Node (`tool-sandbox`, `openai-sdk`) | Exact `setup-node` version (`24.22.3`). Enforced by `contracts`. | Update version string. |
| Service DB image | `postgres:16@sha256:...` with `# 16.16` comment. Enforced by `contracts`. | Update digest and version comment together. |
| Linux sandbox tools | `srt` `0.0.83` exact; Ubuntu 22.04 apt **upstream globs** `bubblewrap=0.6.1-*`, `socat=1.7.4.1-*`, `ripgrep=13.0.0-*` (versions logged on install). Human-maintained (along with `srt`, `cargo-deny`, `gitleaks`). | Update glob and run `srt_canary`. |

Apt packages pin upstream versions without distro revision suffixes (e.g., `-1ubuntu0.3`) so security updates superseding old `.deb` revisions do not break CI without code changes. Global `srt` npm installation has no lockfile; `--ignore-scripts` blocks lifecycle scripts.

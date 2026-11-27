#!/usr/bin/env bash
# Public release gate. Run from any directory inside the repository before publishing.
#
#   scripts/release-check.sh                  Default checks (runs without a test DB)
#   scripts/release-check.sh --list           List step names
#   scripts/release-check.sh --only "fmt deny"  Run selected steps only
#   scripts/release-check.sh --strict         Pre-release mode: skips fail and PostgreSQL tests are required
#
# Environment variables:
#   BOT_PRIVATE_TERMS_FILE      Private terms list outside the repository (missing -> skip).
#   BOT_TEST_DATABASE_URL       Enables PostgreSQL tests when set; required in --strict (sets BOT_TEST_REQUIRE_DB=1).
#   RELEASE_CHECK_OFFLINE=1     Run offline (cargo and cargo-deny make no network requests).
#   RELEASE_CHECK_EMAIL_DOMAIN  Allowed author/tagger email domain (default: users.noreply.github.com).
#   RELEASE_CHECK_REFS          Publishable refs (default: "refs/heads/* refs/tags/*"). History steps pin these refs and HEAD in a full mirror.
#   CARGO_TARGET_DIR            Build target directory (use an absolute path to avoid collisions).
#   BOT_MODEL_IMAGE             Model VM image reference (`name@sha256:...`, from images/model/build.sh) for `integration` / `--strict`.
#   BOT_CLAUDE_BIN, BOT_SRT_BIN Paths to real claude CLI / srt (default: PATH lookup).
#
# Records HEAD and publishable refs at start, then compares at mirror creation and end (mismatch -> fail in --strict, skip otherwise).
# Only those checkpoints are compared; changes reverted before the end or worktree edits are not detected (assumes an exclusive checkout).
# Output never prints absolute repo/list paths, private terms, or connection URLs.
# Exit codes: 0 pass, 1 fail (or skip under --strict), 2 usage error.
# Step names match CI (.github/workflows/ci.yml). See docs/release.md.

set -u
set -o pipefail

STEPS="clean fmt clippy test integration contracts deny secrets-tree secrets-history identity private-terms selftest"
here=$(cd "$(dirname "$0")" && pwd)
strict=0
only=""
while [ $# -gt 0 ]; do
  case "$1" in
    --list) for s in $STEPS; do echo "$s"; done; exit 0 ;;
    --strict) strict=1 ;;
    --only)
      [ $# -ge 2 ] || { echo "--only requires a step list" >&2; exit 2; }
      shift
      only="$1"
      ;;
    -h | --help) sed -n '2,/^$/p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done
for name in $only; do
  case " $STEPS " in
    *" $name "*) ;;
    *) echo "unknown step: $name (see --list)" >&2; exit 2 ;;
  esac
done

root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "must run inside a git repository" >&2; exit 2; }
cd "$root" || exit 2

refs_state() {
  set -f
  # shellcheck disable=SC2086
  git for-each-ref --format='%(refname) %(objectname)' ${RELEASE_CHECK_REFS:-refs/heads/* refs/tags/*} 2>/dev/null
  set +f
}
head_now() { git rev-parse --verify -q HEAD 2>/dev/null || true; }

# Pin start commit/refs; compared at mirror creation and end (--strict fails on drift).
pinned_head=$(head_now)
pinned_refs=$(refs_state)

deny_flags=""
if [ "${RELEASE_CHECK_OFFLINE:-0}" = 1 ]; then
  export CARGO_NET_OFFLINE=true
  deny_flags="--offline"
fi

work=$(mktemp -d) || exit 2
trap 'rm -rf "$work"' EXIT
log="$work/log"
: >"$log"
failed=0
skipped=0
skip_flag=0
note=""

selected() {
  [ -z "$only" ] && return 0
  case " $only " in *" $1 "*) return 0 ;; esac
  return 1
}

x() { printf '+ %s\n' "$*"; "$@"; }
# Print a label instead of commands containing temp, list, or mirror paths (may contain private names).
xs() { local label=$1; shift; printf '+ %s\n' "$label"; "$@"; }

need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  echo "missing required tool: $1 — install: $2"
  note="missing tool: $1"
  return 1
}

run_step() {
  local name=$1 title=$2 fn=$3 start code status
  selected "$name" || return 0
  printf '\n== %s — %s\n' "$name" "$title"
  note=""
  skip_flag=0
  start=$SECONDS
  "$fn"
  code=$?
  if [ "$skip_flag" = 1 ]; then
    status="SKIP"
    skipped=$((skipped + 1))
  elif [ "$code" = 0 ]; then
    status="PASS"
  else
    status="FAIL"
    failed=$((failed + 1))
  fi
  printf '%s|%s|%ss|%s\n' "$status" "$name" "$((SECONDS - start))" "$note" >>"$log"
}

# Copy only publishable files (tracked + untracked, excluding .gitignore) while preserving symlinks.
# Fails on listing/copy errors or zero copied files (never passes an empty tree as clean).
snapshot() {
  local dest=$1 file copied=0
  git ls-files -z --cached --others --exclude-standard >"$work/files.list" 2>/dev/null || {
    echo "failed to list publishable files (git ls-files failed)"
    return 1
  }
  while IFS= read -r -d '' file; do
    [ -e "$file" ] || [ -L "$file" ] || continue # Deleted worktree files are not published.
    [ -d "$file" ] && [ ! -L "$file" ] && continue # Submodule entry.
    mkdir -p "$dest/$(dirname "$file")" && cp -P "$file" "$dest/$file" || {
      echo "failed to copy publishable file"
      return 1
    }
    copied=$((copied + 1))
  done <"$work/files.list"
  [ "$copied" -gt 0 ] || {
    echo "no publishable files found"
    return 1
  }
}

# Fetch publishable refs (RELEASE_CHECK_REFS) and HEAD into a full bare mirror so all history steps
# inspect identical objects. HEAD is pinned too because detached checkouts (e.g., CI merge commits)
# may have no branches. Fails on shallow clones, fetch errors, or zero pinned refs.
mirror=""
mirror_state=""
prepare_mirror() {
  [ "$mirror_state" = ok ] && return 0
  if [ "$mirror_state" = failed ]; then
    echo "cannot scan history without pinned mirror (see earlier error)"
    return 1
  fi
  mirror_state=failed
  if [ "$strict" = 1 ] && [ "$(head_now)" != "$pinned_head" ]; then
    echo "current HEAD differs from pinned start commit: HEAD moved during the run. Re-run on a fixed commit."
    note="HEAD moved — differs from pinned commit"
    return 1
  fi
  local shallow pattern count repo="$work/pinned.git" specs=""
  shallow=$(git rev-parse --is-shallow-repository 2>/dev/null)
  if [ "$shallow" != false ]; then
    echo "shallow clone or unknown depth: truncated history cannot be trusted. Run on a full clone."
    note="shallow clone — full clone required"
    return 1
  fi
  git init --quiet --bare "$repo" 2>/dev/null || {
    echo "failed to initialize pinned mirror"
    return 1
  }
  set -f
  for pattern in ${RELEASE_CHECK_REFS:-refs/heads/* refs/tags/*}; do specs="$specs +$pattern:$pattern"; done
  set +f
  git rev-parse --verify -q HEAD >/dev/null 2>&1 && specs="$specs +HEAD:refs/pinned/head"
  # shellcheck disable=SC2086
  git --git-dir="$repo" fetch --quiet --no-tags --no-auto-gc "$root" $specs 2>/dev/null || {
    echo "failed to fetch publishable refs into mirror"
    note="mirror fetch failed"
    return 1
  }
  count=$(git --git-dir="$repo" for-each-ref 2>/dev/null | wc -l | tr -d ' ')
  if [ "${count:-0}" -lt 1 ]; then
    echo "no refs were pinned: RELEASE_CHECK_REFS matched nothing and HEAD is missing"
    note="no pinned refs"
    return 1
  fi
  mirror=$repo
  mirror_state=ok
  echo "pinned publishable refs and HEAD (refs: ${count})"
}

step_clean() {
  local out count
  out=$(git status --porcelain=v1 --untracked-files=all 2>/dev/null) || {
    echo "failed to read working tree status (git status failed)"
    return 1
  }
  if [ -n "$out" ]; then
    count=$(printf '%s\n' "$out" | wc -l | tr -d ' ')
    echo "found ${count} uncommitted change(s); only committed state is published, so checked state differs."
    note="${count} uncommitted change(s) — differs from published state"
    skip_flag=1
    return 0
  fi
  echo "working tree and index match HEAD"
}

step_fmt() { x cargo fmt --all -- --check; }

step_clippy() {
  x cargo clippy --workspace --all-targets --locked -- -D warnings &&
    x cargo clippy -p bot-cli --bin bot --locked -- -D warnings
}

# Pre-release runs require PostgreSQL connectivity (URL is never printed) and export BOT_TEST_REQUIRE_DB=1.
require_database() {
  local url=${BOT_TEST_DATABASE_URL:-}
  if [ -z "$url" ]; then
    echo "PostgreSQL tests are required (--strict / integration): BOT_TEST_DATABASE_URL is not set."
    note="DB required — BOT_TEST_DATABASE_URL not set"
    return 1
  fi
  need psql "PostgreSQL client (used by CLI test helpers)" || return 1
  if ! PGCONNECT_TIMEOUT=5 psql -X -q -At -d "$url" -c 'select 1' >/dev/null 2>&1; then
    echo "failed to connect to test database (URL omitted)."
    note="DB connection failed"
    return 1
  fi
  export BOT_TEST_REQUIRE_DB=1
}

step_test() {
  local url=${BOT_TEST_DATABASE_URL:-}
  if [ "$strict" = 1 ]; then
    require_database || return 1
    # Require node for UI contract tests as well (matches CI).
    export BOT_REQUIRE_NODE=1
    echo "PostgreSQL tests required (BOT_TEST_REQUIRE_DB=1): missing DB or schema failure will fail instead of skipping."
    note="PostgreSQL tests required"
  elif [ -n "$url" ]; then
    echo "BOT_TEST_DATABASE_URL is set; running PostgreSQL tests."
    note="PostgreSQL tests included"
  else
    echo "BOT_TEST_DATABASE_URL is not set; PostgreSQL tests will self-skip."
    note="PostgreSQL tests skipped (BOT_TEST_DATABASE_URL not set)"
  fi
  # Run all targets without stopping on the first failure so every failure surfaces in one run.
  x cargo test --workspace --locked --no-fail-fast
}

# Required local integration tests (real claude CLI, srt sandbox, Apple container VM, secret sweep, official OpenAI SDK).
# Runs in required mode (*_CANARY=required) and fails on zero tests run, any skip marker, or #[ignore] count mismatch.
# Format: `<package> <test_target> [features] [ignored=N]`. Cross-checked against source by scripts/test_integration_list.py.
INTEGRATION_TESTS='
bot-broker claude_canary testing
bot-broker srt_canary testing
bot-runtime cli_started
bot-runtime cli_resume
bot-runtime cli_refusal
bot-runtime cli_credentials
bot-runtime cli_chat
bot-runtime cli_usage
bot-runtime cli_instructions
bot-runtime cli_memory
bot-runtime container_canary
bot-runtime container_anchor
bot-runtime container_claude
bot-runtime container_firewall
bot-runtime container_credentials
bot-runtime container_cancel
bot-cli claude_route
bot-cli claude_route_gone
bot-cli claude_card_dm
bot-cli claude_wiring
bot-cli claude_stale
bot-cli claude_attempts
bot-cli claude_schedule
bot-cli claude_shutdown
bot-cli serve_restore
bot-cli serve_restore_bundles
bot-cli serve_restore_set
bot-cli vm_serve
bot-cli private_serve
bot-cli tool_secrets
bot-cli split_roles ignored=1
bot-cli team_template
bot-cli secret_sweep
bot-cli openai_compat
bot-cli mcp_http
bot-cli mcp_http_real
bot-cli mcp_http_review
'

step_integration() {
  if [ "$strict" != 1 ] && [ -z "$only" ]; then
    echo "SKIP: required integration tests (real claude, srt, model VM) run only in --strict. Use --only integration to run now."
    note="runs only in --strict — skipped"
    skip_flag=1
    return 0
  fi
  require_database || return 1
  need cargo "rustup" || return 1
  if [ -z "${BOT_CLAUDE_BIN:-}" ]; then need claude "Claude Code CLI (or BOT_CLAUDE_BIN)" || return 1; fi
  if [ -z "${BOT_SRT_BIN:-}" ]; then need srt "npm install --global @anthropic-ai/sandbox-runtime (or BOT_SRT_BIN)" || return 1; fi
  need python3 "python3" || return 1
  need node "node" || return 1
  need container "brew install container" || return 1
  if ! container system status >/dev/null 2>&1; then
    echo "container service is not running: container system start"
    note="container service stopped"
    return 1
  fi
  case "${BOT_MODEL_IMAGE:-}" in
    *@sha256:*) ;;
    *)
      echo "BOT_MODEL_IMAGE=<name@sha256:...> is required (printed by images/model/build.sh)."
      note="BOT_MODEL_IMAGE not set — cannot run model VM tests"
      return 1
      ;;
  esac
  if ! container image inspect "$BOT_MODEL_IMAGE" >/dev/null 2>&1; then
    echo "image referenced by BOT_MODEL_IMAGE is not present locally (images are not pulled)."
    note="model image not present locally"
    return 1
  fi
  # Prepare official OpenAI SDK environment under target dir (never inside the repository); reused offline when intact.
  sdk_dir=${BOT_OPENAI_SDK_DIR:-${CARGO_TARGET_DIR:-$PWD/target}/openai-sdk}
  case "$sdk_dir" in
    /*) ;;
    *)
      echo "BOT_OPENAI_SDK_DIR must be an absolute path: $sdk_dir"
      note="BOT_OPENAI_SDK_DIR is not an absolute path"
      return 1
      ;;
  esac
  if ! x "${RELEASE_CHECK_SDK_SETUP:-$here/openai-sdk-setup.sh}" "$sdk_dir"; then
    note="failed to prepare official OpenAI SDK test environment"
    return 1
  fi
  export BOT_OPENAI_SDK_DIR=$sdk_dir
  export BOT_CLAUDE_CANARY=required BOT_SRT_CANARY=required BOT_CONTAINER_CANARY=required BOT_REQUIRE_NODE=1 BOT_OPENAI_CANARY=required
  local pkg target features out code passed skips files=0 total=0 bad=0
  local rest word allowed ignored
  while read -r pkg target rest; do
    [ -n "$pkg" ] || continue
    features='' allowed=0
    for word in $rest; do
      case "$word" in
        ignored=[0-9]*) allowed=${word#ignored=} ;;
        *) features="${features:+$features }$word" ;;
      esac
    done
    out="$work/integration-$pkg-$target.log"
    printf '+ cargo test --locked -p %s%s --test %s -- --nocapture --test-threads=1\n' "$pkg" "${features:+ --features $features}" "$target"
    # shellcheck disable=SC2086
    cargo test --locked -p "$pkg" ${features:+--features $features} --test "$target" -- --nocapture --test-threads=1 >"$out" 2>&1
    code=$?
    passed=$(LC_ALL=C awk '/^test result: ok\./ { for (i = 1; i <= NF; i++) if ($i == "passed;") n += $(i - 1) } END { print n + 0 }' "$out")
    ignored=$(LC_ALL=C awk '/^test result: ok\./ { for (i = 1; i <= NF; i++) if ($i == "ignored;") n += $(i - 1) } END { print n + 0 }' "$out")
    skips=$(LC_ALL=C grep -a -c -E 'skipped:|skipping:' "$out")
    files=$((files + 1))
    total=$((total + passed))
    if [ "$code" != 0 ]; then
      echo "  FAIL  $target (exit code $code) — tail:"
      tail -n 25 "$out" | sed 's/^/        /'
      bad=$((bad + 1))
    elif [ "$passed" = 0 ]; then
      echo "  FAIL  $target: no tests ran"
      bad=$((bad + 1))
    elif [ "$ignored" != "$allowed" ]; then
      echo "  FAIL  $target: ${ignored} #[ignore] test(s) (list allows ${allowed}) — ignored tests do not pass"
      bad=$((bad + 1))
    elif [ "${skips:-0}" != 0 ]; then
      echo "  FAIL  $target: ${skips} skip marker(s) — test skipped due to missing environment"
      LC_ALL=C grep -a -E 'skipped:|skipping:' "$out" | head -n 3 | sed 's/^/        /'
      bad=$((bad + 1))
    else
      echo "  PASS  $target: ${passed} passed"
    fi
  done <<EOF
$INTEGRATION_TESTS
EOF
  echo "required integration: ${files} files, ${total} passed, 0 skip markers required (${bad} failed)"
  note="integration: ${files} files, ${total} passed"
  [ "$bad" = 0 ]
}

step_contracts() {
  if ! python3 -I -c 'import jsonschema, yaml, tomllib' 2>/dev/null; then
    echo "Python 3.11+ with jsonschema and PyYAML is required — install: python3 -m pip install jsonschema==4.25.1 PyYAML==6.0.2"
    note="missing tools: Python 3.11+, jsonschema, PyYAML"
    return 1
  fi
  x python3 -I .github/scripts/check_contracts.py
}

step_deny() {
  need cargo-deny "brew install cargo-deny" || return 1
  # shellcheck disable=SC2086
  x cargo deny --all-features --locked $deny_flags check --hide-inclusion-graph licenses bans advisories sources
}

# Run gitleaks into a JSON report and print only the redacted summary (verbose mode leaks authors, emails, and messages).
# Exit codes: 0 clean, 2 findings (--exit-code 2), anything else is a tool failure.
leaks() {
  local label=$1 report=$2 strip=$3 code
  shift 3
  printf '+ %s\n' "$label"
  "$@" --redact --no-banner --exit-code 2 --report-format json --report-path "$report" >"$work/gitleaks.out" 2>&1
  code=$?
  case $code in
    0) echo "no secrets found" ;;
    2)
      echo "secrets found (values redacted):"
      python3 -I "$here/gitleaks_summary.py" "$report" "$strip" || echo "  (failed to summarize report)"
      return 1
      ;;
    *)
      echo "gitleaks execution failed (exit code $code)"
      return 1
      ;;
  esac
}

step_secrets_tree() {
  need gitleaks "brew install gitleaks" || return 1
  local tmp code=0
  tmp=$(mktemp -d "$work/tree.XXXXXX") || return 1
  snapshot "$tmp" || return 1
  leaks "gitleaks dir <publishable tree snapshot>" "$work/tree.json" "$tmp" gitleaks dir "$tmp" || code=1
  leaks "gitleaks git --staged" "$work/staged.json" "" gitleaks git . --staged || code=1
  return $code
}

# `-m` is required so secrets introduced in merge conflict resolutions are scanned.
step_secrets_history() {
  need gitleaks "brew install gitleaks" || return 1
  prepare_mirror || return 1
  leaks "gitleaks git <pinned mirror> --log-opts=\"--all --full-history -m\"" "$work/history.json" "" \
    gitleaks git "$mirror" --log-opts="--all --full-history -m"
}

# Verify commit and annotated tag emails use the public domain (prints object hashes only, never emails or names).
step_identity() {
  local domain=${RELEASE_CHECK_EMAIL_DOMAIN:-users.noreply.github.com} commits tags bad
  prepare_mirror || return 1
  commits=$(git --git-dir="$mirror" log --all --format='%h%x09%ae%x09%ce') || {
    echo "failed to read commits (git log failed)"
    return 1
  }
  tags=$(git --git-dir="$mirror" for-each-ref refs/tags --format='%(objectname:short)%09%(taggeremail:trim)') || {
    echo "failed to read tags (git for-each-ref failed)"
    return 1
  }
  bad=$(
    printf '%s\n' "$commits" | awk -F'\t' -v d="@$domain" '
      function ok(s) { return length(s) > length(d) && substr(s, length(s) - length(d) + 1) == d }
      NF && !(ok($2) && ok($3)) { print "commit " $1 }'
    printf '%s\n' "$tags" | awk -F'\t' -v d="@$domain" '
      function ok(s) { return length(s) > length(d) && substr(s, length(s) - length(d) + 1) == d }
      NF && $2 != "" && !ok($2) { print "tag " $1 }'
  )
  if [ -n "$bad" ]; then
    echo "authors without public address (@$domain):"
    echo "$bad" | sed 's/^/  /'
    return 1
  fi
  echo "all commits and tags use @$domain"
}

step_private_terms() {
  if [ -z "${BOT_PRIVATE_TERMS_FILE:-}" ]; then
    echo "SKIP: BOT_PRIVATE_TERMS_FILE is not set; private terms check skipped."
    echo "      Provide the list path and re-run with --strict before publishing."
    note="BOT_PRIVATE_TERMS_FILE not set — skipped"
    skip_flag=1
    return 0
  fi
  if [ ! -f "$BOT_PRIVATE_TERMS_FILE" ]; then
    echo "private terms list file not found"
    note="private terms list file not found"
    return 1
  fi
  prepare_mirror || return 1
  xs "private_terms_check.py <terms list> --git-dir <pinned mirror>" \
    python3 -I "$here/private_terms_check.py" "$BOT_PRIVATE_TERMS_FILE" --git-dir "$mirror"
}

step_selftest() {
  # Guard against recursive selftest when a test invokes release-check.sh.
  if [ -n "${RELEASE_CHECK_IN_SELFTEST:-}" ]; then
    echo "SKIP: running inside release-check selftest (avoiding recursion)."
    note="inside selftest — skipped"
    skip_flag=1
    return 0
  fi
  local code=0 file
  export RELEASE_CHECK_IN_SELFTEST=1
  for file in test_private_terms_check.py test_check_contracts.py test_check_workspace_contracts.py test_check_docs.py test_parity_audit.py test_integration_list.py test_release_check.py test_release_check_integration.py test_openai_sdk_results.py; do
    xs "python3 scripts/$file" python3 -I "$here/$file" || code=1
  done
  return $code
}

echo "Public release gate"
printf 'Tools: cargo %s · rustc %s · cargo-deny %s · gitleaks %s · %s\n' \
  "$(cargo --version 2>/dev/null | cut -d' ' -f2)" "$(rustc --version 2>/dev/null | cut -d' ' -f2)" \
  "$(cargo-deny --version 2>/dev/null | cut -d' ' -f2)" "$(gitleaks version 2>/dev/null | head -1)" \
  "$(python3 --version 2>/dev/null)"
echo "Commit to check: ${pinned_head:-(no commit)}"

run_step clean "working tree matches HEAD (published = checked)" step_clean
run_step fmt "formatting (cargo fmt --check)" step_fmt
run_step clippy "lints (clippy -D warnings, all targets)" step_clippy
run_step test "workspace tests (cargo test --workspace)" step_test
run_step integration "required integration tests (real claude, srt, model VM, secret sweep, 0 skips)" step_integration
run_step contracts "600-line limit, schemas, examples, CI hardening, toolchain pin" step_contracts
run_step deny "dependencies (cargo deny: licenses bans advisories sources)" step_deny
run_step secrets-tree "secrets — publishable working tree (gitleaks dir)" step_secrets_tree
run_step secrets-history "secrets — pinned full mirror history (gitleaks git --all --full-history -m)" step_secrets_history
run_step identity "commit and tag author emails (pinned mirror)" step_identity
run_step private-terms "private terms (worktree, index, pinned mirror history, Cargo.lock, hidden files)" step_private_terms
run_step selftest "private-terms scanner and release-gate self-tests" step_selftest

# Compare end HEAD and publishable refs against start pin (--strict fails, non-strict marks a visible skip).
if [ "$(head_now)" != "$pinned_head" ] || [ "$(refs_state)" != "$pinned_refs" ]; then
  if [ "$strict" = 1 ]; then
    printf 'FAIL|pin|0s|HEAD or publishable refs changed during the run — results may not match the pinned commit\n' >>"$log"
    failed=$((failed + 1))
  else
    printf 'SKIP|pin|0s|HEAD or publishable refs changed during the run — results may not match the start commit\n' >>"$log"
    skipped=$((skipped + 1))
  fi
fi

printf '\n== Summary\n'
passed=0
while IFS='|' read -r status name secs detail; do
  [ "$status" = "PASS" ] && passed=$((passed + 1))
  printf '  %-4s %-16s %6s  %s\n' "$status" "$name" "$secs" "$detail"
done <"$log"
printf 'PASS %s · FAIL %s · SKIP %s\n' "$passed" "$failed" "$skipped"
echo "Checked commit: ${pinned_head:-(no commit)}"
if [ "$skipped" -gt 0 ]; then
  echo "Warning: some checks were skipped. Skips are not passes (--strict fails on skips)."
fi
[ "$failed" -gt 0 ] && exit 1
[ "$strict" = 1 ] && [ "$skipped" -gt 0 ] && exit 1
exit 0

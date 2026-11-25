#!/usr/bin/env bash
# 공개 배포 관문. 공개 저장소를 내보내기 전에 어느 폴더에서든 돌린다.
#
#   scripts/release-check.sh                  기본 검사(시험 DB 없이 도는 것만)
#   scripts/release-check.sh --list           단계 이름만 보인다
#   scripts/release-check.sh --only "fmt deny"  지정한 단계만 돈다
#   scripts/release-check.sh --strict         공개 직전: 건너뜀도 실패이고 PostgreSQL 시험이 필수다
#
# 환경 변수
#   BOT_PRIVATE_TERMS_FILE      비공개 금칙어 목록(저장소 밖). 없으면 그 검사는 «건너뜀»이다.
#   BOT_TEST_DATABASE_URL       있으면 PostgreSQL 시험까지 돈다. 없으면 그 시험은 스스로 건너뛴다.
#                               --strict에서는 필수이고 연결할 수 없으면 실패한다(BOT_TEST_REQUIRE_DB=1을 건다).
#   RELEASE_CHECK_OFFLINE=1     네트워크 없이 돈다(cargo와 cargo-deny가 아무것도 내려받지 않는다).
#   RELEASE_CHECK_EMAIL_DOMAIN  커밋·태그 작성자 메일 도메인(기본 users.noreply.github.com).
#   RELEASE_CHECK_REFS          공개할 참조(기본 "refs/heads/* refs/tags/*"). 이력 검사는 이 참조와 HEAD만 전체 복제해 고정한다.
#   CARGO_TARGET_DIR            빌드 폴더. 다른 작업과 겹치지 않게 절대 경로로 준다.
#   BOT_MODEL_IMAGE             모델 VM 이미지 참조(`이름@sha256:…`, images/model/build.sh가 알려 준다). 필수 통합 시험(`integration`, --strict)이 쓴다.
#   BOT_CLAUDE_BIN·BOT_SRT_BIN  실제 claude CLI·srt의 경로(없으면 PATH에서 찾는다).
#
# 시작할 때의 HEAD·공개할 참조 값을 기록하고, 이력 복제 시점과 끝에서 같은 값인지 견주어 다르면(--strict는 실패, 그 밖은 건너뜀) 알린다.
# 견주는 것은 그 시점들의 값뿐이다: 검사 도중 바뀌었다가 끝나기 전에 원복되는 변화나 작업 트리 내용의 변화는 알 수 없다(단독 체크아웃을 전제로 한다).
# 출력에는 저장소·목록의 절대 경로, 금칙어, 접속 주소를 싣지 않는다.
# 종료 코드: 0 통과 · 1 실패(또는 --strict에서 건너뜀) · 2 사용법 오류.
# 단계 이름은 CI(.github/workflows/ci.yml)의 같은 검사와 짝을 이룬다. 자세한 내용은 docs/release.md.

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
      [ $# -ge 2 ] || { echo "--only 뒤에 단계 이름이 필요하다" >&2; exit 2; }
      shift
      only="$1"
      ;;
    -h | --help) sed -n '2,/^$/p' "$0"; exit 0 ;;
    *) echo "알 수 없는 인자: $1 (--help 참고)" >&2; exit 2 ;;
  esac
  shift
done
for name in $only; do
  case " $STEPS " in
    *" $name "*) ;;
    *) echo "알 수 없는 단계: $name (--list 참고)" >&2; exit 2 ;;
  esac
done

root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "git 저장소 안에서 실행해야 한다" >&2; exit 2; }
cd "$root" || exit 2

# 공개할 참조와 HEAD의 지금 값(참조 이름과 객체 해시). 시작할 때 기록하고 끝에서 다시 읽어 견준다(그 사이의 변화 과정은 보지 못한다).
refs_state() {
  set -f
  # shellcheck disable=SC2086
  git for-each-ref --format='%(refname) %(objectname)' ${RELEASE_CHECK_REFS:-refs/heads/* refs/tags/*} 2>/dev/null
  set +f
}
head_now() { git rev-parse --verify -q HEAD 2>/dev/null || true; }

# 검사할 커밋을 시작할 때 기록한다. --strict는 이력 복제 시점과 끝에서 HEAD·공개할 참조가 시작할 때와 같은지 보고 다르면 실패한다.
# 중간에 바뀌었다가 그 시점들 전에 원복된 변화는 알 수 없다.
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

# 보이는 명령을 먼저 찍고 실행한다.
x() { printf '+ %s\n' "$*"; "$@"; }
# 명령 줄에 임시 폴더·목록·복제본의 경로가 들어가는 것은 줄 대신 이름표를 찍는다(경로에 비공개 이름이 있을 수 있다).
xs() { local label=$1; shift; printf '+ %s\n' "$label"; "$@"; }

# 도구가 없으면 조용히 통과하지 않고 실패로 센다.
need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  echo "필요한 도구가 없다: $1 — 설치: $2"
  note="도구 없음: $1"
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
    status="건너뜀"
    skipped=$((skipped + 1))
  elif [ "$code" = 0 ]; then
    status="통과"
  else
    status="실패"
    failed=$((failed + 1))
  fi
  printf '%s|%s|%ss|%s\n' "$status" "$name" "$((SECONDS - start))" "$note" >>"$log"
}

# 공개될 파일(추적 + 새 파일, .gitignore 제외)만 임시 폴더에 복사한다. 링크는 링크로 둔다.
# git 목록을 못 읽거나 복사에 실패하거나 복사한 파일이 하나도 없으면 실패다(빈 폴더를 «깨끗하다»고 통과시키지 않는다).
snapshot() {
  local dest=$1 file copied=0
  git ls-files -z --cached --others --exclude-standard >"$work/files.list" 2>/dev/null || {
    echo "공개될 파일 목록을 읽지 못했다(git ls-files 실패)"
    return 1
  }
  while IFS= read -r -d '' file; do
    [ -e "$file" ] || [ -L "$file" ] || continue # 작업 트리에서 지운 파일은 공개되지 않는다
    [ -d "$file" ] && [ ! -L "$file" ] && continue # 하위 모듈 항목
    mkdir -p "$dest/$(dirname "$file")" && cp -P "$file" "$dest/$file" || {
      echo "공개될 파일을 복사하지 못했다"
      return 1
    }
    copied=$((copied + 1))
  done <"$work/files.list"
  [ "$copied" -gt 0 ] || {
    echo "공개될 파일이 하나도 없다"
    return 1
  }
}

# 공개할 참조(RELEASE_CHECK_REFS)와 지금 체크아웃한 커밋(HEAD)을 전체 복제해 고정한다. 이력 검사는 모두 이 복제본에서 하므로
# 검사하는 사이 저장소가 바뀌어도 같은 객체를 본다. HEAD를 함께 고정하는 것은 분리된 HEAD(예: CI의 병합 커밋 체크아웃)에는
# 브랜치가 하나도 없을 수 있기 때문이다. 얕은 복제는 이력이 잘려 있어 믿을 수 없으니 실패이고, 복제에 실패하거나 고정한 참조가
# 하나도 없어도 실패다(빈 이력을 «깨끗하다»고 통과시키지 않는다).
mirror=""
mirror_state=""
prepare_mirror() {
  [ "$mirror_state" = ok ] && return 0
  if [ "$mirror_state" = failed ]; then
    echo "고정한 복제본을 만들지 못해 이력을 검사할 수 없다(앞 단계의 메시지 참고)"
    return 1
  fi
  mirror_state=failed
  if [ "$strict" = 1 ] && [ "$(head_now)" != "$pinned_head" ]; then
    echo "시작할 때 고정한 커밋과 지금의 HEAD가 다르다: 검사 도중 HEAD가 바뀌었다. 같은 커밋에서 다시 돌린다."
    note="HEAD가 바뀜 — 고정한 커밋과 다르다"
    return 1
  fi
  local shallow pattern count repo="$work/pinned.git" specs=""
  shallow=$(git rev-parse --is-shallow-repository 2>/dev/null)
  if [ "$shallow" != false ]; then
    echo "얕은 복제이거나 확인하지 못했다: 이력이 잘려 있으면 이력 검사를 믿을 수 없다. 전체 복제본에서 돌린다."
    note="얕은 복제 — 전체 복제본이 필요하다"
    return 1
  fi
  git init --quiet --bare "$repo" 2>/dev/null || {
    echo "고정할 복제본을 만들지 못했다"
    return 1
  }
  set -f
  for pattern in ${RELEASE_CHECK_REFS:-refs/heads/* refs/tags/*}; do specs="$specs +$pattern:$pattern"; done
  set +f
  git rev-parse --verify -q HEAD >/dev/null 2>&1 && specs="$specs +HEAD:refs/pinned/head"
  # shellcheck disable=SC2086
  git --git-dir="$repo" fetch --quiet --no-tags --no-auto-gc "$root" $specs 2>/dev/null || {
    echo "공개할 참조를 복제하지 못했다"
    note="전체 복제 실패"
    return 1
  }
  count=$(git --git-dir="$repo" for-each-ref 2>/dev/null | wc -l | tr -d ' ')
  if [ "${count:-0}" -lt 1 ]; then
    echo "고정한 참조가 하나도 없다: RELEASE_CHECK_REFS가 아무것도 가리키지 않고 HEAD도 없다"
    note="고정한 참조 없음"
    return 1
  fi
  mirror=$repo
  mirror_state=ok
  echo "공개할 참조와 HEAD를 전체 복제해 고정했다: 참조 ${count}개"
}

# 작업 트리가 커밋과 같은가: 공개 대상은 커밋된 것뿐이다. 다르면 지금 검사한 상태와 공개될 상태가 다르다.
step_clean() {
  local out count
  out=$(git status --porcelain=v1 --untracked-files=all 2>/dev/null) || {
    echo "작업 트리 상태를 읽지 못했다(git status 실패)"
    return 1
  }
  if [ -n "$out" ]; then
    count=$(printf '%s\n' "$out" | wc -l | tr -d ' ')
    echo "커밋하지 않은 변경이 ${count}개 있다. 공개 대상은 커밋된 것뿐이라 검사한 상태와 공개될 상태가 다르다."
    note="커밋하지 않은 변경 ${count}개 — 공개 대상과 다르다"
    skip_flag=1
    return 0
  fi
  echo "작업 트리와 인덱스가 커밋과 같다"
}

step_fmt() { x cargo fmt --all -- --check; }

step_clippy() { x cargo clippy --workspace --all-targets --locked -- -D warnings; }

# 공개 직전에는 PostgreSQL 시험을 건너뛰고 통과할 수 없다: DB가 없거나 연결할 수 없으면 실패한다. 주소는 출력하지 않는다. 통과하면 시험이 DB를 필수로 쓰게 한다.
require_database() {
  local url=${BOT_TEST_DATABASE_URL:-}
  if [ -z "$url" ]; then
    echo "PostgreSQL 시험이 필수다(--strict·통합 시험): BOT_TEST_DATABASE_URL이 없다."
    note="DB 필수 — BOT_TEST_DATABASE_URL 없음"
    return 1
  fi
  need psql "PostgreSQL 클라이언트(CLI 시험 도우미가 쓴다)" || return 1
  if ! PGCONNECT_TIMEOUT=5 psql -X -q -At -d "$url" -c 'select 1' >/dev/null 2>&1; then
    echo "시험 DB에 연결하지 못했다(주소는 출력하지 않는다)."
    note="DB 연결 실패"
    return 1
  fi
  export BOT_TEST_REQUIRE_DB=1
}

step_test() {
  local url=${BOT_TEST_DATABASE_URL:-}
  if [ "$strict" = 1 ]; then
    require_database || return 1
    # 화면 규칙 시험(node)도 건너뛰지 않는다: node가 없으면 그 시험이 실패한다(CI도 같은 값을 건다)
    export BOT_REQUIRE_NODE=1
    echo "PostgreSQL 시험이 필수다(BOT_TEST_REQUIRE_DB=1): DB가 없거나 스키마를 만들지 못하면 건너뛰지 않고 실패한다."
    note="PostgreSQL 시험 필수·포함"
  elif [ -n "$url" ]; then
    echo "BOT_TEST_DATABASE_URL이 있어 PostgreSQL 시험까지 돈다."
    note="PostgreSQL 시험 포함"
  else
    echo "BOT_TEST_DATABASE_URL이 없어 PostgreSQL 시험은 건너뛴다(시험이 스스로 건너뜀)."
    note="PostgreSQL 시험은 건너뜀(BOT_TEST_DATABASE_URL 없음)"
  fi
  # 첫 실패에서 멈추지 않는다: 뒤의 크레이트·시험 파일도 돌려 실패를 한 번에 모두 보인다(CI에서 실패가 하나만 보이고 나머지는 돌지 않던 일을 막는다).
  # 판정은 그대로다: 실패가 하나라도 있으면 cargo가 0이 아닌 코드로 끝나 이 단계는 실패다.
  x cargo test --workspace --locked --no-fail-fast
}

# 필수 통합 시험: 실제 claude CLI·srt 도구 샌드박스·Apple container 모델 VM·합성 비밀 쓸어 보기(표준 오류·감사·저장소·수집기에 합성 비밀 0).
# 이 시험들은 환경이 없으면 스스로 «건너뛰고 통과»하므로 통과만으로는 돌았는지 알 수 없다. 그래서 필수 모드(*_CANARY=required)로 돌려 환경이 없으면 실패하게 하고,
# 시험이 하나도 안 돌았거나(통과 0건) 출력에 건너뜀 표지(`skipped:`·«건너뜀»·«건너뛴다»)가 하나라도 있으면 실패로 센다(건너뜀 0).
# 로컬 전용이다: CI에는 Apple container와 실제 claude가 없다(CI의 도구 샌드박스 단계는 따로 필수 모드로 돈다). 공개 직전(--strict)에는 반드시 돈다.
# 한 줄에 `패키지 시험파일 [기능] [ignored=N]`. `#[ignore]`된 시험은 돌지 않은 시험이라 통과가 아니다: 실제 ignored 수가 N(생략하면 0)과 **같아야** 한다 — 소스의 `#[ignore]` 수와 목록의 N은 selftest가 대조한다.
# 도구가 없으면 «건너뛰고 통과»할 수 있는 시험 파일(`*_CANARY`·`have_srt`·`skipped:`)은 모두 여기에 있어야 한다 —
# 빠지면 그 파일은 관문이 돌았는지 알 수 없다. 대조는 `scripts/test_integration_list.py`(selftest)가 소스를 훑어 한다.
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
'

step_integration() {
  if [ "$strict" != 1 ] && [ -z "$only" ]; then
    echo "건너뜀: 필수 통합 시험(실제 claude·srt·모델 VM)은 공개 직전(--strict)에 돌린다. 지금 보려면 --only integration."
    note="공개 직전(--strict)에만 돈다 — 건너뜀"
    skip_flag=1
    return 0
  fi
  require_database || return 1
  need cargo "rustup" || return 1
  if [ -z "${BOT_CLAUDE_BIN:-}" ]; then need claude "Claude Code CLI(또는 BOT_CLAUDE_BIN)" || return 1; fi
  if [ -z "${BOT_SRT_BIN:-}" ]; then need srt "npm install --global @anthropic-ai/sandbox-runtime (또는 BOT_SRT_BIN)" || return 1; fi
  need python3 "python3" || return 1
  need node "node" || return 1
  need container "brew install container" || return 1
  if ! container system status >/dev/null 2>&1; then
    echo "container 서비스가 꺼져 있다: container system start"
    note="container 서비스 꺼짐"
    return 1
  fi
  case "${BOT_MODEL_IMAGE:-}" in
    *@sha256:*) ;;
    *)
      echo "BOT_MODEL_IMAGE=<이름@sha256:…>가 필요하다(images/model/build.sh가 알려 준다)."
      note="BOT_MODEL_IMAGE 없음 — 모델 VM 시험을 돌릴 수 없다"
      return 1
      ;;
  esac
  if ! container image inspect "$BOT_MODEL_IMAGE" >/dev/null 2>&1; then
    echo "BOT_MODEL_IMAGE가 가리키는 이미지가 로컬에 없다(이미지는 내려받지 않는다)."
    note="모델 이미지가 로컬에 없다"
    return 1
  fi
  # 공식 OpenAI SDK(Python 2.29.0·Node 6.29.0) 시험 환경: 해시·잠금 고정 설치, 공식 OpenAPI 스키마(커밋·sha256)와 SDK 판본 확인. 이미 맞게 설치돼 있으면 내려받지 않는다(오프라인 재사용).
  # 저장소에는 설치하지 않는다 — 빌드 폴더 밑에만 둔다.
  sdk_dir=${BOT_OPENAI_SDK_DIR:-${CARGO_TARGET_DIR:-$PWD/target}/openai-sdk}
  case "$sdk_dir" in
    /*) ;;
    *)
      echo "BOT_OPENAI_SDK_DIR는 절대 경로여야 한다: $sdk_dir"
      note="BOT_OPENAI_SDK_DIR가 절대 경로가 아니다"
      return 1
      ;;
  esac
  if ! x "${RELEASE_CHECK_SDK_SETUP:-$here/openai-sdk-setup.sh}" "$sdk_dir"; then
    note="공식 OpenAI SDK 시험 환경을 만들지 못했다"
    return 1
  fi
  export BOT_OPENAI_SDK_DIR=$sdk_dir
  # 필수 모드: 환경이 없으면 시험이 건너뛰지 않고 실패한다
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
    skips=$(LC_ALL=C grep -a -c -E 'skipped:|건너뜀|건너뛴다' "$out")
    files=$((files + 1))
    total=$((total + passed))
    if [ "$code" != 0 ]; then
      echo "  실패  $target (종료 코드 $code) — 끝 부분:"
      tail -n 25 "$out" | sed 's/^/        /'
      bad=$((bad + 1))
    elif [ "$passed" = 0 ]; then
      echo "  실패  $target: 시험이 하나도 돌지 않았다"
      bad=$((bad + 1))
    elif [ "$ignored" != "$allowed" ]; then
      echo "  실패  $target: #[ignore]된 시험이 ${ignored}건이다(목록이 허용한 것은 ${allowed}건) — 돌지 않은 시험은 통과가 아니다"
      bad=$((bad + 1))
    elif [ "${skips:-0}" != 0 ]; then
      echo "  실패  $target: 건너뜀 표지 ${skips}건 — 환경이 없어 시험이 돌지 않았다"
      LC_ALL=C grep -a -E 'skipped:|건너뜀|건너뛴다' "$out" | head -n 3 | sed 's/^/        /'
      bad=$((bad + 1))
    else
      echo "  통과  $target: ${passed}건"
    fi
  done <<EOF
$INTEGRATION_TESTS
EOF
  echo "필수 통합 시험 ${files}개 파일·${total}건 통과, 건너뜀 표지 0이어야 통과다(실패 ${bad}개 파일)"
  note="필수 통합 ${files}개 파일·${total}건"
  [ "$bad" = 0 ]
}

# 600줄 제한·스키마·예시·CI 워크플로 하드닝(외부 액션은 전체 커밋 SHA·이미지는 다이제스트·최상위 `permissions: contents: read`·비밀 없음)·Rust 도구체인 고정. CI와 같은 스크립트를 쓴다.
step_contracts() {
  if ! python3 -I -c 'import jsonschema, yaml, tomllib' 2>/dev/null; then
    echo "파이썬 3.11 이상과 모듈이 필요하다(tomllib은 3.11부터 들어 있다) — 설치: python3 -m pip install jsonschema==4.25.1 PyYAML==6.0.2"
    note="도구 없음: 파이썬 3.11+·jsonschema·PyYAML"
    return 1
  fi
  x python3 -I .github/scripts/check_contracts.py
}

# 라이선스·금지 의존성·RustSec 권고(DB는 처음에만 내려받는다)·출처.
step_deny() {
  need cargo-deny "brew install cargo-deny" || return 1
  # shellcheck disable=SC2086
  x cargo deny --all-features --locked $deny_flags check --hide-inclusion-graph licenses bans advisories sources
}

# gitleaks를 JSON 보고서로 돌리고 요약만 찍는다. 자세한 출력(--verbose)은 작성자·메일·커밋 메시지·날짜를 그대로 찍어 비공개 이름이 샌다.
# 종료 코드: 0 깨끗함 · 2 발견(--exit-code 2) · 그 밖은 도구 오류. 보고서가 없으면 «깨끗함»이 아니라 실패다.
leaks() {
  local label=$1 report=$2 strip=$3 code
  shift 3
  printf '+ %s\n' "$label"
  "$@" --redact --no-banner --exit-code 2 --report-format json --report-path "$report" >"$work/gitleaks.out" 2>&1
  code=$?
  case $code in
    0) echo "비밀 값 없음" ;;
    2)
      echo "비밀 값 발견(값은 가렸다):"
      python3 -I "$here/gitleaks_summary.py" "$report" "$strip" || echo "  (요약을 만들지 못했다)"
      return 1
      ;;
    *)
      echo "gitleaks 실행 실패(종료 코드 $code)"
      return 1
      ;;
  esac
}

# 공개될 작업 트리 + 아직 커밋하지 않은 인덱스(staged) 내용.
step_secrets_tree() {
  need gitleaks "brew install gitleaks" || return 1
  local tmp code=0
  tmp=$(mktemp -d "$work/tree.XXXXXX") || return 1
  snapshot "$tmp" || return 1
  leaks "gitleaks dir <공개될 파일 복사본>" "$work/tree.json" "$tmp" gitleaks dir "$tmp" || code=1
  leaks "gitleaks git --staged" "$work/staged.json" "" gitleaks git . --staged || code=1
  return $code
}

# 고정한 복제본의 모든 참조·병합 해결을 포함한 이력 전체. `-m`이 있어야 병합에서 처음 생긴 값이 보인다.
step_secrets_history() {
  need gitleaks "brew install gitleaks" || return 1
  prepare_mirror || return 1
  leaks "gitleaks git <고정한 복제본> --log-opts=\"--all --full-history -m\"" "$work/history.json" "" \
    gitleaks git "$mirror" --log-opts="--all --full-history -m"
}

# 커밋·주석 태그의 작성자 메일이 모두 공개용 주소인가(고정한 복제본). 주소와 이름은 출력하지 않고 객체 해시만 보인다.
# git 조회에 실패하면 «문제 없음»이 아니라 실패다.
step_identity() {
  local domain=${RELEASE_CHECK_EMAIL_DOMAIN:-users.noreply.github.com} commits tags bad
  prepare_mirror || return 1
  commits=$(git --git-dir="$mirror" log --all --format='%h%x09%ae%x09%ce') || {
    echo "커밋을 읽지 못했다(git log 실패)"
    return 1
  }
  tags=$(git --git-dir="$mirror" for-each-ref refs/tags --format='%(objectname:short)%09%(taggeremail:trim)') || {
    echo "태그를 읽지 못했다(git for-each-ref 실패)"
    return 1
  }
  bad=$(
    printf '%s\n' "$commits" | awk -F'\t' -v d="@$domain" '
      function ok(s) { return length(s) > length(d) && substr(s, length(s) - length(d) + 1) == d }
      NF && !(ok($2) && ok($3)) { print "커밋 " $1 }'
    printf '%s\n' "$tags" | awk -F'\t' -v d="@$domain" '
      function ok(s) { return length(s) > length(d) && substr(s, length(s) - length(d) + 1) == d }
      NF && $2 != "" && !ok($2) { print "태그 " $1 }'
  )
  if [ -n "$bad" ]; then
    echo "공개용 주소(@$domain)가 아닌 작성자:"
    echo "$bad" | sed 's/^/  /'
    return 1
  fi
  echo "모든 커밋·태그의 작성자가 @$domain 주소다"
}

# 비공개 금칙어. 목록은 저장소 밖에 있고 없으면 «건너뜀»이다.
step_private_terms() {
  if [ -z "${BOT_PRIVATE_TERMS_FILE:-}" ]; then
    echo "건너뜀: BOT_PRIVATE_TERMS_FILE이 없어 비공개 금칙어 검사를 하지 않았다."
    echo "        공개 직전에는 목록 경로를 주고 --strict로 다시 돌린다."
    note="BOT_PRIVATE_TERMS_FILE 없음 — 금칙어 검사를 하지 않았다"
    skip_flag=1
    return 0
  fi
  if [ ! -f "$BOT_PRIVATE_TERMS_FILE" ]; then
    echo "금칙어 목록 파일이 없다"
    note="금칙어 목록 파일이 없다"
    return 1
  fi
  prepare_mirror || return 1
  xs "private_terms_check.py <금칙어 목록> --git-dir <고정한 복제본>" \
    python3 -I "$here/private_terms_check.py" "$BOT_PRIVATE_TERMS_FILE" --git-dir "$mirror"
}

# 금칙어 검사기와 관문 스크립트 자체의 시험(합성 목록·임시 저장소만 쓴다).
step_selftest() {
  # 시험이 관문을 다시 돌리다 이 단계에 닿으면 시험이 시험을 다시 부르는 순환이 된다: 안쪽 관문은 이 단계를 건너뛴다(눈에 띄는 «건너뜀»).
  if [ -n "${RELEASE_CHECK_IN_SELFTEST:-}" ]; then
    echo "건너뜀: 관문 자체 시험 안에서 돈 관문이다(시험이 시험을 다시 부르지 않는다)."
    note="관문 자체 시험 안 — 건너뜀"
    skip_flag=1
    return 0
  fi
  local code=0 file
  export RELEASE_CHECK_IN_SELFTEST=1
  for file in test_private_terms_check.py test_check_contracts.py test_integration_list.py test_release_check.py test_release_check_integration.py test_openai_sdk_results.py; do
    xs "python3 scripts/$file" python3 -I "$here/$file" || code=1
  done
  return $code
}

echo "공개 배포 관문"
printf '도구: cargo %s · rustc %s · cargo-deny %s · gitleaks %s · %s\n' \
  "$(cargo --version 2>/dev/null | cut -d' ' -f2)" "$(rustc --version 2>/dev/null | cut -d' ' -f2)" \
  "$(cargo-deny --version 2>/dev/null | cut -d' ' -f2)" "$(gitleaks version 2>/dev/null | head -1)" \
  "$(python3 --version 2>/dev/null)"
echo "검사할 커밋: ${pinned_head:-(커밋 없음)}"

run_step clean "작업 트리가 커밋과 같은가(공개 대상 = 검사 대상)" step_clean
run_step fmt "서식(cargo fmt --check)" step_fmt
run_step clippy "정적 검사(clippy -D warnings, 전체 대상)" step_clippy
run_step test "공개 시험(cargo test --workspace)" step_test
run_step integration "필수 통합 시험(실제 claude·srt·모델 VM·합성 비밀 쓸어 보기, 건너뜀 0)" step_integration
run_step contracts "600줄 제한·스키마·예시·CI 워크플로 하드닝·도구체인 고정" step_contracts
run_step deny "의존성(cargo deny: licenses bans advisories sources)" step_deny
run_step secrets-tree "비밀 값 — 공개될 작업 트리(gitleaks dir)" step_secrets_tree
run_step secrets-history "비밀 값 — 고정한 전체 복제본의 이력(gitleaks git --all --full-history -m)" step_secrets_history
run_step identity "커밋·태그 작성자 메일(고정한 복제본)" step_identity
run_step private-terms "비공개 금칙어(작업 트리·인덱스·고정한 복제본의 이력·Cargo.lock·숨김 파일)" step_private_terms
run_step selftest "금칙어 검사기·관문 스크립트 자체 시험" step_selftest

# 끝에서 HEAD와 공개할 참조가 시작할 때 기록한 값과 같은지 본다. 다르면 시험과 이력 확인이 서로 다른 커밋을 봤을 수 있다(같다고 해서 그 사이에 안 바뀌었다는 뜻은 아니다).
# --strict에서는 실패이고, 그 밖에는 눈에 띄는 «건너뜀»이다.
if [ "$(head_now)" != "$pinned_head" ] || [ "$(refs_state)" != "$pinned_refs" ]; then
  if [ "$strict" = 1 ]; then
    printf '실패|pin|0s|검사 도중 HEAD나 공개할 참조가 바뀌었다 — 고정한 커밋과 다른 상태를 검사했을 수 있다\n' >>"$log"
    failed=$((failed + 1))
  else
    printf '건너뜀|pin|0s|검사 도중 HEAD나 공개할 참조가 바뀌었다 — 결과는 시작할 때 커밋 기준이 아닐 수 있다\n' >>"$log"
    skipped=$((skipped + 1))
  fi
fi

printf '\n== 결과\n'
passed=0
while IFS='|' read -r status name secs detail; do
  [ "$status" = "통과" ] && passed=$((passed + 1))
  printf '  %-4s %-16s %6s  %s\n' "$status" "$name" "$secs" "$detail"
done <"$log"
printf '통과 %s · 실패 %s · 건너뜀 %s\n' "$passed" "$failed" "$skipped"
echo "검사한 커밋: ${pinned_head:-(커밋 없음)}"
if [ "$skipped" -gt 0 ]; then
  echo "주의: 건너뛴 검사가 있다. 건너뜀은 통과가 아니다(--strict는 이것도 실패로 센다)."
fi
[ "$failed" -gt 0 ] && exit 1
[ "$strict" = 1 ] && [ "$skipped" -gt 0 ] && exit 1
exit 0

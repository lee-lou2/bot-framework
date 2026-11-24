#!/usr/bin/env bash
# 모델 VM 이미지를 이 컴퓨터에서 빌드하고 다이제스트로 고정한다(Apple container, Apple silicon). 설명: docs/model-image.md
#
#   images/model/build.sh                  빌드 → 다이제스트 고정 → 참조를 알려 준다
#   NO_CACHE=1 images/model/build.sh       받기·서명 확인 단계를 캐시 없이 다시 한다
#
# 표준 출력에는 `sandbox.apple.image`에 넣을 참조 한 줄(`이름@sha256:…`)만 나오고, 진행·요약은 표준 오류로 나온다:
#   ref=$(images/model/build.sh)
# 환경 변수: MODEL_IMAGE_NAME(기본 bot-model-vm) · MODEL_IMAGE_REGISTRY_PORT(기본 5088, 127.0.0.1에만 잠깐 연다)
# 실제 VM(임시 레지스트리·점검 실행)을 띄우므로 같은 기계의 다른 VM 작업과 겹치지 않게 `/tmp/bot-vm-canary.lock` 잠금을 쥔다.
#
# 왜 레지스트리를 거치는가: `container build`가 만든 이미지는 이름:태그로만 찾을 수 있어(`이름@sha256:…`로는 조회·실행이 안 된다) 태그는 나중에 다른 이미지를
# 가리키게 바뀔 수 있다. 빌드한 이미지를 루프백 레지스트리(CNCF Distribution, 다이제스트로 고정)에 올렸다가 «다이제스트로» 다시 받으면 같은 내용이 불변 참조로
# 로컬에 남는다. 레지스트리는 그 사이에만 뜨고 끝나면 지운다(참조를 쓰는 데 레지스트리가 필요하지 않다).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
name=${MODEL_IMAGE_NAME:-bot-model-vm}
port=${MODEL_IMAGE_REGISTRY_PORT:-5088}
registry_image=docker.io/library/registry:3@sha256:82f6ff4a7f982960c8aa51617fea3aabc485df6e55cce80d22deb1de36f1bc3c
registry_name=bot-model-image-registry
stamp=$(date +%Y%m%d%H%M%S)-$$
tag=$name:build-$stamp
host_ref=127.0.0.1:$port/$name

say() { echo "$@" >&2; }
die() { say "오류: $*"; exit 1; }

case "${1:-}" in
  -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 0 ;;
  '') ;;
  *) die "알 수 없는 인자: $1 (--help 참고)" ;;
esac

# 실제 VM 작업은 한 번에 하나씩만 돈다: 같은 기계의 다른 VM 시험과 겹치면 가끔 실패한다(`/tmp/bot-vm-canary.lock`, 어댑터의 VM 시험도 같은 잠금을 쥔다).
# 잠금을 쥔 채 이 스크립트를 다시 실행한다(잠금은 이 프로세스가 끝나면 풀린다).
if [ -z "${BOT_VM_LOCKED:-}" ]; then
  BOT_VM_LOCKED=1 exec python3 -I -c 'import fcntl, os, sys
fd = os.open("/tmp/bot-vm-canary.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o644)
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    print("다른 VM 작업이 끝나기를 기다린다(/tmp/bot-vm-canary.lock)", file=sys.stderr)
    fcntl.flock(fd, fcntl.LOCK_EX)
os.set_inheritable(fd, True)
os.execvp(sys.argv[1], sys.argv[1:])' "${BASH:-bash}" "$0" "$@"
fi

command -v container >/dev/null 2>&1 || die "Apple container가 필요하다(brew install container)"
command -v curl >/dev/null 2>&1 || die "curl이 필요하다"
[ "$(uname -m)" = arm64 ] || die "Apple silicon(arm64) 전용이다"
container system status >/dev/null 2>&1 || die "container 서비스가 꺼져 있다: container system start"
case "$name" in '' | *[!a-z0-9._-]*) die "MODEL_IMAGE_NAME은 소문자·숫자·._-만 쓴다" ;; esac
case "$port" in '' | *[!0-9]*) die "MODEL_IMAGE_REGISTRY_PORT는 숫자여야 한다" ;; esac
if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
  die "포트 $port 를 이미 쓰고 있다: MODEL_IMAGE_REGISTRY_PORT로 다른 포트를 고른다"
fi

cleanup() {
  container stop "$registry_name" >/dev/null 2>&1 || true
  container delete --force "$registry_name" >/dev/null 2>&1 || true
  container image delete "$host_ref:$stamp" >/dev/null 2>&1 || true
  container image delete "$tag" >/dev/null 2>&1 || true
}
trap cleanup EXIT
# 이전 실행이 남긴 같은 이름의 임시 레지스트리가 있으면 지운다(이 스크립트가 만든 이름이다).
container delete --force "$registry_name" >/dev/null 2>&1 || true

progress=plain
[ -t 2 ] && progress=auto
say "==> 빌드(Containerfile의 고정값: 기반 이미지 다이제스트 · Claude Code 판본과 SHA-256 · 서명 확인)"
no_cache=()
[ -n "${NO_CACHE:-}" ] && no_cache=(--no-cache)
container build --platform linux/arm64 --tag "$tag" --file "$here/Containerfile" --progress "$progress" \
  ${no_cache[@]+"${no_cache[@]}"} "$here" >&2 || die "빌드가 실패했다(위 메시지 참고)"

say "==> 다이제스트 고정: 루프백 레지스트리 127.0.0.1:$port 에 올렸다가 다이제스트로 다시 받는다"
container image pull --platform linux/arm64 --progress none "$registry_image" >&2 || die "임시 레지스트리 이미지를 받지 못했다"
container run -d --rm --name "$registry_name" -p "127.0.0.1:$port:5000" "$registry_image" >/dev/null \
  || die "임시 레지스트리를 띄우지 못했다"
up=
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$port/v2/" >/dev/null 2>&1; then up=1; break; fi
  sleep 1
done
[ -n "$up" ] || die "임시 레지스트리가 응답하지 않는다"

container image tag "$tag" "$host_ref:$stamp" >&2
container image push --scheme http --platform linux/arm64 --progress none "$host_ref:$stamp" >&2 || die "올리지 못했다"
digest=$(curl -fsSI -H 'Accept: application/vnd.oci.image.index.v1+json' "http://127.0.0.1:$port/v2/$name/manifests/$stamp" \
  | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" { print $2 }')
case "$digest" in sha256:????????????????????????????????????????????????????????????????) ;; *) die "레지스트리가 다이제스트를 주지 않았다" ;; esac
ref=$host_ref@$digest

container image delete "$host_ref:$stamp" >/dev/null 2>&1 || true
container image pull --scheme http --platform linux/arm64 --progress none "$ref" >&2 || die "다이제스트로 다시 받지 못했다"
container image inspect "$ref" >/dev/null 2>&1 || die "다이제스트 참조가 조회되지 않는다"
# 올린 내용이 빌드한 내용과 같은가(색인 다이제스트의 앞 12자리를 빌드 태그의 것과 견준다)
short=${digest#sha256:}
short=${short:0:12}
built=$(container image list 2>/dev/null | awk -v n="$name" -v t="$tag" '$1 == n && $2 == substr(t, length(n) + 2) { print $3; exit }')
[ "$built" = "$short" ] || die "올린 이미지(${short})가 빌드한 이미지(${built:-?})와 다르다"

say "==> 레지스트리를 내리고 다이제스트 참조만으로 실행되는지 확인한다"
container stop "$registry_name" >/dev/null 2>&1 || true
# init을 거쳐 실행해 이 컴퓨터의 container 커널에서 nft 규칙과 권한 강등이 되는지까지 본다(릴레이 값은 쓰이지 않는 사설 주소).
version=$(container run --progress none --rm --name "bot-model-image-check-$$" \
  --cap-drop ALL --cap-add CAP_NET_ADMIN --cap-add CAP_SETUID --cap-add CAP_SETGID --cap-add CAP_SETPCAP \
  -e BOT_RELAY_IP=10.0.0.1 -e BOT_RELAY_PORT=1 "$ref" claude --version 2>/dev/null | tail -1) || true
case "$version" in *"(Claude Code)") ;; *) die "다이제스트 참조로 init을 거쳐 실행되지 않는다(container run으로 직접 확인한다)" ;; esac

say ""
say "이미지 참조: $ref"
say "Claude Code : $version"
say "크기        : $(container image list --verbose 2>/dev/null | awk -v n="$host_ref" -v d="$short" '$1 == n && $3 == d { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9.]+$/ && $(i + 1) ~ /^[KMG]B$/) { print $i, $(i + 1); exit } }') (압축 기준)"
say ""
say "host 설정(BOT_HOST_CONFIG)에 넣는다:"
say "  \"sandbox\": { \"apple\": { \"image\": \"$ref\", \"network\": \"<호스트 전용 망 이름>\", \"relay_bind\": \"<망의 호스트 주소:포트>\" } }"
say "점검(init 거절·방화벽·권한 강등·닻·대화·세션): images/model/verify.sh \"$ref\""
say "이 참조를 지우지 말 것: container image prune -a는 쓰이지 않는 이 이미지도 지운다."
echo "$ref"

#!/usr/bin/env bash
# 모델 VM 이미지 점검: 어댑터(crates/bot-runtime/src/model/container/args.rs)와 같은 옵션으로 이미지를 돌려 본다. 실제 모델·실제 키는 쓰지 않는다.
#
#   images/model/verify.sh <이미지 참조>        예: build.sh가 알려 준 이름@sha256:…
#
# 확인하는 것
#   1 claude --version이 init(ENTRYPOINT)을 거쳐 읽기 전용 루트에서 실행되고 이미지 라벨의 판본과 같다. 이미지 안의 init이 저장소의 것과 같다
#   2 init 거절: 릴레이 환경이 없거나 잘못되었을 때 · capability가 하나라도 빠졌을 때 · root가 아닐 때 · 첫 인자가 claude가 아닐 때
#     → 종료 코드 0이 아님, init의 stderr는 `vm_init_failed` 한 단어, 모델(시험 대역)은 시작되지 않음
#   3 방화벽(init이 건 nft)과 권한 강등: 모델 프로세스는 uid·gid 10001, 보조 그룹·capability 0, no_new_privs이고
#     릴레이에만 닿는다 — 같은 게이트웨이의 다른 포트 · 이웃 VM(IPv4·IPv6) · 호스트의 IPv6 · 호스트의 LAN 주소 · 인터넷은 막힌다.
#     규칙을 바꾸려는 시도는 모두 실패하고, 이웃이 모델 VM에 들어오려는 시도도 막힌다. 방화벽 없는 대조 실행은 그 길들이 열려 있음을 보인다.
#   4 읽기 전용 루트·/tmp(tmpfs)·/work 쓰기, 끄는 환경변수가 모델 환경에 있고 BOT_RELAY_*는 없다
#   5 닻(bot-anchor): 비root·capability 0로만 돌고 계속 떠 있다. root거나 capability가 있으면 `vm_anchor_failed`로 끝난다
#   6 로컬 가짜 상류와 실제 `claude -p --bare` 대화(방화벽 안에서)가 끝나고, 상류는 `/v1/messages` 한 건만 받으며 다른 호스트로 나가려는 시도가 0이다.
#     대조: 끄는 환경변수를 비우면 그 시도가 기록된다
#   7 0700 작업 폴더에 uid 10001이 세션 기록을 쓰고, 새 컨테이너가 `--resume`으로 잇고, 호스트가 그 기록을 읽고 지운다
# 점검용으로 잠깐 만드는 것: 호스트 전용 망(부분망은 비어 있는 것을 골라 고정한다: `BOT_MODEL_VERIFY_SUBNET`으로 정할 수 있다) · 닻·이웃·점검 도구 컨테이너(모두 그 망에서) ·
# 임시 폴더 · 가짜 상류와 IPv6 수신기(python3). 끝나면 모두 지운다. 실제 VM을 띄우므로 같은 기계의 다른 VM 작업과 겹치지 않게 잠금(`/tmp/bot-vm-canary.lock`)을 쥔다.
# 점검 도구 busybox(GPL-2.0)는 임시 폴더에만 있다. 하나라도 실패하면 종료 코드 1이다.
# (`set -e`를 쓰지 않는다: 실패한 점검이 스크립트를 멈추지 않고 «실패» 줄로 남아야 한다.)
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
image=${1:-}
case "$image" in '' | -h | --help) sed -n '2,/^set -uo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2 ;; esac
for tool in container python3 curl uuidgen; do
  command -v "$tool" >/dev/null 2>&1 || { echo "오류: $tool 이(가) 필요하다" >&2; exit 2; }
done

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

pid=$$
net=bot-model-verify-$pid
anchor=bot-model-verify-anchor-$pid
neighbor=bot-model-verify-neighbor-$pid
tmp=$(mktemp -d)
bgpids=()
passed=0
failed=0
skipped=0
builder=$(sed -n 's/^ARG BUILDER=//p' "$here/Containerfile" | head -1)
default_caps="CAP_NET_ADMIN CAP_SETUID CAP_SETGID CAP_SETPCAP"

cleanup() {
  local p
  for p in ${bgpids[@]+"${bgpids[@]}"}; do kill "$p" >/dev/null 2>&1; done
  for c in "$anchor" "$neighbor" "bot-model-verify-run-$pid"; do
    container stop "$c" >/dev/null 2>&1
    container delete --force "$c" >/dev/null 2>&1
  done
  container network delete "$net" >/dev/null 2>&1
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
die() { echo "오류: $*" >&2; exit 1; }

check() { # check <설명> — 직전 명령의 종료 코드가 0이면 통과. 사용: `조건; check "…"`
  local code=$?
  if [ "$code" = 0 ]; then passed=$((passed + 1)); echo "  통과  $1"; else failed=$((failed + 1)); echo "  실패  $1"; fi
}
skip() { skipped=$((skipped + 1)); echo "  건너뜀  $1"; }
has() { printf '%s\n' "$1" | grep -qxF -- "$2"; } # has <출력> <한 줄 전체>

# 시간이 지나면 명령을 죽이고 142를 돌려 준다(init이 실수로 대기 경로로 빠져 매달리는 일을 잡는다). perl이 없으면 시간 제한 없이 돌린다.
with_timeout() {
  local limit=$1
  shift
  if command -v perl >/dev/null 2>&1; then perl -e 'alarm shift; exec @ARGV or die' "$limit" "$@"; else "$@"; fi
}

# 어댑터와 같은 옵션으로 모델 VM을 돌린다: run_model <이름> [추가 container 옵션…] -- <init에 넘길 인자…>
# m_reset 값(M_*)을 호출 전에 바꿔 «빠진 capability»·«잘못된 릴레이» 같은 경우를 만든다. M_PROBE=1이면 시험 대역(/dbg 도구, /usr/local/bin의 claude 대역)을 붙인다.
m_reset() { M_CAPS=$default_caps; M_RIP=$gateway; M_RPORT=$port; M_PROBE=1; M_USER=; }
run_model() {
  local name=$1
  shift
  local extra=()
  while [ "$1" != -- ]; do extra+=("$1"); shift; done
  shift
  local flags=() c
  for c in $M_CAPS; do flags+=(--cap-add "$c"); done
  [ -n "$M_RIP" ] && flags+=(-e "BOT_RELAY_IP=$M_RIP")
  [ -n "$M_RPORT" ] && flags+=(-e "BOT_RELAY_PORT=$M_RPORT")
  [ -n "$M_USER" ] && flags+=(--user "$M_USER")
  if [ -n "$M_PROBE" ]; then
    flags+=(--mount "type=bind,source=$tmp/tools,target=/dbg,readonly" --mount "type=bind,source=$tmp/bin,target=/usr/local/bin,readonly")
  fi
  with_timeout 240 container run --progress none --rm --read-only --tmpfs /tmp --cap-drop ALL --init --no-dns --network "$net" -c 2 -m 2048M \
    --label bot.managed=verify --name "$name" -w /bot \
    --mount "type=bind,source=$tmp/in,target=/bot,readonly" --mount "type=bind,source=$tmp/work,target=/work" \
    ${flags[@]+"${flags[@]}"} ${extra[@]+"${extra[@]}"} "$image" "$@"
  local code=$?
  container delete --force "$name" >/dev/null 2>&1
  return "$code"
}
ip4_of() { # ip4_of <컨테이너> — 망에서 받은 IPv4 주소(접두 길이 없이)
  container inspect "$1" | python3 -c 'import json, sys; print(json.load(sys.stdin)[0]["status"]["networks"][0]["ipv4Address"].split("/")[0])'
}
# IPv6는 링크 로컬 주소(fe80::…, 쓸 때 %eth0)로 시험한다: 라우터 광고를 받아야 생기는 전역 주소와 달리 인터페이스가 올라오면 바로 있다.
ll6_of() { container exec "$1" /dbg/busybox ip -6 addr show dev eth0 2>/dev/null | sed -n 's/.*inet6 \(fe80[0-9a-f:]*\)\/.*/\1/p' | head -1; }

# 다른 망·휴면 망과 겹치지 않는 부분망을 고른다(부분망을 안 주면 다른 프로젝트의 휴면 망과 같은 대역이 배정될 수 있다).
pick_subnet() {
  local used n
  used=$(container network list 2>/dev/null | awk 'NR > 1 { print $2 }')
  for n in $(seq 200 239); do
    case "$used" in *"192.168.$n.0/24"*) ;; *) echo "192.168.$n.0/24"; return 0 ;; esac
  done
  return 1
}

echo "==> 준비: 호스트 전용 망 · 점검 도구 · 닻 · 이웃 VM · 가짜 상류"
mkdir -p "$tmp/tools" "$tmp/in" "$tmp/work" "$tmp/bin"
chmod 700 "$tmp/in" "$tmp/work"
subnet=${BOT_MODEL_VERIFY_SUBNET:-$(pick_subnet)}
[ -n "$subnet" ] || die "비어 있는 부분망(192.168.200~239.0/24)이 없다: BOT_MODEL_VERIFY_SUBNET으로 정한다"
container network create --internal --label bot.managed=verify --subnet "$subnet" "$net" >/dev/null || die "호스트 전용 망을 만들지 못했다(부분망 $subnet)"
# 점검용 보조 VM도 이 호스트 전용 망에서 돌린다(기본 NAT 망에서 돌린 직후 같은 기계의 호스트 전용 망 VM이 `NO-CARRIER`로 뜨는 일이 있었다)
container run --progress none --rm --name "bot-model-verify-tools-$pid" --network "$net" --mount "type=bind,source=$tmp/tools,target=/out" "$builder" \
  /bin/sh -c 'cp /bin/busybox /out/busybox && cd /out && for a in $(/bin/busybox --list); do ln -s busybox "$a"; done' >/dev/null 2>&1 \
  || die "점검 도구를 준비하지 못했다"
# 닻: 어댑터가 릴레이를 바인드하기 전에 먼저 띄우는 비root·capability 0 대기 전용 진입(호스트 쪽 게이트웨이 주소는 VM이 붙어 있을 때만 생긴다)
container run --progress none -d --rm --name "$anchor" --network "$net" --user 10001:10001 --cap-drop ALL --read-only --tmpfs /tmp --init --no-dns \
  --entrypoint /usr/local/bin/bot-anchor "$image" >/dev/null || die "닻을 띄우지 못했다(참조를 확인한다: container image inspect)"
# 이웃 VM: 같은 망의 다른 VM(IPv4 9000 · IPv6 9001에서 듣는다)
container run --progress none -d --rm --name "$neighbor" --network "$net" --init --no-dns --read-only --tmpfs /tmp --cap-drop ALL \
  --mount "type=bind,source=$tmp/tools,target=/dbg,readonly" --entrypoint /dbg/busybox "$image" \
  sh -c '/dbg/busybox nc -lk -p 9000 -e /dbg/true & exec /dbg/busybox nc -lk -s :: -p 9001 -e /dbg/true' >/dev/null || die "이웃 VM을 띄우지 못했다"
sleep 2
gateway=$(container network inspect "$net" | sed -n 's/.*"ipv4Gateway" *: *"\([0-9.]*\)".*/\1/p' | head -1)
[ -n "$gateway" ] || die "호스트 전용 망의 게이트웨이 주소를 읽지 못했다"
neighbor4=$(ip4_of "$neighbor")
neighbor6=$(ll6_of "$neighbor")
[ -n "$neighbor4" ] && [ -n "$neighbor6" ] || die "이웃 VM의 주소를 읽지 못했다"
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port=$(free_port)
other=$(free_port)
python3 "$here/fake_upstream.py" "$gateway" "$port" >>"$tmp/upstream.log" 2>&1 &
bgpids+=($!)
python3 "$here/fake_upstream.py" "$gateway" "$other" >/dev/null 2>&1 &
bgpids+=($!)
for _ in $(seq 1 20); do
  curl -s -o /dev/null -m 1 "http://$gateway:$port/__ready" && curl -s -o /dev/null -m 1 "http://$gateway:$other/__ready" && break
  sleep 0.5
done
# 호스트의 IPv6(링크 로컬) 수신기와 LAN 주소의 열린 포트(호스트에서 모든 인터페이스에 바인드한 서비스가 있으면 그것이 «호스트의 다른 서비스» 대조가 된다)
bridge=$(ifconfig 2>/dev/null | awk -v gw="$gateway" '/^[a-z0-9]+:/ { i = $1 } index($0, "inet " gw " ") { print i }' | tr -d ':')
host6=
[ -n "$bridge" ] && host6=$(ifconfig "$bridge" 2>/dev/null | awk '/inet6 fe80/ { print $2; exit }' | sed 's/%.*//')
host6port=$(free_port)
if [ -n "$host6" ]; then
  python3 - "$host6" "$bridge" "$host6port" >/dev/null 2>&1 <<'PY' &
import socket, sys, time
host, bridge, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
s = socket.socket(socket.AF_INET6)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((host, port, 0, socket.if_nametoindex(bridge)))
s.listen(5)
s.settimeout(1)
end = time.time() + 600
while time.time() < end:
    try:
        c, _ = s.accept()
        c.close()
    except Exception:
        pass
PY
  bgpids+=($!)
fi
lan_if=$(route -n get default 2>/dev/null | awk '/interface:/ { print $2 }')
hostlan=
[ -n "$lan_if" ] && hostlan=$(ipconfig getifaddr "$lan_if" 2>/dev/null)
hostlanport=22
if [ -n "$hostlan" ] && ! nc -z -w 1 "$hostlan" "$hostlanport" >/dev/null 2>&1; then hostlan=; fi
echo "    망 $net($subnet) · 게이트웨이 $gateway · 릴레이(가짜 상류) 포트 $port · 같은 게이트웨이의 다른 포트 $other · 이웃 $neighbor4 / $neighbor6"
h6txt=none
[ -n "$host6" ] && h6txt="$host6%$bridge:$host6port"
lantxt=none
[ -n "$hostlan" ] && lantxt="$hostlan:$hostlanport"
echo "    호스트 IPv6 수신기 $h6txt · 호스트 LAN 열린 포트 $lantxt"

# 시험 대역: init이 권한을 버린 뒤 고정 경로(/usr/local/bin/claude)로 exec하는 프로세스를 대신한다. init도 이 이미지 안의 것을 그대로 옮겨 둔다(같은 폴더를 /usr/local/bin에 붙인다).
container run --progress none --rm --network "$net" --mount "type=bind,source=$tmp/tools,target=/dbg,readonly" --entrypoint /dbg/cat "$image" /usr/local/bin/bot-init >"$tmp/bin/bot-init" 2>/dev/null
chmod 755 "$tmp/bin/bot-init"
cat >"$tmp/bin/claude" <<'PROBE'
#!/bin/sh
B=/dbg/busybox
if [ "${P_MODE-}" = marker ]; then : > /work/started; exit 0; fi
$B grep -E '^(Uid|Gid|Groups|CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs):' /proc/self/status | $B tr '\t' ' ' | $B sed 's/^/status /'
echo "mount root $($B grep ' / ' /proc/mounts | $B cut -d' ' -f3,4)"
for v in HOME CLAUDE_CONFIG_DIR DISABLE_AUTOUPDATER DISABLE_UPDATES CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC DISABLE_TELEMETRY DISABLE_ERROR_REPORTING \
  DO_NOT_TRACK DISABLE_FEEDBACK_COMMAND CLAUDE_CODE_DISABLE_FEATURE_FLAGS CLAUDE_CODE_DISABLE_TERMINAL_TITLE \
  CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL CLAUDE_CODE_DISABLE_AUTO_MEMORY CLAUDE_CODE_DISABLE_BUNDLED_SKILLS BOT_RELAY_IP BOT_RELAY_PORT; do
  eval "val=\${$v-<unset>}"
  echo "env $v=$val"
done
w() { if $B touch "$2" 2>/dev/null; then echo "write $1 ok"; $B rm -f "$2"; else echo "write $1 denied"; fi; }
w root /x; w etc /etc/x; w usr /usr/x; w bin /bin/x; w lib /lib/x; w bot /bot/x; w tmp /tmp/x; w work /work/x
# IPv6 주소는 중복 주소 검사(DAD)가 끝나야 쓸 수 있다. 그 전의 «막힘»은 방화벽 때문이 아니니 기다린다.
n=0
while $B ip -6 addr show dev eth0 2>/dev/null | $B grep -q tentative && [ "$n" -lt 20 ]; do $B sleep 0.5; n=$((n + 1)); done
p() { if $B nc -w 3 "$2" "$3" </dev/null >/dev/null 2>&1; then echo "open $1"; else echo "blocked $1"; fi; }
p relay "$P_RELAY" "$P_RPORT"
p other-port "$P_RELAY" "$P_OTHER"
p neighbor-v4 "$P_PEER4" 9000
p neighbor-v6 "$P_PEER6%eth0" 9001
[ -n "${P_HOST6-}" ] && p host-v6 "$P_HOST6%eth0" "$P_HOST6PORT"
[ -n "${P_HOSTLAN-}" ] && p host-lan "$P_HOSTLAN" "$P_HOSTLANPORT"
p internet-443 1.1.1.1 443
p internet-53 8.8.8.8 53
if $B unshare -Urn $B nc -w 3 "$P_RELAY" "$P_RPORT" </dev/null >/dev/null 2>&1; then echo "open newns-relay"; else echo "blocked newns-relay"; fi
t() { n=$1; shift; if "$@" >/dev/null 2>&1; then echo "tamper $n ALLOWED"; else echo "tamper $n denied"; fi; }
t nft-flush /usr/sbin/nft flush ruleset
t nft-add /usr/sbin/nft add rule inet bot_fw output accept
t setpriv-root /bin/setpriv --reuid=0 -- /dbg/true
t ip-link-down $B ip link set eth0 down
t ip-addr-add $B ip addr add 10.9.9.9/24 dev eth0
t ip-route-add $B ip route add default via "$P_RELAY"
t su-root $B su root -c /dbg/true
t chmod-nft $B chmod 755 /usr/sbin/nft
t remount-rw $B mount -o remount,rw /
t sysctl-forward $B sh -c 'echo 1 > /proc/sys/net/ipv4/ip_forward'
p relay-after "$P_RELAY" "$P_RPORT"
$B ip -4 addr show eth0 | $B sed -n 's/.*inet \([0-9.]*\)\/.*/\1/p' | $B head -1 >/work/ip4
$B ip -6 addr show dev eth0 | $B sed -n 's/.*inet6 \(fe80[0-9a-f:]*\)\/.*/\1/p' | $B head -1 >/work/ip6
$B nc -lk -p 9100 -e /dbg/true >/dev/null 2>&1 &
$B nc -lk -s :: -p 9101 -e /dbg/true >/dev/null 2>&1 &
: > /work/ready
i=0
while [ ! -e /work/stop ] && [ "$i" -lt 240 ]; do $B sleep 0.5; i=$((i + 1)); done
PROBE
chmod 755 "$tmp/bin/claude"
probe_env=(-e "P_RELAY=$gateway" -e "P_RPORT=$port" -e "P_OTHER=$other" -e "P_PEER4=$neighbor4" -e "P_PEER6=$neighbor6")
[ -n "$host6" ] && probe_env+=(-e "P_HOST6=$host6" -e "P_HOST6PORT=$host6port")
[ -n "$hostlan" ] && probe_env+=(-e "P_HOSTLAN=$hostlan" -e "P_HOSTLANPORT=$hostlanport")

echo "==> 1 판본과 init"
m_reset
M_PROBE=
version=$(run_model "bot-model-verify-run-$pid" -- claude --version 2>"$tmp/err")
label=$(container image inspect "$image" | sed -n 's/.*"bot.claude-code.version" *: *"\([^"]*\)".*/\1/p' | head -1)
echo "    출력: $version · 이미지 라벨: ${label:-?}"
[ -n "$label" ] && [ "$version" = "$label (Claude Code)" ] && [ ! -s "$tmp/err" ]; check "claude --version이 init을 거쳐 읽기 전용 루트에서 실행되고 라벨의 판본과 같다"
cmp -s "$tmp/bin/bot-init" "$here/bot-init"; check "이미지 안의 bot-init이 저장소의 images/model/bot-init과 같다"

echo "==> 2 init 거절(모델이 시작되지 않는다)"
expect_init_fail() { # expect_init_fail <설명> [추가 옵션…] -- <인자…>
  local desc=$1 code
  shift
  rm -f "$tmp/work/started"
  run_model "bot-model-verify-run-$pid" -e P_MODE=marker "$@" >"$tmp/rej.out" 2>"$tmp/rej.err"
  code=$?
  [ "$code" != 0 ] && [ "$(cat "$tmp/rej.err")" = vm_init_failed ] && [ ! -s "$tmp/rej.out" ] && [ ! -e "$tmp/work/started" ]; check "$desc (종료 $code)"
}
m_reset; run_model "bot-model-verify-run-$pid" -e P_MODE=marker -- claude >/dev/null 2>&1
[ -e "$tmp/work/started" ]; check "대조: 올바른 입력이면 init이 모델(시험 대역)을 시작한다"
m_reset; M_RIP=; M_RPORT=; expect_init_fail "BOT_RELAY_IP·BOT_RELAY_PORT가 없다" -- claude
m_reset; M_RPORT=; expect_init_fail "BOT_RELAY_PORT만 없다" -- claude
m_reset; M_RIP=; expect_init_fail "BOT_RELAY_IP만 없다" -- claude
for bad in 8.8.8.8 127.0.0.1 169.254.1.1 172.32.0.1 192.168.01.1 10.1.2 10.1.2.256 '10.1.2.3;x' '::1'; do
  m_reset; M_RIP=$bad; expect_init_fail "릴레이 IP가 사설 IPv4가 아니거나 잘못되었다: $bad" -- claude
done
for bad in 0 65536 080 '80;x' 1e3 0x50; do
  m_reset; M_RPORT=$bad; expect_init_fail "릴레이 포트가 잘못되었다: $bad" -- claude
done
for missing in CAP_NET_ADMIN CAP_SETUID CAP_SETGID CAP_SETPCAP; do
  m_reset; M_CAPS=$(echo "$default_caps" | sed "s/$missing//"); expect_init_fail "capability ${missing}가 빠졌다" -- claude
done
m_reset; M_CAPS=; expect_init_fail "capability가 하나도 없다" -- claude
m_reset; M_USER=10001:10001; expect_init_fail "root가 아닌 사용자로 시작했다" -- claude
m_reset; expect_init_fail "첫 인자가 claude가 아니다(sh)" -- sh -c 'touch /work/started'
m_reset; expect_init_fail "첫 인자가 claude의 전체 경로다" -- /usr/local/bin/claude

echo "==> 3·4 방화벽 · 권한 강등 · 규칙 변경 시도 · 쓰기 · 환경 (시험 대역을 init 경유로 실행)"
inbound() { # 이웃이 모델 VM(주소 $1·$2)의 수신기에 접속해 본다
  local out4 out6
  out4=$(container exec "$neighbor" /dbg/busybox sh -c "if /dbg/busybox nc -w 3 $1 9100 </dev/null >/dev/null 2>&1; then echo open; else echo blocked; fi" 2>&1 | tail -1)
  out6=$(container exec "$neighbor" /dbg/busybox sh -c "if /dbg/busybox nc -w 3 $2%eth0 9101 </dev/null >/dev/null 2>&1; then echo open; else echo blocked; fi" 2>&1 | tail -1)
  echo "inbound-v4 $out4"
  echo "inbound-v6 $out6"
}
probe_run() { # probe_run <이름> <출력 파일> [모델 VM 옵션…] -- <인자…>: 시험 대역을 돌리고 이웃의 접속 시도 결과를 덧붙인다
  local name=$1 file=$2
  shift 2
  rm -f "$tmp/work/ready" "$tmp/work/stop"
  run_model "$name" "$@" >"$file" 2>&1 &
  local job=$!
  for _ in $(seq 1 240); do [ -e "$tmp/work/ready" ] && break; kill -0 "$job" 2>/dev/null || break; sleep 0.5; done
  if [ -e "$tmp/work/ready" ]; then inbound "$(cat "$tmp/work/ip4")" "$(cat "$tmp/work/ip6")" >>"$tmp/inbound.out"; fi
  : >"$tmp/work/stop"
  wait "$job"
}
m_reset
: >"$tmp/inbound.out"
probe_run "bot-model-verify-run-$pid" "$tmp/fw.out" "${probe_env[@]}" -- claude
fw=$(cat "$tmp/fw.out" "$tmp/inbound.out")
echo "$fw" | grep -E '^(open|blocked|tamper) ' | sed 's/^/    /'
has "$fw" "status Uid: 10001 10001 10001 10001" && has "$fw" "status Gid: 10001 10001 10001 10001"; check "모델 프로세스의 실제·유효·저장·fs uid와 gid가 모두 10001이다"
echo "$fw" | grep -q '^status Groups: *$'; check "보조 그룹이 없다"
[ "$(echo "$fw" | grep -cE '^status Cap(Inh|Prm|Eff|Bnd|Amb): 0000000000000000$')" = 5 ]; check "capability(Inh·Prm·Eff·Bnd·Amb)가 모두 0이다"
has "$fw" "status NoNewPrivs: 1"; check "no_new_privs가 켜져 있다"
has "$fw" "open relay" && has "$fw" "open relay-after"; check "릴레이(게이트웨이:포트)에는 닿고, 규칙 변경을 시도한 뒤에도 닿는다"
has "$fw" "blocked other-port"; check "같은 게이트웨이의 다른 포트는 막힌다"
has "$fw" "blocked neighbor-v4" && has "$fw" "blocked neighbor-v6"; check "이웃 VM은 IPv4·IPv6 모두 막힌다"
has "$fw" "blocked internet-443" && has "$fw" "blocked internet-53"; check "인터넷(1.1.1.1:443 · 8.8.8.8:53)이 막힌다"
if [ -n "$host6" ]; then has "$fw" "blocked host-v6"; check "호스트의 IPv6 서비스는 막힌다"; else skip "호스트의 IPv6 서비스(호스트 전용 망의 브리지 주소를 읽지 못했다)"; fi
if [ -n "$hostlan" ]; then has "$fw" "blocked host-lan"; check "호스트 LAN 주소에서 모든 인터페이스로 열린 서비스(포트 $hostlanport)는 막힌다"; else skip "호스트 LAN 주소의 열린 서비스(호스트에 열린 포트 22가 없다)"; fi
has "$fw" "blocked newns-relay"; check "새 네트워크 이름공간을 만들어도 우회가 되지 않는다"
[ "$(echo "$fw" | grep -c '^tamper .* denied$')" = 10 ] && ! echo "$fw" | grep -q 'ALLOWED'; check "규칙·주소·경로·권한을 바꾸려는 시도 10가지가 모두 거절된다"
has "$fw" "inbound-v4 blocked" && has "$fw" "inbound-v6 blocked"; check "이웃 VM이 모델 VM에 들어오려는 시도(IPv4·IPv6)가 막힌다"
echo "$fw" | grep -q '^mount root ext4 ro,'; check "루트가 읽기 전용으로 붙는다"
[ "$(echo "$fw" | grep -cE '^write (root|etc|usr|bin|lib|bot) denied$')" = 6 ] && has "$fw" "write tmp ok" && has "$fw" "write work ok"; check "/ · /etc · /usr · /bin · /lib · /bot는 쓰기가 막히고 /tmp(tmpfs)와 /work는 쓸 수 있다"
envs_ok=1
for v in DISABLE_AUTOUPDATER DISABLE_UPDATES CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC DISABLE_TELEMETRY DISABLE_ERROR_REPORTING DO_NOT_TRACK DISABLE_FEEDBACK_COMMAND \
  CLAUDE_CODE_DISABLE_FEATURE_FLAGS CLAUDE_CODE_DISABLE_TERMINAL_TITLE CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL CLAUDE_CODE_DISABLE_AUTO_MEMORY CLAUDE_CODE_DISABLE_BUNDLED_SKILLS; do
  has "$fw" "env $v=1" || envs_ok=
done
[ -n "$envs_ok" ] && has "$fw" "env HOME=/work/home" && has "$fw" "env CLAUDE_CONFIG_DIR=/work/claude"; check "끄는 환경변수 12개와 HOME·CLAUDE_CONFIG_DIR이 모델 환경에 있다"
has "$fw" "env BOT_RELAY_IP=<unset>" && has "$fw" "env BOT_RELAY_PORT=<unset>"; check "모델은 BOT_RELAY_IP·BOT_RELAY_PORT를 받지 않는다(init이 지운다)"

echo "==> 3 대조: 방화벽 없이(진입점을 busybox로 바꾼 일반 실행, root) — 같은 길이 열려 있어야 위 «막힘»이 의미가 있다"
: >"$tmp/inbound.out"
m_reset
M_CAPS=
probe_run "bot-model-verify-run-$pid" "$tmp/ctl.out" "${probe_env[@]}" --entrypoint /dbg/busybox -- sh /usr/local/bin/claude
ctl=$(cat "$tmp/ctl.out" "$tmp/inbound.out")
echo "$ctl" | grep -E '^(open|blocked) ' | grep -v -E 'newns|relay-after' | sed 's/^/    /'
has "$ctl" "open relay" && has "$ctl" "open other-port" && has "$ctl" "open neighbor-v4" && has "$ctl" "open neighbor-v6"; check "대조: 방화벽이 없으면 릴레이·다른 포트·이웃 VM(IPv4·IPv6)이 모두 열려 있다"
[ -n "$host6" ] && { has "$ctl" "open host-v6"; check "대조: 방화벽이 없으면 호스트의 IPv6 서비스가 열려 있다"; }
[ -n "$hostlan" ] && { has "$ctl" "open host-lan"; check "대조: 방화벽이 없으면 호스트 LAN 주소의 열린 서비스가 열려 있다"; }
has "$ctl" "inbound-v4 open" && has "$ctl" "inbound-v6 open"; check "대조: 방화벽이 없으면 이웃이 이 VM에 들어올 수 있다"
has "$ctl" "blocked internet-443" && has "$ctl" "blocked internet-53"; check "호스트 전용(--internal) 망만으로도 인터넷·DNS 서버로는 나가지 못한다"
echo "    알려진 한계(방화벽이 없을 때): 호스트에서 모든 인터페이스(0.0.0.0)에 바인드한 서비스는 VM에서 호스트 주소로 닿는다. init의 방화벽이 이 길을 막는다."

echo "==> 5 닻"
state=$(container inspect "$anchor" 2>/dev/null | python3 -c 'import json, sys; print(json.load(sys.stdin)[0]["status"]["state"])' 2>/dev/null)
[ "$state" = running ]; check "닻이 위 시험 내내 떠 있다(비root·capability 0·읽기 전용 루트)"
anchor_fail() { # anchor_fail <설명> [container 옵션…]
  local desc=$1 out code
  shift
  out=$(with_timeout 60 container run --progress none --rm --name "bot-model-verify-run-$pid" --init --no-dns --network "$net" "$@" --entrypoint /usr/local/bin/bot-anchor "$image" 2>&1)
  code=$?
  container delete --force "bot-model-verify-run-$pid" >/dev/null 2>&1
  [ "$code" = 1 ] && [ "$out" = vm_anchor_failed ]; check "$desc (종료 $code)"
}
anchor_fail "닻은 root로는 돌지 않는다" --cap-drop ALL
anchor_fail "닻은 capability가 남아 있으면 돌지 않는다(비root, 기본 capability 집합)" --user 10001:10001
anchor_fail "닻은 capability가 하나라도 남아 있으면 돌지 않는다(비root, 하나만 더함)" --user 10001:10001 --cap-drop ALL --cap-add CAP_NET_BIND_SERVICE

echo "==> 6 가짜 상류와 실제 claude 대화(방화벽 안, claude -p --bare)"
m_reset
M_PROBE=
chat_env=(-e "ANTHROPIC_BASE_URL=http://$gateway:$port" -e ANTHROPIC_API_KEY=fake-session-token-not-a-secret
  -e "HTTPS_PROXY=http://$gateway:$port" -e "HTTP_PROXY=http://$gateway:$port" -e "NO_PROXY=$gateway")
chat_args=(claude -p "Answer briefly." --bare --model fake-model --effort low --tools "" --output-format stream-json --verbose)
sid=$(uuidgen | tr 'A-Z' 'a-z')
: >"$tmp/upstream.log"
chat=$(printf 'first turn' | run_model "bot-model-verify-run-$pid" -i "${chat_env[@]}" -- "${chat_args[@]}" --session-id "$sid" 2>"$tmp/chat.err") && status=0 || status=$?
echo "$chat" | grep -q '"type":"result"' && ! echo "$chat" | grep -q '"is_error":true'; check "대화가 끝나고 결과 이벤트가 있다(종료 코드 $status)"
seen=$(grep '"method"' "$tmp/upstream.log" | grep -v '"path": "/__ready"')
requests=$(echo "$seen" | grep -c '"method"')
others=$(echo "$seen" | grep '"method"' | grep -vc '"path": "/v1/messages')
echo "    상류가 받은 요청 ${requests}건, 그 밖의 경로·다른 호스트 접속 시도 ${others}건"
[ "$requests" = 1 ] && [ "$others" = 0 ]; check "상류는 /v1/messages 한 건만 받았고 다른 호스트로 나가려는 시도가 없다"

echo "==> 6b 대조: 끄는 환경변수를 비우면 바깥으로 나가려는 접속이 기록되는가(점검이 헛돌지 않는지)"
: >"$tmp/upstream.log"
off=()
for var in DISABLE_AUTOUPDATER DISABLE_UPDATES CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC DISABLE_TELEMETRY DISABLE_ERROR_REPORTING DO_NOT_TRACK \
  DISABLE_FEEDBACK_COMMAND CLAUDE_CODE_DISABLE_FEATURE_FLAGS CLAUDE_CODE_DISABLE_TERMINAL_TITLE CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL; do
  off+=(-e "$var=")
done
printf 'first turn' | run_model "bot-model-verify-run-$pid" -i "${chat_env[@]}" "${off[@]}" -- "${chat_args[@]}" --no-session-persistence >/dev/null 2>&1
attempts=$(grep -c '"method": "CONNECT"' "$tmp/upstream.log")
hosts=$(grep '"method": "CONNECT"' "$tmp/upstream.log" | sed -n 's/.*"path": "\([^"]*\)".*/\1/p' | sort -u | tr '\n' ' ')
echo "    변수를 비웠을 때 접속 시도 ${attempts}건: ${hosts:-없음}"
[ "$attempts" -gt 0 ]; check "대조: 끄는 변수를 비우면 CLI가 다른 호스트에 접속하려 한다(이미지 기본값에서는 0건)"

echo "==> 7 0700 작업 폴더 · 세션 기록 · 재개"
record="$tmp/work/claude/projects/-bot/$sid.jsonl"
[ "$(stat -f %Lp "$tmp/work" 2>/dev/null)" = 700 ] && [ -s "$record" ]; check "0700 작업 폴더에 uid 10001이 쓴 세션 기록 /work/claude/projects/-bot/<id>.jsonl이 있고 호스트가 읽는다"
resumed=$(run_model "bot-model-verify-run-$pid" "${chat_env[@]}" -- "${chat_args[@]/#Answer briefly./Continue.}" --resume "$sid" 2>/dev/null) && status=0 || status=$?
echo "$resumed" | grep -q "\"session_id\":\"$sid\"" && echo "$resumed" | grep -q '"type":"result"'; check "새 컨테이너가 같은 세션을 --resume으로 잇는다(종료 코드 $status)"
rm -rf "$tmp/work/claude" "$tmp/work/home" 2>/dev/null
[ ! -e "$tmp/work/claude" ] && [ ! -e "$tmp/work/home" ]; check "호스트가 모델이 만든 기록과 폴더를 지울 수 있다"

echo
echo "통과 $passed · 실패 $failed · 건너뜀 $skipped"
[ "$failed" = 0 ]

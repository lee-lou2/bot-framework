#!/usr/bin/env bash
# Model VM image verification: runs the image with the same flags as the adapter (crates/bot-runtime/src/model/container/args.rs).
# Uses no real model or API key.
#
#   images/model/verify.sh <image-ref>        e.g., name@sha256:... printed by build.sh
#
# Checks:
#   1. `claude --version` runs via bot-init on a read-only rootfs and matches the image label; image bot-init matches repo.
#   2. bot-init fail-closed rejection (missing/invalid relay env, missing cap, non-root, unrecognized arg):
#      non-zero exit, stderr is `vm_init_failed`, model probe does not start.
#   3. Firewall (nftables) and privilege drop: model runs as 10001:10001, no groups/caps, no_new_privs;
#      only the relay is reachable (other gateway port, IPv4/IPv6 neighbor, host IPv6/LAN, internet, inbound, and tamper attempts blocked).
#      A no-firewall control run confirms those paths are otherwise reachable.
#   4. Read-only rootfs, writable /tmp (tmpfs) and /work, disable env vars present, BOT_RELAY_* unset.
#   5. Anchor (`bot-anchor`): stays running as non-root with zero caps; fails with `vm_anchor_failed` if root or granted any cap.
#   6. `claude -p --bare` against local fake upstream receives only `/v1/messages` with zero outbound CONNECT attempts
#      (control run with disable vars cleared confirms CONNECT attempts are caught).
#   7. Session persistence in 0700 /work, `--resume` in a fresh container, and host read/delete.
# Creates a temporary host-only network (override subnet via `BOT_MODEL_VERIFY_SUBNET`), helper containers, temp dir,
# and local listeners, cleaning up on exit. Serializes via `/tmp/bot-vm-canary.lock`. Exits 1 if any check fails.
# (`set -e` is intentionally omitted so failed checks are recorded and summarized.)
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
image=${1:-}
case "$image" in '' | -h | --help) sed -n '2,/^set -uo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2 ;; esac
for tool in container python3 curl uuidgen; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required" >&2; exit 2; }
done

# Serialize real VM runs via `/tmp/bot-vm-canary.lock` (shared with adapter VM tests) to avoid flaky concurrent runs.
if [ -z "${BOT_VM_LOCKED:-}" ]; then
  BOT_VM_LOCKED=1 exec python3 -I -c 'import fcntl, os, sys
fd = os.open("/tmp/bot-vm-canary.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o644)
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    print("waiting for another VM task to finish (/tmp/bot-vm-canary.lock)", file=sys.stderr)
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
die() { echo "error: $*" >&2; exit 1; }

check() {
  local code=$?
  if [ "$code" = 0 ]; then passed=$((passed + 1)); echo "  PASS  $1"; else failed=$((failed + 1)); echo "  FAIL  $1"; fi
}
skip() { skipped=$((skipped + 1)); echo "  SKIP  $1"; }
has() { printf '%s\n' "$1" | grep -qxF -- "$2"; }

# Guard against hangs (returns 142 on timeout) if init accidentally enters an idle path.
with_timeout() {
  local limit=$1
  shift
  if command -v perl >/dev/null 2>&1; then perl -e 'alarm shift; exec @ARGV or die' "$limit" "$@"; else "$@"; fi
}

# Run the model VM with adapter-equivalent flags: run_model <name> [extra container flags...] -- <init args...>
# Adjust M_* vars before calling to test missing caps or bad relay env; M_PROBE=1 mounts /dbg tools and the probe binary.
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
ip4_of() {
  container inspect "$1" | python3 -c 'import json, sys; print(json.load(sys.stdin)[0]["status"]["networks"][0]["ipv4Address"].split("/")[0])'
}
# Use link-local IPv6 (fe80::...%eth0), which is present immediately without router advertisements.
ll6_of() { container exec "$1" /dbg/busybox ip -6 addr show dev eth0 2>/dev/null | sed -n 's/.*inet6 \(fe80[0-9a-f:]*\)\/.*/\1/p' | head -1; }

# Pick an unused /24 subnet to avoid colliding with existing or dormant container networks.
pick_subnet() {
  local used n
  used=$(container network list 2>/dev/null | awk 'NR > 1 { print $2 }')
  for n in $(seq 200 239); do
    case "$used" in *"192.168.$n.0/24"*) ;; *) echo "192.168.$n.0/24"; return 0 ;; esac
  done
  return 1
}

echo "==> Setup: host-only network, probe tools, anchor, neighbor VM, fake upstream"
mkdir -p "$tmp/tools" "$tmp/in" "$tmp/work" "$tmp/bin"
chmod 700 "$tmp/in" "$tmp/work"
subnet=${BOT_MODEL_VERIFY_SUBNET:-$(pick_subnet)}
[ -n "$subnet" ] || die "no unused subnet in 192.168.200..239.0/24: set BOT_MODEL_VERIFY_SUBNET"
container network create --internal --label bot.managed=verify --subnet "$subnet" "$net" >/dev/null || die "failed to create host-only network (subnet $subnet)"
# Run the helper on the same host-only network; running on the default NAT network right before can leave subsequent host-only VMs with NO-CARRIER.
container run --progress none --rm --name "bot-model-verify-tools-$pid" --network "$net" --mount "type=bind,source=$tmp/tools,target=/out" "$builder" \
  /bin/sh -c 'cp /bin/busybox /out/busybox && cd /out && for a in $(/bin/busybox --list); do ln -s busybox "$a"; done' >/dev/null 2>&1 \
  || die "failed to prepare probe tools"
# Start anchor first so the host-only network gateway address comes up before binding listeners.
container run --progress none -d --rm --name "$anchor" --network "$net" --user 10001:10001 --cap-drop ALL --read-only --tmpfs /tmp --init --no-dns \
  --entrypoint /usr/local/bin/bot-anchor "$image" >/dev/null || die "failed to start anchor (check ref with: container image inspect)"
# Neighbor VM on the same network listening on IPv4 9000 and IPv6 9001.
container run --progress none -d --rm --name "$neighbor" --network "$net" --init --no-dns --read-only --tmpfs /tmp --cap-drop ALL \
  --mount "type=bind,source=$tmp/tools,target=/dbg,readonly" --entrypoint /dbg/busybox "$image" \
  sh -c '/dbg/busybox nc -lk -p 9000 -e /dbg/true & exec /dbg/busybox nc -lk -s :: -p 9001 -e /dbg/true' >/dev/null || die "failed to start neighbor VM"
sleep 2
gateway=$(container network inspect "$net" | sed -n 's/.*"ipv4Gateway" *: *"\([0-9.]*\)".*/\1/p' | head -1)
[ -n "$gateway" ] || die "failed to read host-only network gateway address"
neighbor4=$(ip4_of "$neighbor")
neighbor6=$(ll6_of "$neighbor")
[ -n "$neighbor4" ] && [ -n "$neighbor6" ] || die "failed to read neighbor VM addresses"
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
# Optional host-side controls: link-local IPv6 listener on the bridge and an open port on the host LAN IP.
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
echo "    network $net ($subnet) | gateway $gateway | relay port $port | other port $other | neighbor $neighbor4 / $neighbor6"
h6txt=none
[ -n "$host6" ] && h6txt="$host6%$bridge:$host6port"
lantxt=none
[ -n "$hostlan" ] && lantxt="$hostlan:$hostlanport"
echo "    host IPv6 listener $h6txt | host LAN open port $lantxt"

# Extract image bot-init alongside a probe script replacing /usr/local/bin/claude for bind-mounting over /usr/local/bin.
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
# Wait for IPv6 DAD so a tentative address is not mistaken for a firewall drop.
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

echo "==> 1. Version and bot-init"
m_reset
M_PROBE=
version=$(run_model "bot-model-verify-run-$pid" -- claude --version 2>"$tmp/err")
label=$(container image inspect "$image" | sed -n 's/.*"bot.claude-code.version" *: *"\([^"]*\)".*/\1/p' | head -1)
echo "    output: $version | image label: ${label:-?}"
[ -n "$label" ] && [ "$version" = "$label (Claude Code)" ] && [ ! -s "$tmp/err" ]; check "claude --version runs via bot-init on read-only root and matches label"
cmp -s "$tmp/bin/bot-init" "$here/bot-init"; check "image bot-init matches images/model/bot-init"

echo "==> 2. bot-init fail-closed rejection (model must not start)"
expect_init_fail() {
  local desc=$1 code
  shift
  rm -f "$tmp/work/started"
  run_model "bot-model-verify-run-$pid" -e P_MODE=marker "$@" >"$tmp/rej.out" 2>"$tmp/rej.err"
  code=$?
  [ "$code" != 0 ] && [ "$(cat "$tmp/rej.err")" = vm_init_failed ] && [ ! -s "$tmp/rej.out" ] && [ ! -e "$tmp/work/started" ]; check "$desc (exit $code)"
}
m_reset; run_model "bot-model-verify-run-$pid" -e P_MODE=marker -- claude >/dev/null 2>&1
[ -e "$tmp/work/started" ]; check "control: valid inputs allow bot-init to exec probe"
m_reset; M_RIP=; M_RPORT=; expect_init_fail "missing BOT_RELAY_IP and BOT_RELAY_PORT" -- claude
m_reset; M_RPORT=; expect_init_fail "missing BOT_RELAY_PORT" -- claude
m_reset; M_RIP=; expect_init_fail "missing BOT_RELAY_IP" -- claude
for bad in 8.8.8.8 127.0.0.1 169.254.1.1 172.32.0.1 192.168.01.1 10.1.2 10.1.2.256 '10.1.2.3;x' '::1'; do
  m_reset; M_RIP=$bad; expect_init_fail "non-private or invalid relay IP: $bad" -- claude
done
for bad in 0 65536 080 '80;x' 1e3 0x50; do
  m_reset; M_RPORT=$bad; expect_init_fail "invalid relay port: $bad" -- claude
done
for missing in CAP_NET_ADMIN CAP_SETUID CAP_SETGID CAP_SETPCAP; do
  m_reset; M_CAPS=$(echo "$default_caps" | sed "s/$missing//"); expect_init_fail "missing capability $missing" -- claude
done
m_reset; M_CAPS=; expect_init_fail "no capabilities" -- claude
m_reset; M_USER=10001:10001; expect_init_fail "started as non-root" -- claude
m_reset; expect_init_fail "first arg is unrecognized (sh)" -- sh -c 'touch /work/started'
m_reset; expect_init_fail "first arg is full path to claude" -- /usr/local/bin/claude

echo "==> 3 & 4. Firewall, privilege drop, tamper resistance, mounts, env"
inbound() {
  local out4 out6
  out4=$(container exec "$neighbor" /dbg/busybox sh -c "if /dbg/busybox nc -w 3 $1 9100 </dev/null >/dev/null 2>&1; then echo open; else echo blocked; fi" 2>&1 | tail -1)
  out6=$(container exec "$neighbor" /dbg/busybox sh -c "if /dbg/busybox nc -w 3 $2%eth0 9101 </dev/null >/dev/null 2>&1; then echo open; else echo blocked; fi" 2>&1 | tail -1)
  echo "inbound-v4 $out4"
  echo "inbound-v6 $out6"
}
probe_run() {
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
has "$fw" "status Uid: 10001 10001 10001 10001" && has "$fw" "status Gid: 10001 10001 10001 10001"; check "model real/effective/saved/fs uid and gid are all 10001"
echo "$fw" | grep -q '^status Groups: *$'; check "no supplementary groups"
[ "$(echo "$fw" | grep -cE '^status Cap(Inh|Prm|Eff|Bnd|Amb): 0000000000000000$')" = 5 ]; check "all capabilities (Inh/Prm/Eff/Bnd/Amb) are zero"
has "$fw" "status NoNewPrivs: 1"; check "no_new_privs is enabled"
has "$fw" "open relay" && has "$fw" "open relay-after"; check "relay (gateway:port) is reachable before and after tamper attempts"
has "$fw" "blocked other-port"; check "other port on gateway is blocked"
has "$fw" "blocked neighbor-v4" && has "$fw" "blocked neighbor-v6"; check "neighbor VM is blocked over IPv4 and IPv6"
has "$fw" "blocked internet-443" && has "$fw" "blocked internet-53"; check "internet (1.1.1.1:443, 8.8.8.8:53) is blocked"
if [ -n "$host6" ]; then has "$fw" "blocked host-v6"; check "host IPv6 service is blocked"; else skip "host IPv6 service (could not determine bridge IPv6 address)"; fi
if [ -n "$hostlan" ]; then has "$fw" "blocked host-lan"; check "host LAN wildcard service (port $hostlanport) is blocked"; else skip "host LAN service (port 22 not open on host)"; fi
has "$fw" "blocked newns-relay"; check "creating a new network namespace does not bypass firewall"
[ "$(echo "$fw" | grep -c '^tamper .* denied$')" = 10 ] && ! echo "$fw" | grep -q 'ALLOWED'; check "all 10 tamper attempts (rules, addr, route, privs) are denied"
has "$fw" "inbound-v4 blocked" && has "$fw" "inbound-v6 blocked"; check "inbound traffic from neighbor VM (IPv4 and IPv6) is blocked"
echo "$fw" | grep -q '^mount root ext4 ro,'; check "root filesystem is mounted read-only"
[ "$(echo "$fw" | grep -cE '^write (root|etc|usr|bin|lib|bot) denied$')" = 6 ] && has "$fw" "write tmp ok" && has "$fw" "write work ok"; check "writes to /, /etc, /usr, /bin, /lib, /bot are denied; /tmp and /work are writable"
envs_ok=1
for v in DISABLE_AUTOUPDATER DISABLE_UPDATES CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC DISABLE_TELEMETRY DISABLE_ERROR_REPORTING DO_NOT_TRACK DISABLE_FEEDBACK_COMMAND \
  CLAUDE_CODE_DISABLE_FEATURE_FLAGS CLAUDE_CODE_DISABLE_TERMINAL_TITLE CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL CLAUDE_CODE_DISABLE_AUTO_MEMORY CLAUDE_CODE_DISABLE_BUNDLED_SKILLS; do
  has "$fw" "env $v=1" || envs_ok=
done
[ -n "$envs_ok" ] && has "$fw" "env HOME=/work/home" && has "$fw" "env CLAUDE_CONFIG_DIR=/work/claude"; check "12 disable env vars plus HOME and CLAUDE_CONFIG_DIR are set"
has "$fw" "env BOT_RELAY_IP=<unset>" && has "$fw" "env BOT_RELAY_PORT=<unset>"; check "BOT_RELAY_IP and BOT_RELAY_PORT are unset before execing model"

echo "==> 3 control: without firewall (busybox entrypoint, root) — confirms paths are otherwise reachable"
: >"$tmp/inbound.out"
m_reset
M_CAPS=
probe_run "bot-model-verify-run-$pid" "$tmp/ctl.out" "${probe_env[@]}" --entrypoint /dbg/busybox -- sh /usr/local/bin/claude
ctl=$(cat "$tmp/ctl.out" "$tmp/inbound.out")
echo "$ctl" | grep -E '^(open|blocked) ' | grep -v -E 'newns|relay-after' | sed 's/^/    /'
has "$ctl" "open relay" && has "$ctl" "open other-port" && has "$ctl" "open neighbor-v4" && has "$ctl" "open neighbor-v6"; check "control: without firewall, relay, other port, and neighbor VM (IPv4/IPv6) are open"
[ -n "$host6" ] && { has "$ctl" "open host-v6"; check "control: without firewall, host IPv6 service is open"; }
[ -n "$hostlan" ] && { has "$ctl" "open host-lan"; check "control: without firewall, host LAN service is open"; }
has "$ctl" "inbound-v4 open" && has "$ctl" "inbound-v6 open"; check "control: without firewall, neighbor can connect inbound"
has "$ctl" "blocked internet-443" && has "$ctl" "blocked internet-53"; check "host-only (--internal) network alone blocks internet and DNS"
echo "    Note (without firewall): host services bound to 0.0.0.0 are reachable from the VM; bot-init nftables blocks them."

echo "==> 5. Anchor (bot-anchor)"
state=$(container inspect "$anchor" 2>/dev/null | python3 -c 'import json, sys; print(json.load(sys.stdin)[0]["status"]["state"])' 2>/dev/null)
[ "$state" = running ]; check "anchor stays running throughout tests (non-root, zero caps, read-only root)"
anchor_fail() {
  local desc=$1 out code
  shift
  out=$(with_timeout 60 container run --progress none --rm --name "bot-model-verify-run-$pid" --init --no-dns --network "$net" "$@" --entrypoint /usr/local/bin/bot-anchor "$image" 2>&1)
  code=$?
  container delete --force "bot-model-verify-run-$pid" >/dev/null 2>&1
  [ "$code" = 1 ] && [ "$out" = vm_anchor_failed ]; check "$desc (exit $code)"
}
anchor_fail "anchor refuses to run as root" --cap-drop ALL
anchor_fail "anchor refuses default capabilities (non-root)" --user 10001:10001
anchor_fail "anchor refuses any single capability (non-root)" --user 10001:10001 --cap-drop ALL --cap-add CAP_NET_BIND_SERVICE

echo "==> 6. Real claude chat against fake upstream (inside firewall, claude -p --bare)"
m_reset
M_PROBE=
chat_env=(-e "ANTHROPIC_BASE_URL=http://$gateway:$port" -e ANTHROPIC_API_KEY=fake-session-token-not-a-secret
  -e "HTTPS_PROXY=http://$gateway:$port" -e "HTTP_PROXY=http://$gateway:$port" -e "NO_PROXY=$gateway")
chat_args=(claude -p "Answer briefly." --bare --model fake-model --effort low --tools "" --output-format stream-json --verbose)
sid=$(uuidgen | tr 'A-Z' 'a-z')
: >"$tmp/upstream.log"
chat=$(printf 'first turn' | run_model "bot-model-verify-run-$pid" -i "${chat_env[@]}" -- "${chat_args[@]}" --session-id "$sid" 2>"$tmp/chat.err") && status=0 || status=$?
echo "$chat" | grep -q '"type":"result"' && ! echo "$chat" | grep -q '"is_error":true'; check "chat completes with result event (exit $status)"
seen=$(grep '"method"' "$tmp/upstream.log" | grep -v '"path": "/__ready"')
requests=$(echo "$seen" | grep -c '"method"')
others=$(echo "$seen" | grep '"method"' | grep -vc '"path": "/v1/messages')
echo "    upstream requests: $requests, other paths/hosts: $others"
[ "$requests" = 1 ] && [ "$others" = 0 ]; check "upstream received only /v1/messages with zero outbound attempts to other hosts"

echo "==> 6b control: clearing disable env vars triggers detectable outbound CONNECT attempts"
: >"$tmp/upstream.log"
off=()
for var in DISABLE_AUTOUPDATER DISABLE_UPDATES CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC DISABLE_TELEMETRY DISABLE_ERROR_REPORTING DO_NOT_TRACK \
  DISABLE_FEEDBACK_COMMAND CLAUDE_CODE_DISABLE_FEATURE_FLAGS CLAUDE_CODE_DISABLE_TERMINAL_TITLE CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL; do
  off+=(-e "$var=")
done
printf 'first turn' | run_model "bot-model-verify-run-$pid" -i "${chat_env[@]}" "${off[@]}" -- "${chat_args[@]}" --no-session-persistence >/dev/null 2>&1
attempts=$(grep -c '"method": "CONNECT"' "$tmp/upstream.log")
hosts=$(grep '"method": "CONNECT"' "$tmp/upstream.log" | sed -n 's/.*"path": "\([^"]*\)".*/\1/p' | sort -u | tr '\n' ' ')
echo "    CONNECT attempts with disable vars cleared: $attempts (${hosts:-none})"
[ "$attempts" -gt 0 ]; check "control: clearing disable vars causes CLI to attempt outbound CONNECT (0 with image defaults)"

echo "==> 7. 0700 workdir, session persistence, and resume"
record="$tmp/work/claude/projects/-bot/$sid.jsonl"
[ "$(stat -f %Lp "$tmp/work" 2>/dev/null)" = 700 ] && [ -s "$record" ]; check "uid 10001 writes session log /work/claude/projects/-bot/<id>.jsonl in 0700 workdir and host can read it"
resumed=$(run_model "bot-model-verify-run-$pid" "${chat_env[@]}" -- "${chat_args[@]/#Answer briefly./Continue.}" --resume "$sid" 2>/dev/null) && status=0 || status=$?
echo "$resumed" | grep -q "\"session_id\":\"$sid\"" && echo "$resumed" | grep -q '"type":"result"'; check "fresh container resumes session with --resume (exit $status)"
rm -rf "$tmp/work/claude" "$tmp/work/home" 2>/dev/null
[ ! -e "$tmp/work/claude" ] && [ ! -e "$tmp/work/home" ]; check "host can delete session logs and directories created by model"

echo
echo "Passed: $passed | Failed: $failed | Skipped: $skipped"
[ "$failed" = 0 ]

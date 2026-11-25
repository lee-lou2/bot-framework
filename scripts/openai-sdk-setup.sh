#!/usr/bin/env bash
# 공식 openai SDK(Python 2.29.0·Node 6.29.0) 종단 시험 환경을 만든다. 저장소에는 아무것도 설치하지 않는다 — 빌드 폴더 밑 `openai-sdk/`에만 둔다.
#   venv      requirements.lock.txt를 `pip install --require-hashes --no-deps`로(해시·정확 판본, 휠만)
#   node      package-lock.json을 `npm ci --ignore-scripts`로(integrity, 설치 스크립트 없음)
# 설치 뒤 공식 OpenAPI 스키마가 pin.json의 커밋·sha256과 맞는지, SDK 판본이 고정과 같은지 확인하고, 하나라도 다르면 실패한다.
# 사용: scripts/openai-sdk-setup.sh [설치 폴더]   기본: ${CARGO_TARGET_DIR:-<저장소>/target}/openai-sdk   (시험은 BOT_OPENAI_SDK_DIR로 같은 폴더를 가리킨다)
# 잠금 파일이 그대로고 설치된 판본이 맞으면 다시 설치하지 않는다(오프라인에서도 확인만 한다).
# 필요: CPython 3.12(python3.12 또는 python3)·Node 22 이상·npm. 내려받은 파일은 모두 해시·integrity로 확인한다(캐시가 있으면 오프라인에서도 된다).
# SDK 시험 의존성은 해시 고정이며 저장소에 싣지 않는다(MPL-2.0 패키지 certifi·tqdm이 시험 환경에만 들어간다).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/crates/bot-cli/tests/openai_sdk"
dest="${1:-${CARGO_TARGET_DIR:-$root/target}/openai-sdk}"
case "$dest" in /*) ;; *) echo "설치 폴더는 절대 경로여야 한다: $dest" >&2; exit 2;; esac

python=""
for candidate in python3.12 python3; do
  if command -v "$candidate" >/dev/null 2>&1 && [ "$("$candidate" -c 'import sys; print("%d.%d" % sys.version_info[:2])')" = 3.12 ]; then python="$(command -v "$candidate")"; break; fi
done
[ -n "$python" ] || { echo "CPython 3.12가 필요하다(잠금 파일이 3.12 휠만 허용한다)." >&2; exit 1; }
for tool in node npm; do command -v "$tool" >/dev/null 2>&1 || { echo "$tool 이(가) 필요하다." >&2; exit 1; }; done
[ "$(node -p 'process.versions.node.split(".")[0]')" -ge 22 ] || { echo "Node 22 이상이 필요하다." >&2; exit 1; }

mkdir -p "$dest"
# 이미 맞게 설치돼 있으면(잠금 파일의 sha256 표지가 같고 설치된 판본이 고정과 같으면) 다시 내려받지 않는다: 오프라인에서도 돌 수 있다.
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }
stamp="$(sha256 <"$src/requirements.lock.txt")"
node_stamp="$(cat "$src/node/package.json" "$src/node/package-lock.json" | sha256)"
if [ ! -x "$dest/venv/bin/python" ] || [ "$(cat "$dest/venv.stamp" 2>/dev/null)" != "$stamp" ]; then
  rm -rf "$dest/venv"
  "$python" -m venv "$dest/venv"
  "$dest/venv/bin/python" -m pip install --quiet --disable-pip-version-check --require-hashes --no-deps --only-binary=:all: -r "$src/requirements.lock.txt"
  "$dest/venv/bin/python" -m pip check --disable-pip-version-check
  echo "$stamp" >"$dest/venv.stamp"
fi

mkdir -p "$dest/node"
cp "$src/node/package.json" "$src/node/package-lock.json" "$dest/node/"
if [ ! -f "$dest/node/node_modules/openai/package.json" ] || [ "$(cat "$dest/node.stamp" 2>/dev/null)" != "$node_stamp" ]; then
  (cd "$dest/node" && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error)
  echo "$node_stamp" >"$dest/node.stamp"
fi
cp "$src"/node/*.mjs "$dest/node/"

# 고정 확인: SDK 판본·공식 스키마의 커밋/sha256(오프라인에서도 한다). BOT_OPENAI_SPEC에 원본 openapi.yaml을 주면 저장소의 스키마 조각이 그 원본에서 나온 것인지도 본다.
"$dest/venv/bin/python" -I -c 'import importlib.metadata as m; assert m.version("openai") == "2.29.0", m.version("openai")'
[ "$(node -p "require('$dest/node/node_modules/openai/package.json').version")" = 6.29.0 ] || { echo "Node SDK 판본이 6.29.0이 아니다" >&2; exit 1; }
"$dest/venv/bin/python" -I -c "import sys; sys.path.insert(0, '$src/py'); from conformance import Schema; s = Schema(); print('공식 스키마', s.source['openapi'], s.source['commit'][:12], 'sha256', s.source['spec_sha256'][:12])"
if [ -n "${BOT_OPENAI_SPEC:-}" ]; then "$dest/venv/bin/python" -I "$src/schema/extract_subset.py" "$BOT_OPENAI_SPEC" --check && echo "스키마 조각이 고정 원본에서 그대로 나왔다"; fi
echo "준비됨: $dest   (BOT_OPENAI_SDK_DIR=$dest)"

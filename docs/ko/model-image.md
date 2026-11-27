# 모델 VM 이미지

[English](../model-image.md) | 한국어

VM 호스팅 모델 CLI(`claude-cli`, `codex-cli`, `agy-cli`)는 운영자가 로컬에서 빌드하고 다이제스트로 고정한 Apple container VM 안에서 실행됩니다. 이 저장소는 이미지 정의(`images/model/`)와 빌드·검증 스크립트만 제공하며 사전 빌드 이미지는 배포하지 않습니다. 각 CLI는 전용 Containerfile(`claude`용 `images/model/Containerfile`, `codex`용 `images/model/Containerfile.codex`, `agy`용 `images/model/Containerfile.agy`)을 사용합니다.

- **공급망:** 베이스 Alpine 이미지는 다이제스트로 고정되고, Claude Code(`images/model/Containerfile`)는 고정된 Anthropic 릴리스 서명 키 핑거프린트 GPG 검증과 정확한 버전(`2.1.331`)·SHA-256으로 고정됩니다. `Containerfile.codex`와 `Containerfile.agy`도 운영자가 고정한 Linux arm64 릴리스를 사용해 동일한 다이제스트 고정 빌드 계약을 따릅니다.
- **이미지 구성:** 다이제스트 고정 Alpine + `nftables` + `setpriv`로 만든 단일 평탄 레이어(`FROM scratch`)이며 `apk`와 사용하지 않는 모든 BusyBox 심볼릭 링크를 제거했습니다(`/bin/sh`와 `/bin/sleep`만 유지).
- **런타임 자세:** 읽기 전용 루트 파일시스템에서 비루트(`10001:10001`)로 실행되며 공식 환경 변수로 자동 업데이트, 텔레메트리, 오류 보고 및 기타 비필수 트래픽을 비활성화합니다. 신뢰된 `bot-init`이 root로 시작해 릴레이 전용 방화벽을 적용하고 모든 권한을 내려놓은 뒤 선택한 CLI를 `exec`합니다.

```sh
ref=$(images/model/build.sh --cli claude)   # `--cli claude|codex|agy` (기본값 `claude`); `name@sha256:...` 출력
images/model/verify.sh "$ref"               # 가짜 업스트림으로 실제 `claude` 이미지 검증(실제 모델·API 키 불필요)
```

`--cli codex`에는 `--build-arg CODEX_VERSION=... --build-arg CODEX_SHA256=...`가 필요하며, `--cli agy`에는 `--build-arg AGY_VERSION=... --build-arg AGY_URL=... --build-arg AGY_SHA256=...`(glibc 링크 arm64 바이너리용 선택적 `--build-arg AGY_GLIBC_COMPAT=1`)가 필요합니다. 필수 인자가 누락되면 빌드가 fail-closed로 중단됩니다. `images/model/verify.sh`는 현재 `claude` 이미지만 검증합니다.

`host.json`의 `sandbox.apple.image`(`claude-cli` 전용 호스트) 또는 `sandbox.apple.images`(`{"claude": "...", "codex": "...", "agy": "..."}`)에 `build.sh`가 출력한 다이제스트 참조를 설정하세요. `container build`는 가변 `name:tag` 참조만 생성하므로 `build.sh`가 `127.0.0.1`에 바인딩된 다이제스트 고정 CNCF Distribution 레지스트리를 임시로 띄워 이미지를 푸시한 뒤 **다이제스트로 다시 풀**해 불변 로컬 참조를 저장하고 종료 시 레지스트리를 삭제합니다(런타임에는 불필요). `container image prune -a`는 사용하지 않는 다이제스트 참조를 삭제하므로 실행하지 마세요.

## 어댑터 계약

| 항목 | 계약 |
|---|---|
| 엔트리포인트 | `ENTRYPOINT`는 `/usr/local/bin/bot-init`이고 첫 인자는 `claude`, `codex`, `agy` 중 하나여야 하며 `/usr/local/bin/<cli>`로 고정 매핑됩니다. CLI 인자로 바이너리 경로, 사용자, 릴레이 주소를 절대 받지 않습니다. |
| Capabilities | `--cap-drop ALL`에 `CAP_NET_ADMIN`(`nft`), `CAP_SETUID`, `CAP_SETGID`, `CAP_SETPCAP`(바운딩 세트 초기화)만 추가합니다. 하나라도 빠지면 fail-closed로 종료합니다. |
| 호스트 전용 환경 변수 | 어댑터가 바인딩된 리스너에서 `BOT_RELAY_IP`(RFC 1918 IPv4)와 `BOT_RELAY_PORT`(`1..65535`)를 설정하고 CLI별 릴레이 URL(`ANTHROPIC_BASE_URL`, `GOOGLE_GEMINI_BASE_URL`, 또는 검증된 Codex `-c` URL)을 전달합니다. `bot-init`은 둘을 재검증하고 CLI 실행 전에 해제합니다. |
| 초기화 순서 | 릴레이 TCP와 established 응답만 허용하는 `nftables` 규칙을 적용하고 다시 읽어 검증한 뒤, 별도 검사로 UID/GID `10001`, 보조 그룹 없음, capability 0개, `no_new_privs`를 확인하고 권한을 강등해 CLI를 `exec`합니다(`agy`는 `exec` 전 UID `10001` 권한으로 `/work/mcp.agy.template.json`을 RAM 전용 `/tmp/mcp_config.json`(`0600`)으로 확장). |
| 실패 처리 | 초기화 실패 시 stderr에 한 단어 `vm_init_failed`만 출력하고 종료 코드 `1`로 끝나며 모델 CLI를 시작하지 않습니다. CLI가 시작된 뒤에는 종료 코드를 그대로 전달합니다. |
| 앵커 | `--entrypoint /usr/local/bin/bot-anchor --user 10001:10001 --cap-drop ALL`: 비루트이고 capability가 0개일 때만 유휴 대기하며, 아니면 `vm_anchor_failed`와 종료 코드 `1`로 종료합니다. `bot-init`과 분리되어 있습니다. |
| 마운트와 경로 | 읽기 전용 루트 파일시스템, tmpfs `/tmp`, 읽기 전용 `/bot`(작업 디렉터리), 실행별 쓰기 가능 `/work`(호스트가 준비·정리, init은 `chown`하지 않음). 세션 토큰은 환경 변수(`RELAY_TOKEN`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`)로만 전달되며 호스트 디스크에는 절대 기록되지 않습니다(`/work/mcp.claude.json`과 `/work/mcp.agy.template.json`에는 `${RELAY_TOKEN}` 플레이스홀더만 포함, `codex`는 `RELAY_TOKEN`을 직접 읽음). |
| 네트워크 | 호스트 전용(`--internal`), `--no-dns`. 호스트의 `http://` 릴레이만 도달 가능합니다. |

## `verify.sh` 검증 항목

`images/model/verify.sh`는 어댑터와 동등한 플래그로 실제 이미지를 실행하는 동시에, 차단 경로가 원래 열려 있음을 확인하는 무방화벽 대조 실행을 병행합니다.

- **버전과 초기화 무결성:** 읽기 전용 루트 파일시스템에서 `bot-init`을 통해 `claude --version`이 실행되고 이미지 라벨과 일치하며, 이미지 내 `bot-init`이 `images/model/bot-init`과 일치합니다.
- **Fail-closed 초기화 거부:** 릴레이 환경 변수 누락/오류, capability 1개 이상 누락, 비루트 시작, 허용되지 않은 첫 인자(`claude`, `codex`, `agy` 이외)는 모두 정확한 stderr `vm_init_failed`와 0이 아닌 코드로 종료하며 모델을 시작하지 않습니다.
- **권한 강등과 방화벽:** 모델이 UID/GID `10001`, 보조 그룹 없음, 모든 capability 세트 0, `no_new_privs=1`로 실행됩니다. 릴레이는 도달 가능하고 다른 게이트웨이 포트, IPv4/IPv6 형제 VM, 호스트 IPv6/LAN 포트, 인터넷(`1.1.1.1:443`, `8.8.8.8:53`), 인바운드 피어 트래픽, 새 네트워크 네임스페이스 및 10가지 변조 시도(규칙, 주소, 라우트, 권한)는 모두 차단됩니다.
- **가짜 업스트림 대화:** 실제 `claude -p --bare`가 로컬 가짜 업스트림을 대상으로 `/v1/messages` 요청 1건, 다른 호스트 시도 0건으로 완료됩니다. 비활성화 환경 변수를 지운 대조 실행에서는 외부 `CONNECT` 시도(예: `api.anthropic.com:443`)가 기록됩니다.
- **세션 영속성과 앵커:** UID `10001`이 `0700` `/work` 안에 `/work/claude/projects/-bot/<id>.jsonl`을 쓰고, 새 컨테이너가 `--resume`으로 재개하며, 호스트가 파일을 읽고 삭제합니다. `bot-anchor`는 비루트·무권한으로 유휴 유지되고 root이거나 capability가 하나라도 있으면 `vm_anchor_failed`로 종료합니다.

`build.sh`와 `verify.sh` 모두 호스트에서 실제 VM 실행을 직렬화하기 위해 `/tmp/bot-vm-canary.lock`을 획득합니다. `verify.sh`는 사용하지 않는 `192.168.200..239.0/24` 서브넷에 임시 호스트 전용 네트워크를 생성하고(`BOT_MODEL_VERIFY_SUBNET`으로 재정의 가능) 그 위에서 보조 VM을 실행해 휴면 컨테이너 네트워크와의 충돌을 피합니다.

## 버전 업데이트

1. 새 릴리스의 체크섬을 검증합니다(Claude는 서명된 `manifest.json`의 `platforms.linux-arm64-musl.checksum`, Codex/Agy는 운영자가 고정한 arm64 릴리스 체크섬).
2. `images/model/Containerfile`에서 `CLAUDE_VERSION`과 `CLAUDE_SHA256`을 수정합니다(또는 `images/model/Containerfile.codex`/`images/model/Containerfile.agy`에 새 고정 `--build-arg` 값을 전달하며, 베이스 이미지 업데이트 시 `BUILDER` 태그와 다이제스트를 모두 수정).
3. `images/model/build.sh --cli <cli>`(`claude`는 `images/model/verify.sh` 포함)를 다시 실행한 뒤 새 다이제스트 참조를 `sandbox.apple.image` 또는 `sandbox.apple.images.<cli>`에 반영합니다.

`nftables`와 `setpriv`는 고정된 Alpine 브랜치에서 설치되며 버전이 `/etc/bot-model-image`에 기록됩니다. Alpine 패키지 업데이트 이후 같은 `Containerfile`을 다시 빌드하면 다른 다이제스트가 나오므로 항상 `build.sh`가 출력한 정확한 다이제스트를 배포하세요.

## 참고 사항

- **v2.0 범위:** 이미지 정의와 빌드/검증 스크립트만 공개하며 운영자가 로컬에서 빌드합니다(사전 빌드 배포는 추후 과제).
- **라이선스:** `deny.toml`은 Rust 의존성과 저장소에 커밋된 파일만 다룹니다. 빌드된 이미지에 설치된 패키지 버전과 라이선스(BusyBox, nftables, setpriv)는 이미지 내부 `/etc/bot-model-image`를 확인하세요.
- **심층 방어:** `--internal`만으로는 `0.0.0.0`에 바인딩된 호스트 서비스와 같은 네트워크의 형제 VM에 도달할 수 있습니다. VM 내부에서는 `bot-init`의 `nftables`가 둘 다 차단하고, 호스트는 릴레이를 게이트웨이 IP에만 바인딩합니다.
- **플랫폼:** Apple Silicon 전용(`linux-arm64-musl`; glibc 링크 `agy` 바이너리는 `AGY_GLIBC_COMPAT=1`로 선택적 `gcompat`/`libstdc++` 지원). Apple container는 `nftables`(`CONFIG_NF_TABLES`)가 내장된 Linux 커널을 제공합니다.
- **캐시 정리:** 빌더 캐시와 베이스 이미지는 `container builder stop`과 `container image prune`으로 정리하세요(`-a`는 절대 사용 금지).

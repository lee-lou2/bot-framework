# 모델 VM 이미지

모델 CLI(Claude Code)는 Apple container VM 안에서 돌고, 그 VM의 이미지는 운영자가 자기 컴퓨터에서 빌드해 다이제스트로 고정해 씁니다. 저장소에는 정의(`images/model/`)와 빌드·점검 스크립트만 있고 이미지는 배포하지 않습니다. Claude Code는 독점 라이선스라 저장소에 넣거나 재배포하지 않고, 빌드하는 컴퓨터가 Anthropic의 공식 릴리스 저장소에서 받아 설치만 합니다. 기반 이미지는 다이제스트로, Claude Code는 정확한 판본과 SHA-256으로 고정하며, 받은 매니페스트의 GPG 서명(Anthropic 릴리스 서명 키, 지문 고정)·매니페스트의 체크섬·`Containerfile`에 고정한 체크섬·받은 파일의 체크섬이 모두 같아야 설치합니다(하나라도 다르면 빌드가 멈춥니다). 이미지는 다이제스트로 고정한 Alpine 위에 nftables·setpriv를 얹고 쓰지 않는 것(패키지 관리자·busybox 명령)을 지운 한 층짜리입니다. 모델은 비root(10001:10001)로 읽기 전용 루트에서 돌고, 자동 갱신·원격 측정·오류 보고 같은 필수가 아닌 통신은 공식 환경변수로 끕니다. 신뢰 init(`bot-init`)이 모델보다 먼저 root로 시작해 릴레이로만 나가는 방화벽을 걸고 권한을 모두 버린 뒤에야 Claude를 시작합니다.

```sh
ref=$(images/model/build.sh)         # 빌드 → 다이제스트로 고정 → `이름@sha256:…` 한 줄 (진행·요약은 stderr)
images/model/verify.sh "$ref"        # 실제 이미지로 점검(가짜 상류만 쓴다. 실제 모델·키는 쓰지 않는다)
```

`sandbox.apple.image`에 `build.sh`가 알려 준 참조를 넣습니다. 로컬 이미지는 이름:태그로만 조회되고 태그는 바뀔 수 있어서, `build.sh`는 빌드한 이미지를 루프백 레지스트리(CNCF Distribution, 다이제스트로 고정, `127.0.0.1`에만 잠깐 열림)에 올렸다가 **다이제스트로** 다시 받아 불변 참조를 만듭니다. 레지스트리는 그 사이에만 뜨고 끝나면 지우며, 참조를 쓰는 데 필요하지 않습니다. `container image prune -a`는 이 참조도 지우니 쓰지 않습니다.

## 어댑터와 맞춘 규약

| 항목 | 값 |
|---|---|
| 진입 | 이미지의 `ENTRYPOINT`는 `/usr/local/bin/bot-init`, 인자는 `claude <인자…>`. 경로·사용자·릴레이를 사용자 인자로 받지 않는다 |
| 필요한 capability | `--cap-drop ALL` 뒤에 `CAP_NET_ADMIN`(nft) · `CAP_SETUID`·`CAP_SETGID`(사용자 전환) · `CAP_SETPCAP`(bounding set 비우기) 넷만 더한다. 하나라도 빠지면 init이 거절한다 |
| 환경(호스트만 정한다) | `BOT_RELAY_IP`(사설 IPv4) · `BOT_RELAY_PORT`(1..65535) — 어댑터가 실제 listener 주소에서 만든다. `ANTHROPIC_BASE_URL`은 같은 주소의 `http://…`. init은 두 값만 다시 검사하고 모델에는 넘기지 않는다 |
| init이 하는 일 | nft 규칙을 적용하고 다시 읽어 확인한다. 지정한 릴레이로 향하는 TCP와 해당 연결의 응답만 허용한다. 별도 검사 프로세스가 UID·GID 10001, 보조 그룹 없음, capability 0, `no_new_privs`를 확인한 뒤 같은 권한으로 Claude를 실행한다. 실패하면 `vm_init_failed`로 종료한다. |
| 실패 | init이 모델을 시작하지 않고 종료 코드 1, init의 stderr는 `vm_init_failed` 한 단어. 그 뒤 단계(claude 자체)의 종료 코드는 그대로다 |
| 닻 | `--entrypoint /usr/local/bin/bot-anchor --user 10001:10001 --cap-drop ALL`: 비root·capability 0일 때만 대기하고, 아니면 `vm_anchor_failed`로 끝난다. 모델 경로와 따로이며 init은 닻으로 바뀌지 않는다 |
| 마운트 | 루트 읽기 전용 · `/tmp` tmpfs · `/bot` 읽기 전용 입력(작업 폴더) · `/work` 실행별 쓰기 공간(호스트가 준비·정리, init은 chown하지 않는다). `HOME=/work/home`, `CLAUDE_CONFIG_DIR=/work/claude`, 재개용 세션 기록은 `/work/claude/projects/-bot/<id>.jsonl`, 도구 서버 설정은 `/work/mcp.claude.json`(헤더는 `Bearer ${RELAY_TOKEN}` 자리표시뿐 — 세션 토큰은 환경변수 `RELAY_TOKEN`·`ANTHROPIC_API_KEY`로만 가고 어떤 파일에도 없다) |
| 망 | 호스트 전용(`--internal`) 망 · DNS 없음. 상류는 호스트가 릴레이로 여는 `http://` 주소뿐 |

## 점검이 보는 것

`verify.sh`는 어댑터와 같은 옵션으로 실제 이미지를 돌려 다음을 확인합니다. 방화벽 없이 같은 시험을 돌리는 대조 실행이 각 길이 열려 있음을 먼저 보여 줍니다.

- `claude --version`이 읽기 전용 루트에서 init을 거쳐 실행되고 이미지에 박은 판본과 같다
- init 거절: 릴레이 환경이 없거나 잘못됐을 때, capability가 하나라도 빠졌을 때, root가 아닐 때, 첫 인자가 `claude`가 아닐 때 — 종료 코드 0이 아님, stderr는 `vm_init_failed` 한 단어, 모델이 시작되지 않음
- 모델 프로세스는 uid·gid 10001, 보조 그룹 없음, capability 전부 0, `no_new_privs`. 릴레이에만 닿고 같은 게이트웨이의 다른 포트·이웃 VM(IPv4·IPv6)·호스트의 IPv6와 LAN 주소·인터넷은 막힌다. 규칙·주소·경로·권한을 바꾸려는 시도는 모두 거절되고 이웃이 이 VM에 들어오려는 시도도 막힌다
- 로컬 가짜 상류와 실제 `claude -p --bare` 대화가 끝나고, 상류는 `/v1/messages` 한 건만 받는다. 끄는 환경변수를 비우면 다른 호스트(`api.anthropic.com:443`)로 나가려는 시도가 기록된다
- 0700 작업 폴더에 uid 10001이 세션 기록을 쓰고, 새 컨테이너가 `--resume`으로 잇고, 호스트가 그 기록을 읽고 지운다

`build.sh`와 `verify.sh`는 실제 VM을 띄우므로 같은 기계의 다른 VM 작업과 겹치지 않게 `/tmp/bot-vm-canary.lock` 잠금을 쥐고(다른 VM 시험이 끝나기를 기다린다), `verify.sh`는 비어 있는 부분망(`192.168.200~239.0/24` 중 하나, `BOT_MODEL_VERIFY_SUBNET`으로 정할 수 있다)을 고정한 호스트 전용 망을 만들어 점검용 보조 VM도 그 망에서 돌립니다. 부분망을 정하지 않으면 다른 프로젝트의 휴면 망과 같은 대역이 배정될 수 있습니다.

## 판본을 올릴 때

새 판본의 `manifest.json`(서명 확인 뒤)에서 `platforms.linux-arm64-musl.checksum`을 읽어 `Containerfile`의 `CLAUDE_VERSION`·`CLAUDE_SHA256`을 함께 바꾸고, 다시 `build.sh`·`verify.sh`를 돌린 뒤 새 참조를 호스트 설정에 넣습니다. 기반 이미지를 올릴 때는 `BUILDER`의 판본 태그와 다이제스트를 함께 바꿉니다. nftables·setpriv는 고정한 Alpine 가지의 현재 판본이 들어가고, 설치된 판본은 이미지의 `/etc/bot-model-image`에 기록됩니다. 같은 `Containerfile`로 다시 빌드해도 이 두 패키지의 판본이 달라지면 다른 이미지가 되므로, 운영에는 `build.sh`가 알려 준 다이제스트 참조를 씁니다.

## 알아 둘 것

- **v2.0 공개 범위.** 모델 VM에 관해서는 이미지 빌드 정의와 스크립트만 공개합니다. 완성 이미지는 운영자가 로컬에서 빌드합니다. 완성 이미지의 향후 배포 여부는 별도로 결정합니다.
- **라이선스.** `deny.toml`의 허용 목록은 Rust 의존성과 저장소에 포함하는 파일에만 적용합니다. 로컬 이미지에 설치되는 BusyBox·nftables·setpriv 등 패키지의 판본과 라이선스는 이미지 안의 `/etc/bot-model-image`에서 확인할 수 있습니다.
- **방화벽이 막는 것.** `--internal` 망만으로는 VM이 호스트에서 모든 인터페이스(`0.0.0.0`)에 열린 서비스와 같은 망의 이웃 VM에 닿습니다. init의 nft 규칙이 이를 VM 안쪽에서 막습니다. 호스트 쪽에서도 릴레이는 게이트웨이 주소에만 바인드하고 호스트에 필요 없는 서비스를 열어 두지 않는 것이 좋습니다.
- **Apple silicon 전용**(`linux-arm64-musl`). 커널은 Apple container가 제공하며 nftables가 내장돼 있습니다(`CONFIG_NF_TABLES` 등).
- 빌드 캐시와 기반 이미지는 `container` 서비스의 빌더 VM에 남습니다. 필요 없으면 `container builder stop`과 `container image prune`으로 정리합니다(다이제스트 참조는 `prune -a`로 지워지니 `-a`는 쓰지 않습니다).

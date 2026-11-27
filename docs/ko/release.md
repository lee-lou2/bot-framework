# 릴리스 게이트

[English](../release.md) | 한국어

배포 전에 `scripts/release-check.sh`를 실행하세요(CI도 같은 스크립트를 실행). 릴리스 전 검증에서는 배포 가능한 저장소 전반의 커밋 신원과 비공개 용어 검사가 추가됩니다.

v2.0에서 모델 VM 산출물은 빌드 정의와 스크립트뿐이며 사전 빌드 이미지는 배포하지 않습니다([모델 VM 이미지](model-image.md)). `deny.toml` 라이선스 허용 목록은 Rust 의존성(`cargo deny`)과 저장소에 커밋된 파일만 다룹니다.

```sh
scripts/release-check.sh                    # 기본 검사 (BOT_TEST_DATABASE_URL 설정 시 PG 테스트 실행)
scripts/release-check.sh --strict           # 릴리스 전 게이트: PostgreSQL 필수, SKIP 시 실패
scripts/release-check.sh --only "fmt deny"  # 지정한 단계만 실행
scripts/release-check.sh --list             # 단계 이름 출력
```

## 게이트 단계

| 단계 | 검증 내용 | CI 대응 작업 |
|---|---|---|
| `clean` | 작업 트리와 인덱스가 `HEAD`와 일치하는지 확인. 변경사항이 있으면 `SKIP`(`--strict`에서는 실패). | 로컬 전용 (CI `--only`에서 제외) |
| `fmt` | `cargo fmt --all -- --check` | 동일 |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` (`--bin bot` 포함) | 동일 |
| `test` | `cargo test --workspace --locked --no-fail-fast`(한 번에 모든 실패 노출). `BOT_TEST_DATABASE_URL`이 설정되면 PG 테스트 실행(미설정 시 자체 건너뜀). `--strict`에서는 DB 미설정·연결 불가 시 사전 실패하고 `BOT_TEST_REQUIRE_DB=1`과 `BOT_REQUIRE_NODE=1`을 설정. | 동일 (PostgreSQL 16 서비스 + DB 필수) |
| `integration` | 필수 엔드투엔드 통합 테스트: 실제 `claude` CLI, `srt` 샌드박스, Apple container 모델 VM, 합성 시크릿 스윕(`secret_sweep`: stderr/감사/스토어/컬렉터 누출 0건), 공식 OpenAI SDK(`openai_compat`)를 `BOT_CLAUDE_CANARY`, `BOT_SRT_CANARY`, `BOT_CONTAINER_CANARY`, `BOT_OPENAI_CANARY=required`, `BOT_TEST_REQUIRE_DB=1`, `BOT_REQUIRE_NODE=1`로 실행. **무건너뛰기 계약:** 필수 도구, `container` 서비스, `BOT_MODEL_IMAGE`, DB, SDK 환경 중 하나라도 빠지면 사전 실패하며 모든 타깃은 1개 이상 통과하고 건너뛰기 표식(`skipped:`/`skipping:`)이 0건이어야 함. `#[ignore]` 수는 `INTEGRATION_TESTS`의 `ignored=N`과 일치해야 함(기본 `0`, 현재 CI 전용 `split_roles`만 `ignored=1`이며 `selftest`가 소스와 대조 검증). `--strict` 또는 `--only integration`에서 실행(기본 실행에서는 눈에 띄는 `SKIP`). | CI에 없음 (Apple container·실제 `claude` 없음. CI `Linux tool sandbox` 작업이 필수 모드로 `srt_canary` 실행) |
| `contracts` | `.github/scripts/check_contracts.py`(Python 3.11+ `tomllib`, `jsonschema`, `PyYAML` 필요): 파일당 ≤600줄(`Cargo.lock` 제외), 1행 `//!` 모듈 헤더, 편집기/병합 잔여물(`*.bak`, `*.orig`, `*.rej`, `*.swp`, `*~`) 금지, 스키마/예제 검증(`AGENT.md`/`SKILL.md` 프런트매터 스키마, 폴더명 일치, 닫히지 않은 `---` — 엄격한 YAML 1.2 파싱, 중복 키/앵커 금지, 크기 제한, 파일 간 참조는 `test`의 Rust `bot-profile` 테스트에서 수행), 문서 계약, CI 워크플로 강화, `rust-toolchain.toml` 고정, `crates/bot-cli/tests/openai_sdk` 고정 + CI 작업 보호(아래 상세). | 동일 |
| `deny` | `cargo deny --all-features --locked check licenses bans advisories sources` | 동일 |
| `secrets-tree` | 배포 가능 파일(추적 파일 + `.gitignore` 제외 미추적 파일)을 임시 디렉터리로 복사해 `gitleaks dir` 실행 + `gitleaks git --staged`. 목록 오류나 파일 0개 시 실패. | 동일 |
| `secrets-history` | **고정된 전체 미러**에서 `gitleaks git --log-opts="--all --full-history -m"` 실행(모든 ref와 병합 부모 포함). | 동일 |
| `identity` | 고정 미러의 모든 커밋(author + committer)과 주석 태그가 `@users.noreply.github.com`을 사용하는지 확인. git 오류 시 실패. | 로컬 전용 |
| `private-terms` | `scripts/private_terms_check.py`로 작업 트리, 인덱스, 고정 미러 이력(`Cargo.lock`과 숨김 파일 포함) 스캔. | 로컬 전용 (목록 비공개) |
| `selftest` | 합성 목록과 임시 저장소로 `scripts/test_*.py` 자체 테스트 실행. | 동일 |
| *(CI 전용)* | Linux `tool-sandbox`(`srt_canary`)와 `runs_personal_demo`(`split_roles`로 실제 도구, 승인, `cron`, PostgreSQL 검증). | CI 전용 |

**핵심 불변식:**
- **조용한 건너뛰기 금지:** 필수 도구 누락은 실패이며 건너뛴 테스트는 통과로 치지 않습니다. 환경 조건부 테스트는 반드시 `skipped:` 또는 `skipping:`을 출력해야 합니다. 새 통합 테스트는 `scripts/release-check.sh`의 `INTEGRATION_TESTS`에 등록하세요 — `scripts/test_integration_list.py`(`selftest`에 포함)가 `*_CANARY`, `have_srt`, `skipped:`를 사용하는 모든 `#[test]` 파일을 스캔해 누락을 잡습니다(DB 건너뛰기는 `BOT_TEST_REQUIRE_DB=1`이 이미 실패로 전환).
- **CI 및 툴체인 계약 검사:** `.github/workflows`는 모든 외부 액션을 40자 커밋 SHA(또는 `docker://...@sha256:...`) + 버전 주석으로, 모든 `image:`를 `@sha256:` + 버전 주석으로, `setup-node`를 정확한 `x.y.z`로 고정해야 합니다. 최상위 `permissions`는 정확히 `contents: read`(작업에서는 축소만 가능), 모든 작업에 `timeout-minutes` ≤ 120, `actions/checkout`에 `persist-credentials: false`를 설정해야 합니다. `secrets.`, `GITHUB_TOKEN`, `pull_request_target`, `*-latest` 러너, 미고정 `pip install`(정확한 버전 또는 `--require-hashes` 필수), `--ignore-scripts`와 정확한 버전 없는 `npm install`/`ci`, 다운로드를 셸로 파이핑하는 명령은 금지됩니다. 루트 `rust-toolchain.toml`은 `rustfmt`와 `clippy`를 포함한 정확한 `x.y.z` `channel`을 지정해야 합니다.
- **커밋 고정과 배포 가능 미러:** 게이트는 시작 시 `HEAD`와 `RELEASE_CHECK_REFS`를 기록하고(`Commit to check: <hash>`) 미러 생성 시와 `Checked commit: <hash>` 출력 직전에 재확인합니다(변동 시 `--strict`는 `pin`에서 실패, 기본 모드는 눈에 띄는 `SKIP`). 이력 단계(`secrets-history`, `identity`, `private-terms`)는 `RELEASE_CHECK_REFS` + `HEAD`를 임시 베어 전체 미러로 fetch하여(얕은 클론은 실패) 도달 가능한 모든 커밋, 중첩 태그 객체, 블롭, 경로를 스캔합니다. 시점 비교는 단독 체크아웃을 가정합니다(`clean`이 시작 시 작업 트리 상태 검증).
- **마스킹된 보고:** `private_terms_check.py`는 불투명 위치(객체 해시, 줄 번호, 경로가 용어와 일치하면 `path#xxxxxxxx`)와 `[list line N]`만 출력합니다. `gitleaks`는 규칙, 파일, 줄, 짧은 커밋 해시의 JSON 요약만 출력합니다(이메일, 메시지, 날짜, 시크릿 값 없음). 임시/목록/미러 경로는 라벨로만 표시됩니다(`fmt`/`clippy`/`test` cargo 출력에는 작업 디렉터리 경로가 포함될 수 있음).

## 공식 OpenAI SDK 엔드투엔드 테스트 (`openai_compat`)

실제 PostgreSQL, 실제 게이트웨이, 실제 `claude` CLI, 합성 샌드박스 위에서 공식 `openai` SDK(Python `2.29.0`, Node `6.29.0`)로 OpenAI 호환 HTTP 진입점을 엔드투엔드 검증합니다(모델 업스트림만 가짜이며 SDK는 `base_url`과 `api_key`만 재정의, 실제 OpenAI API나 키 없음).

- **와이어 및 스키마 검증:** 투명 프록시가 실제 요청 수(SDK 재시도 포함)를 세고 모든 JSON과 SSE 응답을 고정된 공식 OpenAPI 명세(`openai/openai-openapi`, 커밋 `498c71ddf6f1c45b983f972ccabca795da211a3e`, 고정 SHA-256, 3.0 `nullable`/`$ref` 포함)로 검증합니다. PG 호출 원장과 가짜 업스트림이 실행, 모델 호출, 도구 실행 횟수를 집계합니다.
- **고정 테스트 환경:** `scripts/openai-sdk-setup.sh`가 `target/` 아래(저장소 내부 금지, 테스트 전용 MPL-2.0 휠 `certifi`·`tqdm` 2개 포함)에 해시 고정 의존성을 설치하고 SDK 버전과 스키마 커밋/SHA-256을 검증하며 온전한 설치는 오프라인으로 재사용합니다. `BOT_OPENAI_SPEC`을 전달하면 `schema/openai-subset.json`이 고정 `openapi.yaml`과 일치하는지도 검사합니다.
- **우회 방지 강제:** `crates/bot-cli/tests/openai_sdk/sdk_run.py`는 `expected.json`의 모든 테스트가 정확히 1회 통과할 것을 요구하며 건너뛰기, 예상 실패, 예기치 않은 성공, todo, 취소, 누락/추가 테스트, 통과 0건, 프록시 요청 0건, 미검증 기대 스키마가 있으면 실패합니다. 실행 전 소스 검사가 `skip`, `todo`, `only`, `expectedFailure`, `skipTest`를 거부합니다. `contracts`는 `pin.json`, Python `--hash=sha256`, npm `sha512`/레지스트리/설치 스크립트 금지, 그리고 CI의 `openai-sdk` 작업이 `if`, `continue-on-error`, `shell`, 파이프, `|| true`, 직접 `unittest`/`node --test`/`pytest` 없이 `sdk_run.py manifest`, `run_stub.sh`, `run_node_stub.sh`, `check_runners.sh`를 호출하는지 검증합니다. 고장 예제 픽스처(`check_runners.sh`와 `scripts/test_openai_sdk_results.py`)가 이 규칙을 고정합니다.
- **CI 대 로컬 `--strict`:** CI의 `OpenAI SDK harness` 작업은 고정 값, 네거티브 러너, 계약 스텁만 테스트하므로 **CI 통과만으로는 실제 게이트웨이를 실행하지 않습니다**. 릴리스 전 로컬 `--strict`(`integration` / `openai_compat`) 실행이 필수입니다.

## 릴리스 전 체크리스트

1. 깨끗한 전체 클론에서 아래 필수 환경 변수와 함께 `scripts/release-check.sh --strict`를 실행하고, 통과한 커밋을 기록한 뒤 정확히 그 커밋을 푸시하세요(브랜치 이름이나 `HEAD`를 다시 읽지 말 것).
2. `RELEASE_CHECK_OFFLINE=1` **없이** `scripts/release-check.sh --only deny`를 실행해 RustSec 권고 DB를 갱신하세요(네트워크가 필요한 유일한 게이트 단계이며 CI는 항상 온라인 실행). 영향받는 의존성을 업그레이드하거나 `deny.toml`에 날짜와 근거가 포함된 `ignore` 항목을 기록하세요.
3. `deny.toml`의 `allow`는 최소로 유지하세요. `license-not-encountered` 경고를 유발하는 라이선스는 제거합니다(`multiple-versions = "warn"`은 치명적 오류 아님).

## 환경 변수

| 변수 | 용도 |
|---|---|
| `BOT_PRIVATE_TERMS_FILE` | 저장소 외부의 비공개 용어 목록 경로(누락 시 `SKIP`, 저장소 내부 경로는 거부). |
| `BOT_TEST_DATABASE_URL` | 일회용 PostgreSQL 테스트 DB(테스트가 임시 스키마 생성/삭제). `--strict`에서는 `psql`과 함께 필수, URL은 출력하지 않음. |
| `RELEASE_CHECK_OFFLINE=1` | `cargo`와 `cargo deny`의 네트워크 접근 비활성화(캐시된 `~/.cargo/advisory-dbs` 사용, 릴리스 전 온라인 `deny` 실행 필요). |
| `RELEASE_CHECK_EMAIL_DOMAIN` | `identity`에서 허용할 이메일 도메인(기본값 `users.noreply.github.com`). |
| `RELEASE_CHECK_REFS` | 고정 미러로 fetch할 배포 가능 ref 패턴(기본값 `refs/heads/* refs/tags/*`). |
| `BOT_MODEL_IMAGE` | 로컬 다이제스트 고정 `claude` 모델 VM 참조(`images/model/build.sh --cli claude`의 `name@sha256:...`). `integration` / `--strict` 필수, 출력하지 않음(`codex-cli`와 `agy-cli`는 `test` 단계의 `crates/bot-cli/tests/cli_adapters.rs`에서 스크립트 샌드박스로 검증). |
| `BOT_CLAUDE_BIN` / `BOT_SRT_BIN` | 실제 `claude` CLI / `srt`의 명시적 경로(기본값 `PATH`). |
| `BOT_OPENAI_SDK_DIR` | 공식 SDK 테스트 환경의 절대 경로(기본값 `$CARGO_TARGET_DIR/openai-sdk`). |
| `BOT_OPENAI_SPEC` | 커밋된 스키마 부분집합 추출을 검증하기 위한 원본 `openapi.yaml` 선택 경로. |
| `RELEASE_CHECK_SDK_SETUP` | SDK 설정 스크립트를 재정의하는 자체 테스트 훅(일반 사용 금지). |
| `CARGO_TARGET_DIR` | 동시 실행을 격리하기 위한 절대 빌드 디렉터리. |

## 비공개 용어 목록

비공개 이름(회사, 제품, 봇 이름)은 이 저장소 밖에 보관하고 파일 경로를 `BOT_PRIVATE_TERMS_FILE`로 전달하세요(저장소 내부 경로는 거부). 한 줄에 한 항목이며 빈 줄과 `#` 주석은 무시합니다.

| 문법 | 일치 규칙 |
|---|---|
| `<text>` | 대소문자 무시 부분 문자열 |
| `word:<text>` | 대소문자 무시 전체 단어(비영숫자 경계) |
| `re:<regex>` | 대소문자 무시 Python 정규식 |
| `cs:<regex>` | 대소문자 구분 Python 정규식(예: 식별자 형태) |

배포 가능한 모든 작업 트리 파일(`git add -A`, `Cargo.lock`과 숨김 파일 포함 — 파일명, 심볼릭 링크 대상, 내용), 스테이징 전용 diff, 전체 고정 미러 이력(모든 ref 이름, 중첩 태그, 커밋 author/committer/메시지, 삭제된 파일과 병합 충돌 해결본을 포함한 모든 과거 블롭과 경로)을 스캔합니다. 출력은 일치한 용어를 생략하므로 보고서를 안전하게 공유할 수 있습니다.

## 검증된 도구 버전과 CI 고정

2026-11-24 macOS에서 검증됨(Homebrew: `brew install gitleaks cargo-deny actionlint`).

| 도구 | 버전 | 비고 |
|---|---|---|
| `gitleaks` | `8.30.1` | CI는 다이제스트 고정 [공식 이미지](https://github.com/gitleaks/gitleaks/pkgs/container/gitleaks) `v8.30.1`을 풀하고 바이너리 버전을 검사합니다. |
| `cargo-deny` | `0.21.0` | CI는 `0.21.0`을 설치하고 바이너리 버전을 검사합니다. 온라인 실행 시 `~/.cargo/advisory-dbs`의 RustSec DB를 갱신합니다. |
| Python 모듈 | `jsonschema 4.25.1`, `PyYAML 6.0.2` | CI에 고정됨. 다른 버전은 동일한 보장을 제공하지 않을 수 있습니다. |
| 공식 `openai` SDK (테스트 전용) | Python `2.29.0`, Node `6.29.0` | 저장소 외부에 해시 고정. 배포/Rust 허용 목록 외 MPL-2.0 휠 2개(`certifi`, `tqdm`) 포함. Python 락은 CPython 3.12 Linux x86_64 및 macOS arm64 휠만 허용. |

**CI 도구 고정** (버전 업데이트 위치):

| 대상 | 고정 방식 | 업데이트 방법 |
|---|---|---|
| Rust | 루트 `rust-toolchain.toml` `channel`(`1.98.1`, `rustfmt` + `clippy` 포함). CI는 `rustup toolchain install --no-self-update` 실행. `contracts`가 강제. | `channel` 수정 후 `scripts/release-check.sh --strict` 검증. |
| Node (`tool-sandbox`, `openai-sdk`) | 정확한 `setup-node` 버전(`24.22.3`). `contracts`가 강제. | 버전 문자열 수정. |
| 서비스 DB 이미지 | `# 16.16` 주석이 붙은 `postgres:16@sha256:...`. `contracts`가 강제. | 다이제스트와 버전 주석을 함께 수정. |
| Linux 샌드박스 도구 | `srt` `0.0.83` 정확 고정, Ubuntu 22.04 apt **업스트림 글롭** `bubblewrap=0.6.1-*`, `socat=1.7.4.1-*`, `ripgrep=13.0.0-*`(설치 시 버전 로깅). 사람이 유지보수(`srt`, `cargo-deny`, `gitleaks` 포함). | 글롭 수정 후 `srt_canary` 실행. |

apt 패키지는 배포판 리비전 접미사(예: `-1ubuntu0.3`) 없이 업스트림 버전을 고정하여, 이전 `.deb` 리비전을 대체하는 보안 업데이트가 코드 변경 없이 CI를 깨뜨리지 않게 합니다. 전역 `srt` npm 설치에는 락파일이 없으며 `--ignore-scripts`로 수명 주기 스크립트를 차단합니다.

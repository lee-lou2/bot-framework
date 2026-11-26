# 공개 배포 관문

공개 저장소를 내보내기 전에 `scripts/release-check.sh`를 돌립니다. CI도 이 스크립트를 사용합니다. 공개 직전에는 내보낼 저장소에서 작성자와 비공개 금칙어까지 확인합니다.

모델 VM의 v2.0 공개물은 이미지 빌드 정의와 스크립트뿐입니다. 완성 이미지는 v2.0 배포 대상에서 제외하며, 향후 배포 여부는 별도로 결정합니다. `deny.toml`의 라이선스 허용 목록은 Rust 의존성과 저장소에 포함하는 파일에만 적용합니다. `cargo deny`는 이 중 Rust 의존성을 검사합니다. [모델 이미지 빌드와 구성요소](model-image.md)

```sh
scripts/release-check.sh                    # 기본 점검. 시험 DB가 있으면 PG 시험도 실행
scripts/release-check.sh --strict           # 공개 직전 점검. PostgreSQL 필수, 건너뛴 항목도 실패
scripts/release-check.sh --only "fmt deny"  # 지정한 단계만
scripts/release-check.sh --list             # 단계 이름
```

| 단계 | 하는 일 | CI의 짝 |
|---|---|---|
| `clean` | 작업 트리·인덱스가 커밋과 같은가. 다르면 검사한 상태와 공개될 상태가 다르니 «건너뜀»이고 `--strict`에서는 실패 | 로컬 공개 점검에서 실행. CI의 선택 목록에는 없음 |
| `fmt` | `cargo fmt --all -- --check` | 같음 |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | 같음 |
| `test` | `cargo test --workspace --locked --no-fail-fast`(첫 실패에서 멈추지 않고 모든 시험 파일을 돌려 실패를 한 번에 보인다. 실패가 하나라도 있으면 이 단계는 실패다). `BOT_TEST_DATABASE_URL`이 있으면 PostgreSQL 시험까지 돈다. 없으면 그 시험은 스스로 건너뛴다. **`--strict`에서는 DB가 필수다:** 주소가 없거나 연결할 수 없으면 시험 전에 실패하고, 연결되면 `BOT_TEST_REQUIRE_DB=1`을 걸어 DB·스키마 문제가 «건너뜀»으로 바뀌지 않게 한다 | 같음. CI에서는 PostgreSQL 16 서비스를 띄우고 DB 시험을 필수로 설정한다 |
| `integration` | **필수 통합 시험**: 실제 claude CLI·srt 도구 샌드박스·모델 VM(Apple container)·합성 비밀 쓸어 보기(`secret_sweep`: 표준 오류·감사·저장소·수집기에 합성 비밀 0), **공식 OpenAI SDK 종단**(`openai_compat`: 아래)을 필수 모드(`BOT_CLAUDE_CANARY`·`BOT_SRT_CANARY`·`BOT_CONTAINER_CANARY`·`BOT_OPENAI_CANARY=required`, `BOT_TEST_REQUIRE_DB=1`, `BOT_REQUIRE_NODE=1`)로 돌린다. 이 시험들은 환경이 없으면 스스로 «건너뛰고 통과»하므로, 통과만으로는 돌았는지 알 수 없다 — 그래서 **건너뜀 0**이 조건이다: 환경(claude·srt·python3·node·`container` 서비스·`BOT_MODEL_IMAGE`·DB·공식 SDK 시험 환경)이 없으면 시험 전에 실패하고, 시험 파일마다 통과가 한 건 이상이어야 하며, 출력에 건너뜀 표지(`skipped:`·«건너뜀»·«건너뛴다»)가 하나라도 있으면 실패다. **`#[ignore]`된 시험도 돌지 않은 시험이라 통과가 아니다:** 목록 줄의 `ignored=N`(생략하면 0)과 실제 ignored 수가 **같아야** 하고(지금은 CI 전용 데모 하나인 `split_roles`만 `ignored=1`), 소스의 `#[ignore]` 수와 목록의 N은 `selftest`가 대조한다. `--strict`에서는 늘 돌고, 그 밖에는 `--only integration`으로 고를 때만 돈다(기본 실행에서는 눈에 띄는 «건너뜀») | **CI에 없음**(CI에는 Apple container와 실제 claude가 없다. CI의 «Linux tool sandbox» 작업이 srt를 따로 필수 모드로 돌린다) |
| `contracts` | 파일마다 600줄 이하 · 스키마 · 예시(`check_contracts.py`는 예시 매니페스트와 `AGENT.md`/`SKILL.md` front matter의 JSON Schema·폴더명·미닫힘 `---`만 검사하며, YAML 1.2 엄격 파싱·중복 키·크기 상한·교차 참조 전체 검증은 `test` 단계의 Rust `bot-profile` 시험이 맡는다) · **CI 워크플로 하드닝** · **Rust 도구체인 고정**(`.github/scripts/check_contracts.py`): `.github/workflows`의 외부 액션은 전체 커밋 SHA(또는 이미지 다이제스트)와 판본 주석, `image:`는 `@sha256:` 다이제스트와 판본 주석, `setup-node`는 `x.y.z` 정확 판본, 최상위 `permissions`는 정확히 `contents: read`이고 작업은 줄이기만 하며, 모든 작업에 `timeout-minutes`(120 이하), `actions/checkout`은 `persist-credentials: false`, `secrets.`·`GITHUB_TOKEN`·`pull_request_target`·`*-latest` 러너는 없다. 공개할 파일 목록에는 편집기·병합 잔여 파일(`*.bak`·`*.orig`·`*.rej`·`*.swp`·`*~`)이 없다(무시 파일은 제외). 루트 `rust-toolchain.toml`은 `channel`이 `x.y.z` 정확 판본이고 rustfmt · clippy를 포함한다. **공식 SDK 시험 환경**(`crates/bot-cli/tests/openai_sdk`)은 `pin.json`(공식 OpenAPI의 40자 커밋·sha256과 SDK 정확 판본), 파이썬 잠금(모든 패키지 `==`와 `--hash=sha256`), npm 잠금(모든 패키지 sha512 integrity·registry 주소·설치 스크립트 없음), 저장소에 둔 스키마 조각이 `pin.json`의 sha256과 같은지를 지키고, CI의 `openai-sdk` 작업이 소스 검사(`sdk_run.py manifest`)와 세 실행기(`run_stub.sh`·`run_node_stub.sh`·`check_runners.sh`)를 `if`·`continue-on-error`·`shell`·파이프·`|| true` 없이 부르고 `unittest`·`node --test`를 직접 쓰지 않는지(실행기를 우회하지 않는지)도 지키고, 워크플로의 `pip install`은 정확 판본 또는 `--require-hashes`, `npm install`·`npm ci`는 `--ignore-scripts`와 정확 판본이며 내려받은 것을 셸로 흘리지 않는다. 파이썬 3.11 이상이 필요하다(`tomllib`) | 같음 |
| `deny` | `cargo deny --all-features --locked check licenses bans advisories sources` | 같음. 네 검사를 잠금 파일로 실행한다 |
| `secrets-tree` | 공개될 파일(추적 + 새 파일, `.gitignore` 제외)만 임시 폴더에 복사해 `gitleaks dir`, 그리고 아직 커밋하지 않은 인덱스(`gitleaks git --staged`). 목록을 못 읽거나 복사한 파일이 하나도 없으면 «깨끗함»이 아니라 실패 | 같음 |
| `secrets-history` | **고정한 전체 복제본**에서 `gitleaks git --log-opts="--all --full-history -m"` (모든 참조와 병합 커밋의 각 부모) | 같음 |
| `identity` | 고정한 복제본의 모든 커밋(작성자·커미터)과 주석 태그의 메일이 `@users.noreply.github.com`인지. git 조회가 실패하면 실패 | CI에 없음 |
| `private-terms` | 비공개 금칙어 목록으로 작업 트리·인덱스·고정한 복제본의 이력(`Cargo.lock`·숨김 파일 포함) 검사 | CI에 없음(목록이 비공개) |
| `selftest` | 금칙어 검사기·관문 스크립트 자체의 시험(`scripts/test_*.py`, 합성 목록·임시 저장소) | 같음 |
| (없음) | 리눅스 도구 샌드박스와 `runs_personal_demo`(실제 도구·승인·cron, PostgreSQL) | CI 전용. 이 스크립트는 돌리지 않는다 |

도구가 없으면 통과가 아니라 실패로 센다. 건너뜀은 통과가 아니다. 환경이 없으면 건너뛰는 시험은 반드시 `skipped: …`로 시작하는 줄을 남긴다(`integration`이 그 표지를 센다). 시험 목록은 `scripts/release-check.sh`의 `INTEGRATION_TESTS`이고, 새 통합 시험은 거기에 더한다. **목록에서 빠지면 그 파일은 도구가 없을 때 «건너뛰고 통과»해도 아무도 모른다** — 그래서 `selftest`의 `scripts/test_integration_list.py`가 소스를 훑어 `*_CANARY`·`have_srt`·`skipped:`를 쓰거나 그런 도우미를 부르는 `#[test]`가 든 시험 파일은 모두 목록에 있는지 대조한다(데이터베이스 건너뜀은 `BOT_TEST_REQUIRE_DB=1`이 실패로 바꾸므로 대상이 아니다).

**공식 SDK 종단(`openai_compat`).** OpenAI 호환 HTTP 입구를 공식 `openai` SDK(Python 2.29.0·Node 6.29.0)로 끝에서 끝까지 확인한다: 실제 PG·실제 게이트웨이·실제 `claude` CLI·합성 샌드박스 위에서 모델 상류만 가짜이고, SDK는 `base_url`·`api_key`만 바꾼다. SDK와 입구 사이의 투명 프록시가 SDK 재시도까지 포함해 **실제로 간 요청**을 세고, 돌아오는 모든 JSON·SSE 응답을 고정한 공식 OpenAPI(`openai/openai-openapi` manual_spec, 커밋 `498c71d…`·sha256 고정, 3.0 `nullable`·`$ref` 포함)로 검사한다. 서버 쪽 작업·모델 호출·도구 실행은 PG의 호출 원장과 상류에서 센다. 시험 의존성은 해시 고정이고 저장소에 싣지 않는다(빌드 폴더 밑 설치, MPL-2.0 패키지 `certifi`·`tqdm` 2개는 시험 환경에만 들어간다). 준비는 `scripts/openai-sdk-setup.sh`가 한다: 잠금대로 설치하고 SDK 판본·스키마의 커밋/sha256을 확인하며, 이미 맞게 설치돼 있으면 내려받지 않는다(오프라인 재사용). `BOT_OPENAI_SPEC`에 원본 `openapi.yaml`을 주면 저장소의 스키마 조각이 그 원본에서 나온 것인지도 본다. 실제 OpenAI API·실키는 쓰지 않는다. SDK 시험은 `openai_sdk/sdk_run.py`가 돌리고 검사한다: `expected.json`에 적힌 시험 이름이 **전부 정확히 한 번 통과**해야 하고, 건너뜀(skip)·예상된 실패·뜻밖의 성공·todo·취소가 하나라도 있거나, 기대한 시험이 돌지 않았거나, 목록에 없는 시험이 끼었거나, 통과가 0건이거나, 프록시가 본 요청이 0건이거나, 기대한 공식 스키마를 하나라도 한 번도 검사하지 않았으면 실패다. 시험을 더하거나 지우면 `expected.json`을 의식적으로 고쳐야 하고(`selftest`가 소스와 대조), 시험 소스의 `skip`·`todo`·`only`·`expectedFailure`·`skipTest`는 실행 전에 소스 검사가 거절하고, 이 규칙들은 깨진 예시 시험(`openai_sdk/check_runners.sh`와 `scripts/test_openai_sdk_results.py`)으로 고정돼 있다. SDK 시험 틀 자체(공식 SDK·스키마 고정 확인, 계약 대역 기준선, 깨진 예시)는 CI의 «OpenAI SDK harness» 작업이 돈다. **CI 초록만으로 실제 게이트웨이 경로를 증명하지 않는다** — 그 작업은 실제 게이트웨이가 아니라 계약 대역을 상대한다. 공개 전에는 로컬 관문 `--strict`의 `integration`(`openai_compat`)이 필수다.

**시작·끝의 커밋 확인.** 관문은 시작할 때 HEAD와 공개할 참조의 값을 기록하고 `검사할 커밋: <해시>`로 알린다. 이력 단계는 복제하는 시점의 값을, 끝에서는 `검사한 커밋: <해시>`를 출력하기 전에 다시 읽은 값을 시작할 때와 견준다. **`--strict`에서는** 그 시점의 HEAD(분리된 HEAD 포함)나 브랜치·태그가 시작할 때와 다르면 `pin` 줄이 «실패»이고, 이력 단계는 다른 HEAD를 보면 복제하지 않고 실패한다. 그 밖의 실행에서는 `pin` 줄이 눈에 띄는 «건너뜀»이다. 아무것도 다르지 않으면 `pin` 줄은 없다. **견주는 것은 그 시점의 값뿐이다:** 검사 도중 바뀌었다가 끝나기 전에 원복되는 변화나 작업 트리 내용의 변화는 잡지 못한다(작업 트리가 커밋과 같은지는 `clean` 단계가 시작할 때 한 번 본다). CI처럼 검사 중 아무도 저장소를 바꾸지 않는 단독 체크아웃을 전제로 한다.

**검사 대상 = 공개 대상.** 이력 단계(`secrets-history`·`identity`·`private-terms`)는 공개할 참조(`RELEASE_CHECK_REFS`, 기본 모든 브랜치와 태그)를 임시 전체 복제본으로 가져와 **그 객체만** 본다. 복제본을 만든 뒤에는 저장소가 바뀌어도 세 단계가 같은 객체를 보고, 얕은 복제(이력이 잘림)는 실패다. 복제본에서 각 참조가 닿는 모든 커밋·태그 객체(태그가 태그를 가리키는 경우 포함)·blob·경로를 읽는다. HEAD가 아직 없어도 브랜치·태그는 본다. 작업 트리는 커밋과 같을 때만 «공개될 상태»이니 `clean` 단계가 다름을 알리고, 인덱스에만 있는 내용도 따로 본다. git 목록·조회가 실패하면 «문제 없음»이 아니라 실패다.

**검사 결과에 표시되는 정보.** 금칙어 검사는 금칙어·이름을 찍지 않고 위치(커밋·blob·태그 객체 해시, 줄 번호)와 «목록의 몇 번째 줄»만 찍는다. 금칙어가 든 경로·참조 이름은 짧은 해시(`경로#xxxxxxxx`)로 바꾼다. gitleaks는 JSON 보고서에서 규칙·파일·줄·짧은 커밋 해시만 요약하고(작성자·메일·메시지·날짜·값은 찍지 않는다), 목록·복제본·임시 폴더의 경로는 명령 줄 대신 이름표로 보인다. 다만 `fmt`·`clippy`·`test`는 cargo 자신의 출력이라 작업 폴더 경로가 나올 수 있다.

## 공개 직전 체크리스트

1. 깨끗한 전체 복제본에서 `scripts/release-check.sh --strict`를 환경 변수(아래 표)와 함께 끝까지 돌려 통과시키고, 통과한 커밋을 기록한다. 올리는 것은 그 커밋이다(브랜치 이름이나 HEAD를 다시 읽어 올리지 않는다).
2. **권고 DB를 최신으로 받아 `deny`를 한 번 돌린다.** `RELEASE_CHECK_OFFLINE=1`이면 `cargo deny`는 `~/.cargo/advisory-dbs`에 이미 있는 권고 DB만 써서 그 뒤에 나온 RustSec 권고를 모른다. 공개 직전에는 이 변수 없이 `scripts/release-check.sh --only deny`를 돌려 권고 DB를 새로 받고 통과를 확인한다(관문에서 네트워크가 필요한 단계는 이것뿐이다. CI는 이 변수 없이 돌아 매번 새로 받는다). 새 권고가 나오면 의존성을 올리거나 `deny.toml`의 `ignore`에 이유와 정리 시점을 적는다.
3. 허용 라이선스는 `deny.toml`의 `allow`에 쓰이는 것만 둔다. `cargo deny`가 «license-not-encountered» 경고를 내면 그 항목을 뺀다. 중복 버전 경고(`multiple-versions = "warn"`)는 실패가 아니라서 그대로 둔다.

## 환경 변수

| 이름 | 뜻 |
|---|---|
| `BOT_PRIVATE_TERMS_FILE` | 비공개 금칙어 목록의 경로(저장소 밖). 없으면 `private-terms`는 «건너뜀»이고 요약에 그 사실이 남는다. 목록이 저장소 안에 있으면 거절한다 |
| `BOT_TEST_DATABASE_URL` | PostgreSQL 시험용 데이터베이스. 시험이 임시 스키마를 만들었다 지우니 일회용 서버를 가리킨다. `--strict`에서는 필수이고 `psql`이 있어야 하며 주소는 출력하지 않는다 |
| `RELEASE_CHECK_OFFLINE=1` | 네트워크 없이 돈다(`cargo`·`cargo deny`가 아무것도 내려받지 않는다). 권고 DB가 이미 내려받아져 있어야 하고, 그 뒤에 나온 권고는 모른다 — 공개 직전에는 이 변수 없이 `deny`를 돌린다(위 체크리스트) |
| `RELEASE_CHECK_EMAIL_DOMAIN` | `identity`가 요구하는 메일 도메인(기본 `users.noreply.github.com`) |
| `RELEASE_CHECK_REFS` | 공개할 참조 패턴(기본 `refs/heads/* refs/tags/*`). 이력 단계가 이 참조만 전체 복제해 고정한다 |
| `BOT_MODEL_IMAGE` | 모델 VM 이미지 참조(`이름@sha256:…`, `images/model/build.sh`가 알려 준다). `integration`(`--strict`)이 필수로 요구하고 로컬에 있어야 한다. 출력하지 않는다 |
| `BOT_CLAUDE_BIN`·`BOT_SRT_BIN` | 실제 claude CLI·srt의 경로. 없으면 PATH에서 찾는다 |
| `BOT_OPENAI_SDK_DIR` | 공식 SDK 시험 환경 폴더(절대 경로). 없으면 `$CARGO_TARGET_DIR/openai-sdk`. `integration`이 `scripts/openai-sdk-setup.sh`로 만들고 `openai_compat`에 넘긴다 |
| `BOT_OPENAI_SPEC` | (선택) 원본 `openapi.yaml`의 경로. 주면 설치 확인 때 저장소의 스키마 조각이 고정 원본에서 그대로 나온 것인지도 본다 |
| `RELEASE_CHECK_SDK_SETUP` | 관문 자체 시험 전용: SDK 환경을 만드는 스크립트를 대역으로 바꾼다. 평소에는 쓰지 않는다 |
| `CARGO_TARGET_DIR` | 빌드 폴더. 다른 작업과 겹치지 않게 절대 경로로 준다 |

## 비공개 금칙어 목록

목록은 회사·제품·봇 이름 같은 비공개 낱말이라 **이 저장소에 두지 않습니다.** 파일 하나를 저장소 밖에 만들고 경로를 `BOT_PRIVATE_TERMS_FILE`로 줍니다. 한 줄에 하나이고 빈 줄과 `#` 줄은 무시합니다.

| 쓰는 모양 | 뜻 |
|---|---|
| `낱말` | 대소문자 무시, 부분 문자열 |
| `word:낱말` | 대소문자 무시, 낱말 경계(앞뒤가 영문자·숫자가 아님) |
| `re:식` | 대소문자 무시, Python 정규식 |
| `cs:식` | 대소문자 구분, Python 정규식(ID 모양처럼 대문자가 뜻인 것) |

검사 범위는 `git add -A`가 올릴 모든 파일(`Cargo.lock`·숨김 파일 포함)의 **이름·링크 대상·내용**, 인덱스에만 있는 내용, 그리고 고정한 복제본의 이력 전체(모든 참조 이름, 태그 객체와 그것이 가리키는 태그, 모든 커밋의 작성자·커미터·메시지, 이력에 한 번이라도 있던 모든 blob 버전과 경로 — 지워진 파일·병합 충돌 해결 포함)입니다. 결과에는 낱말을 싣지 않고 위치와 «목록의 몇 번째 줄»만 나옵니다. 그래서 출력을 공유해도 목록이 새지 않습니다. 목록이 저장소 안에 있으면 거절합니다.

## 검증한 도구 버전

2026-11-24, macOS(Homebrew)에서 확인했습니다.

| 도구 | 버전 | 비고 |
|---|---|---|
| gitleaks | 8.30.1 | CI는 [공식 이미지](https://github.com/gitleaks/gitleaks/pkgs/container/gitleaks)의 v8.30.1 다이제스트와 실행 파일 버전을 확인합니다 |
| cargo-deny | 0.21.0 | CI는 0.21.0을 설치하고 실행 파일 버전을 확인합니다 |
| 파이썬 모듈 | jsonschema 4.25.1 · PyYAML 6.0.2 | CI가 고정한 버전입니다. 다른 버전도 대개 돌지만 같은 결과를 보장하지 않습니다 |
| 공식 openai SDK(시험 전용) | Python 2.29.0 · Node 6.29.0 | SDK 시험 의존성은 해시 고정, 저장소 미포함, MPL-2.0 2개(`certifi`·`tqdm`) 포함. 라이선스 허용 목록(배포물·Rust 의존성 대상) 밖이다. Python 잠금은 CPython 3.12의 Linux x86_64·macOS arm64 휠만 허용한다 |

설치: `brew install gitleaks cargo-deny`. `cargo deny`는 오프라인이 아니면 실행할 때마다 RustSec 권고 DB(공개 자료)를 `~/.cargo/advisory-dbs`로 새로 받습니다(처음에는 전체를 내려받습니다).

CI의 GitHub 액션은 전체 커밋 SHA로 고정하고 주석에 판본을 적습니다. 올릴 때는 SHA와 주석을 함께 바꿉니다. 워크플로 파일은 `actionlint`(`brew install actionlint`)로 검사합니다. 워크플로는 `permissions: contents: read`만 쓰고 비밀(`secrets.*`)과 `pull_request_target`을 쓰지 않습니다. 이 규칙은 `contracts` 단계가 지킵니다(태그로 되돌리거나 권한을 넓히면 관문이 막습니다).

**CI 도구 판본 고정**(올릴 때 바꿀 곳):

| 대상 | 고정 방법 | 올리는 법 |
|---|---|---|
| Rust | 저장소 루트 `rust-toolchain.toml`의 `channel`(지금 1.98.1, rustfmt · clippy 포함). CI와 로컬 관문이 같은 컴파일러를 쓴다. CI는 `rustup toolchain install --no-self-update`로 설치한다 | 파일의 `channel` 한 줄을 바꾸고, 그 판본에서 `scripts/release-check.sh --strict`가 통과하는지 본다 |
| Node(`tool-sandbox`) | `setup-node`의 정확한 판본(지금 24.22.3) | 판본 문자열을 바꾼다 |
| 서비스 DB 이미지 | `postgres:16@sha256:…`(다이제스트가 정본, 주석에 판본) | 새 다이제스트와 주석을 함께 바꾼다 |
| 리눅스 도구 | `srt` 0.0.83은 정확 고정. `bubblewrap` · `socat` · `ripgrep`은 우분투 22.04 패키지의 **상류 판본 글롭**(`0.6.1-*` · `1.7.4.1-*` · `13.0.0-*`)으로 고정하고 설치 직후 판본을 로그에 찍는다 | 글롭을 바꾸고 `srt_canary`를 돌린다 |

이 표 중 Rust 도구체인(`channel`이 정확 판본이고 rustfmt · clippy 포함)·Node(정확 판본)·서비스 이미지(다이제스트와 판본 주석)는 `contracts` 단계가 지킨다(위 `contracts` 행). apt 글롭과 `srt`·`cargo-deny`·`gitleaks` 판본은 사람이 지킨다.

apt 패키지를 `-1ubuntu0.3`처럼 배포판 수정판까지 못 박지 않는 이유: 보안 갱신이 나오면 옛 수정판이 저장소에서 사라져 **코드 변경 없이 CI가 깨진다.** 상류 판본만 고정하고 수정판 번호는 열어 둔다. `srt`의 전이 의존(npm)은 전역 설치라 잠금 파일이 없다(`--ignore-scripts`로 설치 스크립트는 막는다).

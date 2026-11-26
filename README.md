# Bot Framework

**AI 모델을 믿지 않는 전제 위에서, 프로필 선언만으로 안전한 에이전트·도구·승인 봇을 운영하는 Rust 프레임워크입니다.**

프로필 폴더에 에이전트, 스킬, 도구, 승인 규칙을 선언하면 단일 바이너리(`bot`)가 CLI · Slack · OpenAI 호환 HTTP 요청과 예약을 받아 실행하고 원래 대화로 회신합니다.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/diagrams/runtime-architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/diagrams/runtime-architecture-light.svg">
  <img alt="Bot Framework 전체 런타임 아키텍처" src="docs/diagrams/runtime-architecture-light.svg" width="100%">
</picture>

## 설계 방향

AI는 프롬프트 주입으로 속거나 실수할 수 있다고 가정하고, 구조 자체로 피해를 막습니다. ([왜 이렇게 만들었나](docs/why.md) · [보안](docs/security.md))

- **모델 격리와 비밀 비노출:** 모델은 호스트 파일·셸과 서비스 키에 닿지 못합니다. 일반 대화·도구 호출은 워커가 전송 직전 헤더에만 키를 싣는 **직접 API(`anthropic-api`·`openai-api`)**를 쓰고, 작업 공간 파일 읽기가 필요할 때는 세션 토큰만 가진 **일회용 VM(`claude-cli`)**에 가둡니다.
- **브로커 단일 관문:** 모든 도구 호출은 `Broker::call` 한 곳을 지나 `요청자 권한 ∩ 에이전트 상한 ∩ 도구 범위`를 검증한 뒤 샌드박스(`srt`)나 등록된 원격 MCP 서버에서만 실행합니다.
- **DB 우선 접수:** 접수 성공은 PostgreSQL 커밋 성공입니다. 프로세스가 재시작되거나 승인을 기다려도 요청, 체크포인트, 효과 원장, 회신(Outbox), 감사 체인을 잃지 않습니다.
- **되돌릴 수 없는 일은 사람 승인:** 외부 발송·삭제 같은 효과(effect)는 정확한 인자에 묶인 승인 카드로 사람만 승인합니다. 모델은 승인할 수 없습니다.

## 현재 구성

한 호스트에서는 `bot serve --role all`로 네 역할을 함께 띄우고, 필요하면 `--role`로 역할별 프로세스를 나눌 수 있습니다.

| 역할 | 크레이트 | 맡는 일 |
|---|---|---|
| **Gateway** | `bot-gateway` | CLI · Slack · OpenAI 호환 HTTP(`/v1`) 접수, 인증·정규화, Outbox 회신 |
| **Scheduler** | `bot-runtime` | 정적 Cron · 대화 예약 발화 (모델 없이 도구 직접 호출 가능) |
| **Worker & Broker** | `bot-runtime` · `bot-broker` | 큐 리스 점유, 모델 실행(직접 API / Apple `container` VM), 자식 위임, 도구 인가·승인·MCP(`srt`) 실행 |
| **Admin & Obs** | `bot-admin` | 승인·보류 판정·실행 재개/취소 웹 UI/API, OTLP 추적·로그, Sentry 호환 이슈 내보내기 |

- **8-크레이트 단방향 구조:** `bot-core` ◄ {`bot-profile`, `bot-gateway`, `bot-store`, `bot-runtime`, `bot-broker`, `bot-admin`} ◄ `bot-cli`. 가운데 여섯 크레이트는 서로 의존하지 않고 `bot-core` 포트 트레이트로만 만납니다. ([전체 구조 · 크레이트·실행 경로 다이어그램](docs/architecture.md))
- **OpenAI 호환 입구:** 공식 Python·Node SDK의 `base_url`을 `http://127.0.0.1:8788/v1`로 바꾸고 발급받은 키로 `models.list()`와 `chat.completions.create()`를 그대로 호출할 수 있습니다. ([SDK 연결 예시](docs/gateway.md#openai-호환-api) · [모델 바인딩](docs/profile-spec.md#openai-모델-바인딩))
- **프로필 단위 격리:** 개인·팀·조직 프로필을 폴더 단위로 나누어 검증(`bot validate`)하고 활성화합니다. ([개인 예시](examples/profiles/personal) · [팀 템플릿](examples/profiles/team))

## 빠른 시작 (로컬 데모)

외부 계정 없이 로컬 데모 모델과 합성 일정 MCP 서버([demo-agenda](examples/servers/demo-agenda))로 전체 흐름을 바로 확인합니다.

- **준비물:** `rustup`(저장소에 고정된 Rust 1.98.1 사용), Node.js / npm, Python 3, PostgreSQL (Linux는 `bubblewrap`, `socat`, `ripgrep` 추가)

### 1. 설치 및 기동

저장소 루트에서 실행합니다. `bot`과 `srt`가 두 터미널의 `PATH`에서 보여야 합니다.

```sh
cargo install --locked --path crates/bot-cli
npm install -g --ignore-scripts @anthropic-ai/sandbox-runtime@0.0.83
createdb -h localhost bot_demo
export BOT_DATABASE_URL="postgres://localhost/bot_demo"
export BOT_PROFILES="$PWD/examples/profiles"
bot validate
bot serve --role all
```

<details>
<summary>로컬 PostgreSQL이 없다면 (Docker로 임시 DB 띄우기)</summary>

데모마다 난수 비밀번호로 `127.0.0.1`에만 열리는 PostgreSQL 16 컨테이너를 띄울 수 있습니다. 끝나면 `docker stop bot-demo-pg`로 정리하세요.

```sh
PG_DEMO_PASS="$(python3 -c 'import secrets; print(secrets.token_urlsafe(18))')"
docker run -d --rm --name bot-demo-pg -p 127.0.0.1:5432:5432 -e POSTGRES_DB=bot_demo -e POSTGRES_PASSWORD="$PG_DEMO_PASS" postgres:16
until docker exec bot-demo-pg pg_isready -h 127.0.0.1 -U postgres -d bot_demo >/dev/null 2>&1; do sleep 1; done
export BOT_DATABASE_URL="postgres://postgres:${PG_DEMO_PASS}@127.0.0.1:5432/bot_demo"
```

</details>

### 2. 관리 화면 접속

`bot serve`가 출력한 주소(기본 `8787`, 호스트 설정 `admin.bind`로 변경 가능)를 열고, 사용자 이름에는 아무 값이나, 비밀번호에는 터미널에 표시된 값을 입력합니다. 터미널 없이 띄울 때는 `BOT_ADMIN_TOKEN_FILE`에 절대 경로(권한 `0600`)를 지정합니다. ([호스트 설정 예시](docs/profile-spec.md#새-프로필의-호스트-설정))

### 3. 대화 · 예약 · 승인 체험

다른 터미널에서 같은 저장소 루트로 이동해 실행합니다.

| 해 볼 일 | 실행 / 조작 | 확인 내용 |
|---|---|---|
| **대화** | `bot chat --profile personal "오늘 일정 알려줘"` | `main` 에이전트가 `agenda.list`를 조회해 CLI로 답합니다. 팀 예시는 `--profile team`으로 실행합니다. |
| **예약** | 관리 화면 실행 목록에서 다음 분에 생긴 `morning` 실행 확인 | 모델 호출 없이 `agenda.list`를 직접 실행한 완료 상태를 확인합니다. |
| **사람 승인** | `bot chat --profile personal --request-id walk-1 "산책 일정을 삭제해줘"` 후 관리 화면에서 **승인** 클릭 | 대상과 인자를 확인해 승인하면 삭제 영수증을 반환합니다. `--attach --request-id walk-1`로 결과를 다시 조회할 수 있습니다. |

### 4. 내 프로필 만들기

```sh
bot init my-bot --template team            # personal · team · company 템플릿
bot add agent main/cs                      # 에이전트 뼈대 추가
bot add skill triage --agent main/cs       # 스킬 추가
bot add tool triage.lookup                 # 도구 선언 추가 (권한 상한은 수동 검토)
bot validate && bot doctor                 # 설정·환경 점검
```

자세한 절차는 [첫 도구 만들기](docs/first-tool.md) · [팀 템플릿 안내](docs/team-template.md) · [명령 안내](docs/gateway.md#자격-보관소와-명령)를 참고하세요.

## 문서 안내

| 주제 | 문서 |
|---|---|
| 설계 · 구조 | [왜 이렇게 만들었나](docs/why.md) · [전체 구조](docs/architecture.md) · [용어집](docs/glossary.md) · [로드맵](docs/roadmap.md) |
| 프로필 · 도구 | [프로필 작성](docs/profile-spec.md) · [첫 도구 만들기](docs/first-tool.md) · [팀 템플릿](docs/team-template.md) · [MCP 도구](docs/plugins.md) |
| 보안 · 복구 | [보안](docs/security.md) · [장애 복구](docs/reliability.md) · [모델 이미지](docs/model-image.md) |
| 운영 · 확장 | [게이트웨이](docs/gateway.md) · [관리·관측](docs/admin.md) · [확장 안내](docs/extending.md) · [배포 점검](docs/release.md) |

## 라이선스

[MIT](LICENSE-MIT)와 [Apache-2.0](LICENSE-APACHE) 중 하나를 선택해 사용할 수 있습니다.

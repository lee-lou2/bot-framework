# Bot Framework

[English](README.md) | 한국어

**AI 모델은 신뢰할 수 없다는 전제 아래, 선언적 프로필 디렉터리에서 안전한 에이전트·도구·사람 승인 워크플로를 실행하는 Rust 프레임워크입니다.**

프로필 디렉터리에 에이전트, 스킬, 도구, 승인 규칙을 선언하면 단일 바이너리(`bot`)가 CLI, Slack, Telegram, OpenAI 호환 HTTP 요청, 스케줄 트리거를 수락해 엄격한 신뢰 경계 안에서 실행하고 원래 대화로 응답합니다.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/diagrams/runtime-architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/diagrams/runtime-architecture-light.svg">
  <img alt="Bot Framework 런타임 아키텍처" src="docs/diagrams/runtime-architecture-light.svg" width="100%">
</picture>

## 설계 철학

모델은 환각을 일으키거나 프롬프트 인젝션으로 탈취될 수 있다고 가정하고, 프롬프트가 아니라 구조적으로 피해를 차단합니다. ([설계 배경](docs/ko/why.md) · [보안](docs/ko/security.md))

- **모델 격리와 시크릿 노출 차단:** 모델은 호스트 파일, 셸, 서비스 키에 절대 접근하지 못합니다. 일반 대화와 도구 호출 실행에는 **직접 API(`anthropic-api`, `openai-api`)**를 사용해 워커가 아웃바운드 요청 헤더에만 프로바이더 키를 첨부하며, CLI 하니스(`claude-cli`, `codex-cli`, `agy-cli`)는 세션 토큰만 보유한 **일회용 VM** 안에서 실행합니다.
- **단일 브로커 게이트:** 모든 도구 호출은 `Broker::call`을 통과하며, `srt` 샌드박스 안에서 실행하거나 등록된 원격 MCP 서버를 호출하기 전에 `호출자 스코프 ∩ 에이전트 상한 ∩ 도구 스코프`를 강제합니다.
- **DB 우선 내구성:** 요청은 PostgreSQL 트랜잭션이 커밋된 후에만 수락됩니다. 크래시, 재시작, 승인 대기가 발생해도 요청, 체크포인트, 이펙트 원장, 아웃박스 응답, 감사 체인은 유실되지 않습니다.
- **비가역 작업의 사람 승인:** 외부 쓰기와 삭제(이펙트)는 승인 카드의 정확한 인자에 바인딩되며 사람의 확인을 요구합니다. 모델은 어떤 작업도 승인할 수 없습니다.

## 런타임 아키텍처

한 호스트에서 `bot serve --role all`로 네 역할을 함께 실행하거나 `--role`로 프로세스를 분리할 수 있습니다.

| 역할 | 크레이트 | 책임 |
|---|---|---|
| **Gateway** | `bot-gateway` | CLI, Slack, Telegram, OpenAI 호환 HTTP(`/v1`) 접수, 인증/정규화, 아웃박스 전달 |
| **Scheduler** | `bot-runtime` | 정적 크론 및 대화형 스케줄(모델 없이 도구를 직접 호출 가능) |
| **Worker & Broker** | `bot-runtime` · `bot-broker` | 큐 리스, 모델 실행(직접 API / Apple `container` VM), 자식 위임, 도구 권한/승인/MCP(`srt`) |
| **Admin & Obs** | `bot-admin` | 승인·보류(hold)·실행 재개/취소용 웹 UI/API, OTLP 트레이스/로그 및 Sentry 호환 이슈 내보내기 |

- **8개 크레이트 단방향 그래프:** `bot-core` ◄ {`bot-profile`, `bot-gateway`, `bot-store`, `bot-runtime`, `bot-broker`, `bot-admin`} ◄ `bot-cli`. 중간 6개 크레이트는 런타임 의존성에서 서로 의존하지 않으며 `bot-core` 포트 트레이트를 통해서만 통신합니다. ([아키텍처 및 다이어그램](docs/ko/architecture.md))
- **하이브리드 메모리 및 자동 저장:** 추가 확장 없는 PostgreSQL 하이브리드 회상(3072차원 IEEE-754 `f16` 임베딩 + 유니코드 어휘/트라이그램 + z-score 오탐 방지 게이트 + RRF), 내장 `memory.search`/`save`/`forget`/`share` 도구, 그리고 1:1 DM/CLI 대화에서 생성 토큰 없이 저장·갱신·삭제를 자동 판단하는 선택적 System One(`memory_judge`: `d1.2-ultra`/`jev-latest`) 연동. ([메모리 아키텍처](docs/ko/architecture.md#하이브리드-메모리와-자동-저장))
- **OpenAI 호환 엔드포인트:** 공식 Python 또는 Node SDK의 `base_url`을 `http://127.0.0.1:8788/v1`로 지정하고 발급된 키를 넣으면 `models.list()`와 `chat.completions.create()`를 호출할 수 있습니다. ([SDK 예제](docs/ko/gateway.md#openai-호환-api) · [모델 바인딩](docs/ko/profile-spec.md#openai-모델-바인딩))
- **프로필 단위 격리:** 개인·팀·조직 봇을 별도 프로필 디렉터리로 분리하고 활성화 전에 `bot validate`로 검증합니다. ([개인 예제](examples/profiles/personal) · [팀 템플릿](examples/profiles/team))

| 영역 | 지원 범위 | 비고 |
|---|---|---|
| **플랫폼** | Linux·macOS (직접 API + `srt` 도구); macOS Apple Silicon (VM CLI 하니스) | `anthropic-api`와 `openai-api`는 Linux/macOS 모두 지원; `claude-cli`/`codex-cli`/`agy-cli`는 Apple `container` VM 필요 |
| **채널** | CLI 소켓, OpenAI 호환 HTTP(`/v1`), JWT HTTP 웹훅, Slack Socket Mode, Telegram Bot API(1:1 DM) | 축소 전용 드리프트 가드로 코어 채널 중립성 유지; 새 채널은 게이트웨이 접수/전달로 추가 |
| **인증** | 정적 시크릿, 원격 MCP OAuth 2.0(PKCE), 실험적 Google 서비스 계정(RS256) 및 옵트인 CLI 구독 | PostgreSQL(`bot_credentials`) 호스트 암호화; 모델은 키나 토큰에 절대 접근 불가 ([인증 가이드](docs/ko/auth.md)) |

## 빠른 시작 (로컬 데모)

데모 모델과 합성 일정 MCP 서버([demo-agenda](examples/servers/demo-agenda))를 사용해 외부 계정 없이 전체 파이프라인을 로컬에서 체험할 수 있습니다.

- **사전 요구 사항:** `rustup`(고정된 Rust 1.98.1 툴체인 사용), Node.js / npm, Python 3, PostgreSQL(Linux에서는 `bubblewrap`, `socat`, `ripgrep` 추가 필요).

### 1. 설치 및 실행

두 터미널 모두에서 `bot`과 `srt`가 `PATH`에 있는 상태로 저장소 루트에서 실행합니다.

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
<summary>로컬 PostgreSQL이 없다면 임시 Docker 컨테이너로 시작하기</summary>

임의 비밀번호를 사용해 `127.0.0.1`에만 바인딩된 PostgreSQL 16을 시작합니다. 종료할 때는 `docker stop bot-demo-pg`를 실행하세요.

```sh
PG_DEMO_PASS="$(python3 -c 'import secrets; print(secrets.token_urlsafe(18))')"
docker run -d --rm --name bot-demo-pg -p 127.0.0.1:5432:5432 -e POSTGRES_DB=bot_demo -e POSTGRES_PASSWORD="$PG_DEMO_PASS" postgres:16
until docker exec bot-demo-pg pg_isready -h 127.0.0.1 -U postgres -d bot_demo >/dev/null 2>&1; do sleep 1; done
export BOT_DATABASE_URL="postgres://postgres:${PG_DEMO_PASS}@127.0.0.1:5432/bot_demo"
```

</details>

### 2. Admin UI 열기

`bot serve`가 출력한 URL(`Admin UI: http://...`, 기본 포트 `8787`, `admin.bind`로 변경 가능)을 엽니다. 아무 사용자 이름과 터미널에 출력된 비밀번호를 입력하세요. 비대화형 환경에서는 `BOT_ADMIN_TOKEN_FILE`에 권한 `0600`의 절대 경로를 지정합니다. ([호스트 설정 예제](docs/ko/profile-spec.md#새-프로필의-호스트-설정))

### 3. 대화, 스케줄, 승인 체험하기

저장소 루트의 두 번째 터미널에서 실행합니다.

| 흐름 | 실행 | 예상 결과 |
|---|---|---|
| **대화** | `bot chat --profile personal "Show today's agenda"` | `main`이 `agenda.list`를 호출해 CLI로 응답합니다. 팀 예제는 `--profile team`을 사용하세요. |
| **스케줄** | Admin UI 실행 목록에서 다음 분의 `morning` 실행 확인 | 모델을 호출하지 않고 `agenda.list`를 직접 완료합니다. |
| **승인** | `bot chat --profile personal --request-id walk-1 "Delete the walk event"` 실행 후 Admin UI에서 **Approve** 클릭 | 정확한 대상과 인자를 승인하면 삭제 영수증이 반환됩니다. `--attach --request-id walk-1`로 결과를 다시 확인할 수 있습니다. |

### 4. 나만의 프로필 만들기

```sh
bot init my-bot --template team            # personal, team, company 템플릿 지원
bot add agent main/cs                      # 자식 에이전트 스캐폴딩
bot add skill triage --agent main/cs       # 스킬 추가
bot add tool triage.lookup                 # 도구 선언 (스코프 상한은 직접 검토)
bot validate && bot doctor                 # 설정 및 호스트 사전 요구 사항 점검
```

자세한 내용은 [첫 번째 도구](docs/ko/first-tool.md), [팀 템플릿](docs/ko/team-template.md), [시크릿 저장소 및 명령](docs/ko/gateway.md#시크릿-저장소와-명령)을 참고하세요.

## 문서

| 주제 | 가이드 |
|---|---|
| 설계 및 아키텍처 | [설계 배경](docs/ko/why.md) · [아키텍처](docs/ko/architecture.md) · [용어집](docs/ko/glossary.md) · [로드맵](docs/ko/roadmap.md) |
| 프로필 및 도구 | [프로필 명세](docs/ko/profile-spec.md) · [첫 번째 도구](docs/ko/first-tool.md) · [팀 템플릿](docs/ko/team-template.md) · [MCP 도구](docs/ko/plugins.md) |
| 보안 및 복구 | [보안](docs/ko/security.md) · [인증 및 OAuth](docs/ko/auth.md) · [신뢰성](docs/ko/reliability.md) · [모델 이미지](docs/ko/model-image.md) |
| 운영 및 확장 | [게이트웨이](docs/ko/gateway.md) · [관리자 및 관측성](docs/ko/admin.md) · [확장 가이드](docs/ko/extending.md) · [릴리스 체크리스트](docs/ko/release.md) |

## 라이선스

[MIT](LICENSE-MIT) 또는 [Apache-2.0](LICENSE-APACHE) 듀얼 라이선스 중 선택하여 사용할 수 있습니다.

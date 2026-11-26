# Bot Framework

프로필에 에이전트, 도구, 승인 규칙을 선언하면 같은 봇과 대화하고 작업을 예약할 수 있습니다. 한 호스트에서 `bot serve --role all`로 시작하세요. 역할을 따로 띄울 때는 `gateway`, `scheduler`, `worker`, `admin` 중 하나를 지정합니다. 워커가 브로커를 함께 실행합니다.

OpenAI 호환 HTTP 입구를 구성하면 공식 Python·Node SDK의 `base_url`을 `http://127.0.0.1:8788/v1`로 바꾸고 발급받은 API 키로 에이전트에 요청할 수 있습니다. `models.list()`로 호출할 모델을 찾고 `chat.completions.create()`로 대화합니다. [SDK 연결 예시](docs/gateway.md#openai-호환-api) · [모델 바인딩 설정](docs/profile-spec.md#openai-모델-바인딩)

## 로컬 데모 시작

예시는 외부 계정 없이 로컬 데모 모델과 합성 일정 서버를 씁니다. rustup, Node.js와 npm, Python 3, 실행 중인 PostgreSQL을 준비합니다. rustup은 저장소에 고정된 Rust 1.98.1과 Cargo를 사용합니다. Linux에서는 `bubblewrap`, `socat`, `ripgrep`도 설치합니다. 저장소 루트에서 실행하세요.

```sh
cargo install --locked --path crates/bot-cli
npm install -g --ignore-scripts @anthropic-ai/sandbox-runtime@0.0.83
createdb -h localhost bot_demo
export BOT_DATABASE_URL="postgres://localhost/bot_demo"
export BOT_PROFILES="$PWD/examples/profiles"
bot validate
bot serve --role all
```

`bot`과 `srt`는 두 터미널의 PATH에서 찾을 수 있어야 합니다. 위 DB 명령은 현재 OS 사용자로 로컬 PostgreSQL에 접속할 수 있을 때의 예입니다. 이미 준비한 DB가 있으면 그 접속 주소를 `BOT_DATABASE_URL`에 넣으세요.

로컬에 PostgreSQL이 없다면 Docker로 `127.0.0.1`에만 열리는 데모 전용 PostgreSQL 16을 대신 띄울 수 있습니다. 데모마다 일회용 난수 비밀번호를 만들어 컨테이너와 접속 주소에 함께 넘깁니다(Python 3는 앞의 준비물에 포함되어 있습니다):

```sh
PG_DEMO_PASS="$(python3 -c 'import secrets; print(secrets.token_urlsafe(18))')"
docker run -d --rm --name bot-demo-pg -p 127.0.0.1:5432:5432 -e POSTGRES_DB=bot_demo -e POSTGRES_PASSWORD="$PG_DEMO_PASS" postgres:16
export BOT_DATABASE_URL="postgres://postgres:${PG_DEMO_PASS}@127.0.0.1:5432/bot_demo"
```

데모가 끝나면 `docker stop bot-demo-pg`로 컨테이너를 정리합니다.

`serve`가 출력한 관리 화면 주소를 열고, 사용자 이름 칸에는 아무 이름을, 비밀번호 칸에는 터미널에 표시된 값을 넣으세요. 터미널 없이 띄울 때는 `BOT_ADMIN_TOKEN_FILE`에 비밀번호를 받을 파일의 절대 경로를 지정하세요(권한 0600). 기본 포트 8787을 사용 중이면 호스트 설정의 `admin.bind`를 바꿀 수 있습니다. [호스트 파일 예시](docs/profile-spec.md#새-프로필의-호스트-설정)

다른 터미널에서 같은 저장소 루트로 이동한 뒤 아래 명령을 실행하세요.

| 해 볼 일 | 조작 | 기대 결과 |
|---|---|---|
| 대화 | `bot chat --profile personal "오늘 일정 알려줘"` | main이 agenda.list를 조회하고 CLI에 일정을 돌려줍니다. |
| 예약 | 관리 화면 실행 목록에서 다음 분에 생긴 `morning` 실행을 엽니다. | 상세에는 `agenda.list`의 완료 상태가 보입니다. 조회한 일정 본문은 위 대화에서 확인합니다. 이 예약은 모델 호출 없이 도구를 직접 실행합니다. |
| 승인 | `bot chat --profile personal --request-id walk-1 "산책 일정을 삭제해줘"`를 실행하고 관리 화면에서 대상과 인자를 확인한 뒤 **승인**을 누릅니다. | CLI가 가상 삭제 영수증을 표시합니다. 같은 요청의 결과는 `bot chat --profile personal --attach --request-id walk-1`로 다시 받습니다. |

개인 비서 프로필은 일정 도구의 권한과 입력을 선언하고, 별도 MCP 서버가 합성 일정으로 응답합니다. [예시 프로필](examples/profiles/personal) · [데모 도구 서버](examples/servers/demo-agenda)

팀 프로필 템플릿은 팀 리드 아래에 개발·디자인·기획·운영 에이전트를 나눈 복사용 예시이고, 같은 준비로 `bot chat --profile team "오늘 일정 알려줘"`를 해 볼 수 있습니다. [팀 예시 프로필](examples/profiles/team) · [팀 템플릿 안내](docs/team-template.md)

내 프로필은 `bot init my-bot --template team`(`personal`·`team`·`company`)으로 호스트 설정 예시와 프로필 폴더를 만들고, 출력된 안내대로 환경 변수를 정한 뒤 `bot validate`와 `bot doctor`로 점검합니다. 에이전트·스킬·도구 뼈대는 `bot add agent main/cs`, `bot add skill triage --agent main/cs`, `bot add tool triage.lookup`으로 더합니다. 권한 상한은 자동으로 바꾸지 않습니다. [첫 도구 만들기](docs/first-tool.md) · [명령 안내](docs/gateway.md#자격-보관소와-명령)

[왜 이렇게 만들었나](docs/why.md) · [전체 구조](docs/architecture.md) · [프로필 작성](docs/profile-spec.md) · [첫 도구 만들기](docs/first-tool.md) · [보안](docs/security.md) · [장애 복구](docs/reliability.md) · [MCP 도구](docs/plugins.md) · [관리·관측](docs/admin.md) · [게이트웨이](docs/gateway.md) · [배포 점검](docs/release.md) · [로드맵](docs/roadmap.md)

## 라이선스

사용자는 [MIT](LICENSE-MIT)와 [Apache-2.0](LICENSE-APACHE) 중 하나를 선택할 수 있습니다.

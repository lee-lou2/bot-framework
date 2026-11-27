# 프로필 명세

[English](../profile-spec.md) | 한국어

`BOT_PROFILES`에는 선언형 프로필 디렉터리가 하나 이상 들어갑니다. 개인 비서 프로필의 예시 구조는 다음과 같습니다.

```text
profiles/personal/
  profile.yaml
  rules.md
  knowledge/
  skills/
  agents/main/
    AGENT.md
    env.yaml
    triggers.yaml
    memory/
    knowledge/
    skills/agenda/{SKILL.md,tools.yaml[,tools.lock.yaml]}
    agents/research/AGENT.md
```

`profile.yaml`과 각 `AGENT.md`는 필수이며, 모든 스킬에는 `SKILL.md`가 필요합니다. 나머지는 모두 선택 사항입니다. 프로필마다 루트 에이전트는 하나이며, 에이전트는 직계 자식에게만 위임할 수 있습니다. 에이전트 ID는 계보 경로(`main/research`), 도구 ID는 `<skill>.<tool>`(`agenda.list`) 형식입니다. 각 세그먼트는 최대 63자의 kebab-case(`^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$`)여야 합니다.

## 최소 프로필

다음 두 파일만으로 호스트가 허용한 `assistant` 모델 별칭을 사용해 `local:owner`로 CLI 대화를 실행할 수 있습니다.

**`profile.yaml`**

```yaml
version: 1
id: personal
model: assistant
roles: {owner: [run]}
members: [{identity: "local:owner", roles: [owner]}]
bindings: [{id: desk, adapter: cli, account: owner, agent: main}]
```

**`agents/main/AGENT.md`**

```markdown
오늘 사용자의 할 일을 간결하게 요약하세요.
```

| `profile.yaml` 필드 | 필수 여부 / 기본값 |
|---|---|
| `version`, `id`, `model` | 필수. `version`은 `1`, `id`는 폴더명과 일치, `model`은 호스트가 허용한 모델 별칭. |
| `roles`, `members`, `bindings` | 필수. 미등록 주체(principal)와 입구(entrance)는 거부됩니다. |
| `timezone`, `secrets`, `super_admins` | 기본값: `UTC`, `[]`, `[]` (비어 있지 않은 `super_admins`는 정책 리비전에 포함). |
| `policy.scopes`, `policy.agents`, `policy.approve` | 기본값: 역할 스코프 합집합, 부모 상한 상속, `[]`. |

### 에이전트 내장 읽기 도구

모델 CLI의 내장 도구는 기본으로 비활성화되며(Claude Code는 `--tools ""`로 실행), 브로커가 인가한 MCP 도구만 사용할 수 있습니다. 파일을 조회해야 하는 에이전트는 `AGENT.md` YAML 프런트매터([`agent.schema.json`](../../schemas/v1/agent.schema.json))에 명시적으로 옵트인해야 합니다.

```markdown
---
description: Inspect source files and cite exact evidence
builtin: [read]
---
Locate relevant files and evidence in the provided workspace.
```

- 허용되는 `builtin` 항목은 `read`(파일 내용 읽기), `grep`(텍스트 검색), `glob`(파일명 검색)뿐입니다. 생략하거나 `[]`이면 아무것도 부여하지 않습니다. 자식 에이전트는 `builtin`을 상속받지 않습니다. 중복, 셸/쓰기/웹 도구, 알 수 없는 이름은 검증에 실패합니다.
- `builtin`은 추가 호스트 경로를 마운트하지 않습니다. 모델 VM에는 해당 시도(attempt)의 읽기 전용 입력 디렉터리와 쓰기 가능한 임시(scratch) 디렉터리만 마운트되며, 디스크에 세션 토큰 파일을 두지 않습니다.
- 직접 API 공급자(`anthropic-api`, `openai-api`)는 내장 파일 도구가 없으므로 `builtin`을 선언한 에이전트를 호출 전에 거부합니다(`unsupported_builtin`). 버전이 고정된 Claude Code `--bare` VM은 `read`만 지원하며 `grep`이나 `glob`은 실행 전에 거부합니다. 워커는 상속된 `rules.md`와 `AGENT.md` 지침을 조립해 시스템 프롬프트로 전달합니다([architecture.md](architecture.md)).

## 새 프로필의 호스트 설정

내장 데모 호스트는 `personal`과 `team` 프로필에만 `assistant` 모델 별칭을 허용합니다. `notes` 같은 새 프로필을 실행하려면 `host.json`의 `models.notes`에 별칭을 허용해야 하며, 그렇지 않으면 `bot validate`가 `model-not-allowed`로 실패합니다.

`uid`는 `id -u` 결과로, `root`는 서버 디렉터리의 절대 경로로 바꿉니다(`admin.bind` 기본값은 `127.0.0.1:8787`).

```json
{
  "version": 1,
  "gateway": {"cli": {"accounts": {"owner": {"uid": 1000}}}},
  "models": {"notes": ["assistant"]},
  "providers": {"assistant": {"kind": "demo"}},
  "servers": {
    "demo-agenda": {
      "root": "/opt/bot/examples/servers/demo-agenda",
      "command": ["bin/server", "--stdio"]
    }
  },
  "admin": {"bind": "127.0.0.1:8790"}
}
```

`BOT_PROFILES`를 `notes/`의 상위 디렉터리로, `BOT_HOST_CONFIG`를 `host.json`의 절대 경로로 설정한 뒤 `bot validate`를 실행합니다. 두 프로필에 CLI 입구를 동시에 열려면 서로 다른 OS 사용자와 계정 별칭을 매핑해야 합니다. `bot map --profile notes`로 위임 트리와 권한 상한을 확인할 수 있습니다. 데모 모델은 읽기 요청에 `agenda.list`를, 삭제 시뮬레이션에 `agenda.list` + `agenda.delete`를 호출합니다. 위임이나 지식/메모리 회상을 테스트하려면 실제 모델 공급자를 연결해야 합니다.

### 프로필 관련 호스트 레지스트리 키

`BOT_HOST_CONFIG`는 엄격한 JSON 파일(<= 1 MiB, `version: 1`, 알 수 없는 키 거부)입니다. 프로필과 관련된 주요 키는 다음과 같습니다.

| 섹션 | 키 | 용도 |
|---|---|---|
| 최상위 | `version`, `gateway`, `models`, `providers`, `servers`, `egress`, `private_egress`, `super_admins`, `sandbox`, `limits`, `admin` | 호스트 권한 레지스트리(`models`, `egress`, `super_admins`, `private_egress`는 프로필 ID별 맵). |
| `providers.<alias>` | `kind`, `model`, `endpoint`, `secret`, `effort`, `max_tokens`, `max_completion_tokens` | `kind`는 `demo`, `claude-cli`, `anthropic-api`, `openai-api` 중 하나. `effort`는 `low`/`medium`/`high`/`xhigh`/`max`. `max_tokens`(1..=1,000,000)와 OpenAI 전용 `max_completion_tokens`는 상호 배타적. `secret`은 루프백 직접 API 엔드포인트에서만 선택 사항. |
| `servers.<alias>` | `transport`, `root`, `command`, `state`, `url`, `auth` | 로컬 stdio(기본 `transport: "stdio"`): `root`, `command`, `state`(기본 `"ephemeral"` 또는 `"required"`). 원격 HTTP(`transport: "http"`): `url`, 선택적 `auth`(`{"kind":"bearer","secret":"<name>"}`). |
| `sandbox` | `srt` (`binary`, `work_root`, `extra_path`), `apple` (`image`, `network`, `relay_bind`, `work_root`, `instance`, `binary`) | 도구 샌드박스(`srt`, `extra_path`는 절대 경로 최대 16개) 및 다이제스트 고정 모델 VM(`apple`, [model-image.md](model-image.md) 참고). |
| `limits` | `tool_call_max_secs` | 도구 호출당 타임아웃 상한(초, 기본 `60`, 최대 `900`). 기본값이 아니면 활성화 지문(fingerprint)에 포함. |
| `gateway` | `cli` (`accounts`, `uid`, `os_user`), `slack` (`workspaces`, `team_id`, `bot_token`, `app_token`, `dm`, `channels`), `http` (`endpoints`, `kind`, `keys`, `secret`, `subject`, `wait_ms`, `max_active_runs`, `issuer`, `audience`, `key`) | 활성화 시 검증되는 입구 매핑([gateway.md](gateway.md)). |
| `admin` | `bind` | 로컬 관리자 UI/API 주소(`Admin UI: http://...`, [admin.md](admin.md) 참고). |

## 전체 프로필: 예약, 상속, 메모리

```yaml
version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, calendar.read, calendar.delete]
  reader: [calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "service:morning", roles: [reader]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run, calendar.read, calendar.delete]
  agents:
    main/research: [run, calendar.read]
  approve: []
```

- **바인딩(`bindings`):** 외부 입구 하나를 에이전트 하나에 연결합니다. CLI(`adapter: cli`, `account`), Slack 채널(`adapter: slack`, `workspace`, `channel`, 선택적 `events`), Slack DM(`adapter: slack`, `workspace`, `dm: true`, 선택적 `events: [dm]`), 웹훅 HTTP(`adapter: http`, `endpoint`), 또는 OpenAI 호환 HTTP(`model` 및 선택적 `approvers` 추가)를 지원합니다. 활성 프로필 전체에서 동일한 입구를 중복 바인딩할 수 없습니다.
- **상속:** 지침은 `rules.md` -> 루트 `AGENT.md` -> ... -> 현재 `AGENT.md` 순으로 이어집니다. 에이전트는 자신과 조상의 `skills/`·`knowledge/`만 볼 수 있으며, 가시 경로 내 스킬 이름 충돌은 거부됩니다. 지식 검색은 요청당 최대 8개 문서 / 32 KiB를 출처 인용과 함께 읽습니다. `AGENT.md` 프런트매터는 `model`(부모 또는 프로필 `model` 상속), `description`(한 줄 <= 200자, 비상속, 기본값은 폴더명), `builtin`을 받습니다. `env.yaml`은 비비밀 문자열 맵으로 위에서 아래로 상속되며 자식이 덮어쓸 수 있고, 셸 확장이나 호스트 환경 변수 상속은 하지 않습니다.
- **메모리 시드(`memory/`):** `(profile, agent, relative path, content hash)` 기준으로 최초 한 번만 부트스트랩됩니다. 프런트매터에는 프로필 멤버인 `owner`가 필수이며, `visibility` 기본값은 `private`(공유는 `profile`)입니다. 회상 시 프로필·에이전트·요청자 가시성(`private`은 소유자의 DM/CLI에서만, 채널·HTTP·예약은 `profile`만 조회)으로 필터링되며 회상된 항목은 신뢰할 수 없는 참고 입력으로 표시됩니다. 자식 위임 시 요청자와 대화 종류가 유지됩니다. 런타임 메모리는 `owner`를 인증된 주체에 고정하며, 소유자만 수정·삭제하거나 `profile` 공유를 승인할 수 있습니다.

## 스킬, 도구, 트리거

### `SKILL.md` 프런트매터와 크기 제한

`SKILL.md`는 상단과 닫는 `---` 줄로 구분된 Agent Skills YAML 프런트매터를 선택적으로 지원합니다([`skill.schema.json`](../../schemas/v1/skill.schema.json)). 닫히지 않은 선두 `---`(파일 끝의 단독 `---` 포함)는 `front-matter-unclosed`로 실패합니다(레거시 `AGENT.md`의 단독 `---`는 호환 유지).

| 프런트매터 키 | 규칙 |
|---|---|
| `name` | 선택; 1..=64자 `[a-z0-9]+(?:-[a-z0-9]+)*`; 스킬 폴더명과 일치해야 함(`skill-name-mismatch`). |
| `description` | 선택; 공백이 아닌 1..=1024자; 구조화된 메타데이터에 보존. |
| `license` | 선택; 공백이 아닌 문자열. |
| `compatibility` | 선택; 공백이 아닌 1..=500자. |
| `metadata` | 선택; 비어 있지 않은 문자열 키와 문자열 값의 맵. |

- **거부되는 키:** `allowed-tools`는 `skill-allowed-tools`로, 그 밖의 알 수 없는 키는 `schema`로 실패합니다. 스킬은 권한을 부여할 수 없으며([security.md](security.md)), 도구 권한은 오직 `tools.yaml`과 `profile.yaml`(`roles`, `policy`)에만 정의합니다.
- **프롬프트 본문과 크기 상한:** 프롬프트 생성 전 프런트매터는 제거되며, 본문이 비어 있으면 `empty-file`로 실패합니다. 에이전트당 가시 스킬 본문 합계(프런트매터 제외)는 **64 KiB(65,536바이트)**로 제한됩니다(`skill-too-large`). 큰 참고 자료는 `knowledge/`로 옮기거나 하위 에이전트 범위로 분리하세요.
- **슈퍼관리자와 `skill-creator`:** `profile.yaml`의 `super_admins`에 비서비스 멤버(`local:*`, `slack:*/*`, `http:*/*`)를 등록하고(`service:*` -> `super-admin-service`, 비멤버 -> `super-admin-not-member`, 호스트 `super_admins.<profile>` 상한 밖 -> `super-admin-not-allowed`), `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove` 스코프를 부여하면 `bot add skill skill-creator`를 통해 스킬과 MCP 서버를 실시간으로 관리할 수 있습니다([plugins.md](plugins.md#모델에-노출되는-내장-도구)).

### `tools.yaml`과 `tools.lock.yaml`

`tools.yaml`은 스킬의 도구들을 호스트에 등록된 단일 `server` 별칭에 연결합니다([`tools.schema.json`](../../schemas/v1/tools.schema.json)).

```yaml
version: 1
server: demo-agenda
tools:
  - id: list
    class: read
    scopes: [calendar.read]
    secrets: {}
    egress: []
    input: {type: object, properties: {}, additionalProperties: false}
  - id: delete
    class: irreversible
    scopes: [calendar.delete]
    secrets: {}
    egress: []
    input:
      type: object
      required: [event]
      properties: {event: {type: string}}
      additionalProperties: false
```

- **선택적 도구 필드:** `name`(서버 측 MCP 도구 이름이 kebab-case `id`와 다를 때 사용, `[A-Za-z0-9_.-]+` <= 128자), `external`(불리언, 기본 `false`; 검색을 포함해 외부 제3자에게 입력을 전송하면 반드시 `true`), `effects`(불리언, 기본 `false`; 다단계 쓰기 콜백 활성화), `timeout_secs`(정수 1..=900, 기본 60, `limits.tool_call_max_secs`로 상한 제한), `env`(상속된 `env.yaml`의 키 목록), `destination`(원래 대화 회신용 `"reply"` 또는 임의 수신자용 `{recipient: "args.<path>", content: "args.<path>"}` — 임의 수신자 지정 시 항상 `irreversible`로 승격), `secrets`(`{ENV_VAR: profile-secret-name}`), `egress`(`["host:port"]`, 호스트 `egress.<profile>` 상한과 교집합).
- **스키마 잠금:** `input`을 생략하면 `bot mcp sync <skill-dir>`가 생성한 같은 폴더의 `tools.lock.yaml`에서 스키마와 설명을 불러옵니다([plugins.md](plugins.md#도구-정의-잠금-toolslockyaml)). 인라인 `input`이 우선하며, 둘 다 없으면 `missing-input`으로 검증에 실패합니다. 원격 HTTP 서버(`transport: "http"`)는 `effects`, `secrets`, `env`를 금지하며 `egress`에 `url`의 정확한 `host:port`를 포함해야 합니다.

### `triggers.yaml`

```yaml
version: 1
triggers:
  - id: morning
    "on": {cron: "* * * * *"}
    principal: "service:morning"
    target: {tool: agenda.list, args: {}}
```

`on`은 5필드 `cron`(또는 7개 `@` 별칭), `{event, binding}`(`message`, `thread-reply`, `reaction`), `{http: {method, path}, binding}` 중 하나입니다. Cron 트리거는 `service:*` `principal`이 필요하고, `beat: true`(중복 헬스 핑 병합)를 지원하며, 승인 필요 도구를 대상으로 할 때는 사람 `approver`가 필요합니다(`preapproved`인 경우 제외). 이벤트와 HTTP 트리거는 바인딩의 인증된 주체를 사용합니다. `target`은 `{tool, args?}` 또는 소유 에이전트에 전달할 `{instruction}`입니다.

## OpenAI 모델 바인딩

`profile.yaml`에 `model`이 포함된 HTTP 바인딩을 추가하면 에이전트를 OpenAI 호환 모델로 노출할 수 있습니다.

```json
{"id":"assistant","adapter":"http","endpoint":"openai","model":"assistant","agent":"main"}
```

`host.json`의 `gateway.http.endpoints.openai` 아래에 엔드포인트를 등록합니다(`automation-api-key`는 해당 프로필 비밀 저장소의 비밀 이름이며, `members`에 `http:openai/build`를 `run` 권한으로 등록해야 합니다).

```json
{"kind":"openai","keys":{"automation":{"secret":"automation-api-key","subject":"build"}},"wait_ms":120000,"max_active_runs":32}
```

- 하나의 엔드포인트에 여러 모델을 둘 수 있으며, 각 `model`은 고유한 소문자 슬러그(<= 63자)여야 하고, 한 엔드포인트는 하나의 활성 프로필에만 속합니다. 키의 subject를 `members`에서 제거하면 `403`을 반환합니다. OpenAI 바인딩을 참조하는 트리거는 검증에 실패합니다. 키 슬롯, subject, approvers, 에이전트 매핑 변경에는 명시적 `--activate`가 필요하지만, 키의 비밀 값만 교체할 때는 리비전이 유지됩니다.
- 키를 발급하기 전에 승인자 매핑을 먼저 선언할 수 있습니다(`bot validate`는 `approver-key-unknown` 경고를 내지만 활성화는 허용). 멤버의 마지막 키 슬롯을 제거하면 활성화 이후 기존 키는 `401`을 받으며 다른 슬롯에는 영향을 주지 않습니다.
- HTTP 멤버의 승인을 프로필 내 사람 멤버에게 라우팅하려면 바인딩에 `approvers`를 추가합니다(로컬은 `{"principal":"local:owner"}`, 같은 워크스페이스 Slack 바인딩은 아래 예시 참고).

```json
{
  "id": "assistant",
  "adapter": "http",
  "endpoint": "openai",
  "model": "assistant",
  "agent": "main",
  "approvers": {
    "http:openai/build": {"principal": "slack:home/U123", "binding": "team"}
  }
}
```

- 웹훅은 `kind: "webhook"`(레거시 JWT 엔드포인트의 기본값)을 사용하며 바인딩에서 `model`을 생략합니다. 하나의 별칭에 웹훅과 OpenAI 모드를 섞을 수 없으며, 함께 활성화하려면 `/v1`과 충돌하는 레거시 웹훅 별칭의 이름을 변경해야 합니다. SDK 사용법, 오류, 폴링 안내 문구(`"Awaiting approval. Query again with the same Idempotency-Key and request."` / `"Request is still in progress. Query again with the same Idempotency-Key and request."`)는 [gateway.md](gateway.md)의 OpenAI 호환 API 절을 참고하세요.

## 이벤트-도구 직접 라우팅

게이트웨이는 입구 요청을 인증한 뒤 `request_id`, `binding`, `principal`, `conversation`(종류, ID, 스레드), `attachments`(권한 범위가 지정된 참조), `body`, 그리고(HTTP의 경우) `method`/`path`를 담은 검증된 봉투(envelope)를 생성합니다.

```yaml
version: 1
triggers:
  - id: cancel
    "on": {http: {method: POST, path: /agenda}, binding: inbox}
    order: 10
    match: {body.action: cancel}
    input: {event: body.event}
    target: {tool: agenda.delete}
    response: {mode: accepted}
```

- **매칭 및 매핑:** 동일한 바인딩과 이벤트(또는 HTTP `method`/`path`)의 후보는 `order` 오름차순으로 평가됩니다(기본 `0`, 그룹 내 중복 `order`는 검증 오류). `match`는 점 표기 경로(dot-path)의 리터럴 일치를 검사하며 생략하면 모두 일치합니다. `input`은 도구 인자 이름을 봉투 점 표기 경로에 매핑하며 정적 `args`와 겹칠 수 없습니다(표현식·셸·템플릿 불가).
- **프라이빗 DM 트리거:** DM 바인딩(`{id: direct, adapter: slack, workspace: home, dm: true, agent: main}`)에서 `{id: secure, on: {event: message, binding: direct}, private: true, target: {tool: inbox.receive}, input: {text: body.text}}` 같은 직접 도구 트리거는 원본 본문 텍스트를 일반 큐, 감사 로그, 모델 컨텍스트에 쓰지 않고 해당 도구로만 전달합니다. `private: true`는 DM 직접 도구 트리거에서만 허용되며, 일반 핸들러보다 앞에 위치해야 하고 도구 실패 시 대화로 폴백하지 않습니다.
- **HTTP 응답 모드:** 기본 `response: {mode: accepted}`는 커밋 후 `202`와 `request_id`를 반환합니다. `{mode: sync}`는 도구의 `{status, body}`를 직접 반환합니다(타임아웃 시 `202`로 폴백). 동일 주체의 `GET /{endpoint}/_requests/{id}` 조회는 프로필이 업데이트된 뒤에도 접수 시점에 기록된 응답 모드를 따릅니다.

### Slack 채널 이벤트 선택

채널 바인딩은 `events`를 받습니다(기본 `[mention]`, 비어 있지 않고 중복 불가, 변경 시 활성화 리비전 갱신).

```yaml
bindings:
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main,
     events: [mention, thread_reply, reaction]}
```

| 바인딩 `events` 값 | 트리거 `on.event` | 수신 이벤트 | `body` 필드 |
|---|---|---|---|
| `mention` | `message` | 등록된 채널에서의 봇 멘션 | `text` |
| `thread_reply` | `thread-reply` | 봇 참여가 검증된 스레드의 사람 답글 | `text`, `channel`, `ts`, `thread_ts` |
| `reaction` | `reaction` | 채널 메시지에 대한 사람의 이모지 리액션 | `reaction`, `channel`, `item_ts`, `item_user` |

DM 바인딩은 `events: [dm]`만 허용하며(생략과 동일), CLI와 HTTP 바인딩은 `events`를 금지합니다. 비활성화된 이벤트를 참조하면 `event-not-enabled`, 채널이 아닌 바인딩에 `thread-reply`나 `reaction`을 붙이면 `binding-kind-mismatch`로 실패합니다. 이벤트는 행위자가 프로필 멤버일 때만 접수됩니다([gateway.md](gateway.md)).

```yaml
triggers:
  - id: ack
    "on": {event: reaction, binding: team}
    match: {body.reaction: white_check_mark}
    target: {tool: reports.ack}
    input: {item: body.item_ts}
```

### 처리하지 않은 이벤트 패스

Slack 이벤트에 여러 직접 도구 트리거가 일치하면 접수 시점에 `order` 오름차순으로 후보 목록을 고정합니다. 처음 일치하는 지침(`instruction`) 트리거가 폴백이 되며(그 뒤 트리거는 도달 불가), 없으면 바인딩의 에이전트가 폴백이 됩니다(접수 및 핸드오프 시 `run` 권한 필요). 후보 도구가 `_meta["bot/pass"]: true`를 반환하면([plugins.md](plugins.md#처리하지-않은-이벤트-패스-botpass)) 실행은 다음 후보로 넘어가고 마지막에 폴백 대화로 이어지며, 매 단계마다 현재 권한과 활성 리비전을 재확인합니다. 정상 결과(오류 포함)는 체인을 종료합니다. 감사 로그에는 트리거 ID와 위치만 기록됩니다(`trigger.select`, `call.pass`, `trigger.next`). DM 트리거에 `private: true`와 함께 `private_pass: true`를 선언하면 유효한 빈 PASS 반환 시 원본 본문과 첨부 파일이 일반 DM 대화로 승격됩니다.

## 사전 승인 정적 호출과 지정 사람 승인

### 사전 승인 정적 Cron 호출 (`preapproved`)

운영자는 활성화 시점에 범위가 한정된 반복 정적 도구 호출을 사전 승인할 수 있습니다. 이는 정적 `{tool, args}`를 사용하는 비-beat `cron` 트리거에만 엄격히 허용됩니다.

```yaml
- id: notify
  "on": {cron: "0 9 * * *"}
  principal: "service:reporter"
  target: {tool: reports.notify, args: {report: daily}}
  preapproved:
    max_per_day: 24
    targets:
      - {operation: post, target: "channel:reports"}
```

- **한도:** `max_per_day`는 `1..=65535`, `targets`는 고유한 `{operation, target}` 쌍 1..=64개입니다(정확한 ID 또는 비밀 이름만 허용; 와일드카드, 접두사, URL, 공백, `..`, 비밀 값 금지).
- **적용 조건:** `input`, `instruction`, `private`, `beat`, `human_approval`, 이벤트/HTTP 트리거와 함께 쓸 수 없습니다(`invalid_preapproval`). 대상 도구는 반드시 `effects: true`를 선언해야 합니다(`preapproval_unenforceable`). `approver`를 생략할 수 있으며, 멤버십·스코프·정책·비밀·설치·egress 검사는 그대로 적용됩니다.
- **승인 부여(Grant) 수명 주기:** Grant는 명시적 `bot serve --activate`에서만 발급·교체·폐기됩니다(Grant 누락 시 호출은 `preapproval_stale`로 종료). 각 Grant는 프로필/에이전트/트리거, 소스/주체/스코프, 도구 ID, 설치 리비전, 정적 `args`의 JCS SHA-256, 정렬된 허용 쌍 목록, `max_per_day`, 대상 매핑(선언된 비밀 이름, `env`, `egress`, `destination`)의 지문에 묶입니다. 입력 중 하나라도 바뀌면 기존 Grant는 즉시 무효화되며, 롤백 시 폐기된 ID를 부활시키지 않고 새 Grant ID를 발급합니다. **한계:** 같은 비밀 이름의 값만 교체하면 지문이 바뀌지 않으므로, 재승인을 강제하려면 비밀 이름을 변경해야 합니다.
- **실행과 일시 정지:** 상속되지 않습니다(모델, 자식, wake, 사용자 예약, 다른 트리거로 전달 불가). 모든 쓰기는 허용된 `(operation, target)`과 일치하는 `effect.request{step,args:{operation,target,params}}`를 사용하며 새로운 `send:true`당 UTC 일일 할당량 1을 소모합니다(읽기/재생 = 0, `not_executed` 후 재시도 = 1, 거부는 `denied`/`stale`/`invalid`/`conflict` 반환). 운영자는 Admin API/UI로 트리거를 일시 정지/재개할 수 있으며([admin.md](admin.md)), 일시 정지는 재시작 후에도 유지되고 마지막 일시 정지/재개 전환 이전에 예약된 발생분은 건너뜁니다.

### 지정 사람 승인 (`human_approval`)

직접 도구 Slack 이벤트 트리거(`message`, `thread-reply`, `reaction`)에서는 지정된 사람의 일치하는 메시지나 리액션으로 승인 단계 1회를 충족할 수 있습니다.

```yaml
- id: confirm
  "on": {binding: team, event: reaction}
  match: {body.reaction: white_check_mark}
  input: {item: body.item_ts}
  target: {tool: reports.notify}
  human_approval:
    approvers: ["slack:work/U123"]
```

- `match`에 비어 있지 않은 정확한 확인 리터럴이 필요합니다(`message`/`thread-reply`는 `body.text`, `reaction`은 `body.reaction`; 누락 시 `invalid_human_approval`).
- `approvers`는 바인딩 워크스페이스 소속 Slack 사람 프로필 멤버 1..=16명(중복 불가)이어야 합니다(위반 시 `invalid_human_approval`).
- 증적(evidence)은 접수 시점에 해당 이벤트의 인증된 행위자를 첫 번째 후보의 트리거/바인딩/이벤트 ID에 묶고, 호출 시작 시 단 한 번 소비됩니다(증적 ID, 이벤트, 주체, 도구, 인자 다이제스트 기록). 현재 리비전과 고정된 리비전 모두에 행위자가 여전히 포함되어 있어야 합니다(`human_approval_mismatch` 시 일반 승인 카드로 폴백). 모델, 위임, PASS, 프라이빗 트리거, 소유자 전용 승인으로는 절대 상속되지 않습니다.

## 검증, 리비전 복원, 권한 규칙

`bot validate`와 `bot serve`는 동일한 검증기로 `schemas/v1/{profile,agent,skill,tools,triggers,env}.schema.json`에 대해 검사합니다. 검증된 바이트로 새 리비전을 생성하고 모든 바인딩을 원자적으로 교체하며, 실패 시 이전 리비전을 유지합니다. 실행(run)은 접수 시점에 지침과 도구 선언을 고정하고, 매 호출·승인·전달 직전에 현재 권한과 교집합을 구해 검사합니다. 폐기된 서버나 바인딩은 재개할 수 없습니다.

### 재배포 후 리비전 복원 계약

어떤 역할이든 열기 전에(최초 게시, 활성 집합 합류, 또는 `--activate`), 프로세스는 검증된 번들을 `(profile_id, revision SHA-256)` 키로 PostgreSQL에 저장하고 필요한 모든 과거 리비전을 `Registry::adopt`로 복원합니다(실패 시 활성 상태·카드·시도를 변경하지 않고 기동을 중단합니다. [reliability.md](reliability.md) 참고).

- **복원 범위:** 대상 활성 집합의 프로필 ID 기준으로 계산합니다: 게시된 각 리비전 + 미완료 실행·재개 가능한 dead letter·대기/미소비 카드에 고정된 리비전 + 열려 있거나 소진되지 않은 정적 cron 윈도우 + 활성 사용자 예약의 생성자 리비전. 게시 트랜잭션은 활성 집합을 잠그고 필수 리비전을 재검사하며, 게시가 보이면 오래된 워커는 `Stale`을 받습니다. 활성 집합에서 제거된 프로필은 복원되지 않으며 이전 권한으로 실행할 수 없습니다. 누락되거나 손상된 번들이 있으면 프로필 ID와 전체 64자 SHA-256을 출력하고 기동을 중단합니다.
- **레거시 DB 백필:** `bot profile-bundle put <profile-dir>`을 실행합니다(`BOT_DATABASE_URL` 필요, 호스트 설정·비밀 불필요). 디렉터리를 검증하고 정규 `BOTPROFILE1`/`BOTPROFILE2` 번들 SHA-256 해시를 계산해 일치하는 고정 리비전을 멱등적으로 저장하며, 바이트 불일치 시 덮어쓰지 않고 실패하고 활성 상태를 변경하지 않습니다.

| 대상 | 재평가 규칙 |
|---|---|
| 접수 / 재개된 실행 | 접수 시 고정된 지침과 도구 선언에 현재 권한의 교집합을 적용. 고정 리비전에 없으면 `unknown_tool`, 현재 폐기되었으면 `denied`(실행 0회). |
| 정적 cron | 예정 시각을 포함하는 윈도우의 리비전과 대상을 사용(재시작 후 놓친 틱 포함). |
| 사용자 예약 / wake | 발화 시 현재 멤버십, 바인딩, 전달 경로, 생성 시점 스코프 상한을 검사하고 발화 시점 리비전을 고정. |
| 대기 중인 승인 | 현재 권한, 승인자, 위험 등급, 정책, 도구, 설치 리비전을 재검사; 폐기된 도구는 `stale`로 종결. |

- **카탈로그 및 번들 포맷:** `bot_core::profile::ProfileCatalog`는 `active`, `current`, `lookup`을 제공합니다. `ProfileSource::validate`는 집합 리비전을 반환하고, `Accept.revision`은 프로필 리비전을 기록합니다. `SKILL.md` 프런트매터가 있는 번들은 `BOTPROFILE2` 헤더를 쓰며, 없는 프로필은 안정적인 해시와 레거시 복원 호환성을 위해 `BOTPROFILE1`을 유지합니다.
- **주체 식별자와 개발용 비밀:** 허용 형식은 `local:<account>`, `slack:<workspace-alias>/<U|W...>`(Slack 사용자 ID <= 32자), `http:<endpoint-alias>/<sub>`, 그리고 cron 전용 `service:<name>`입니다. 사람 승인자는 `local:*`과 `slack:*`만 가능합니다. `env` 비밀 저장소는 `(personal, calendar-reader)`를 `BOT_SECRET__personal__calendar_reader`로 매핑합니다(소문자, 하이픈은 단일 밑줄, 경계는 이중 밑줄; 값은 진단에 절대 출력되지 않음).

### `bot-profile` 검증 규칙

- **파일시스템 및 한도:** `BOT_PROFILES` 바로 아래의 프로필 폴더만 읽습니다(루트 자체는 심볼릭 링크 허용). 심볼릭 링크, 하드 링크, 소켓, FIFO, 디바이스, 비UTF-8, 제어/양방향(bidi) 문자, 대소문자만 다른 이름 충돌을 거부합니다. 허용 이름: 프로필(`profile.yaml`, `rules.md`, `agents`, `knowledge`, `skills`), 에이전트(`AGENT.md`, `env.yaml`, `triggers.yaml`, `memory`, `knowledge`, `skills`, `agents`), 스킬(`SKILL.md`, `tools.yaml`, `tools.lock.yaml`), `knowledge/`(`.md`, `.txt`), `memory/`(`.md`). `.git`, `.DS_Store`, `.gitkeep`은 무시합니다. 한도: 프로필 64개, 프로필당 파일 2,000개 및 16 MiB, 파일당 1 MiB(YAML은 256 KiB), 깊이 24, 에이전트 128개, 에이전트 계층 8단계, 에이전트당 가시 스킬 본문 64 KiB.
- **YAML 및 cron:** YAML 1.2 코어 스키마(`yes/no/on/off`는 문자열)를 따르며 앵커, 별칭, 태그, 멀티 문서, 중복/복합 키, `Inf`/`NaN`, 중첩 > 32, 항목 > 20,000개를 거부합니다. `bot_core::cron::Cron`은 5필드 cron, `@hourly`/`@daily`/`@midnight`/`@weekly`/`@monthly`/`@yearly`/`@annually`, 일/요일 필드의 `?`를 지원합니다. 도구 `input`은 [plugins.md](plugins.md)에 정의된 제한된 JSON Schema 어휘와 이식 가능한 Rust 정규식 `pattern` 규칙을 따릅니다.

| `SKILL.md` 오류 코드 | 원인 |
|---|---|
| `front-matter-unclosed` | 여는 `---`에 대응하는 닫는 `---` 줄이 없음(파일 끝의 단독 `---` 포함). |
| `skill-allowed-tools` | 프런트매터에 `allowed-tools`가 포함됨(권한은 `tools.yaml`과 `profile.yaml`에만 선언). |
| `skill-name-mismatch` | 프런트매터 `name`이 스킬 폴더명과 다름. |
| `skill-too-large` | 단일 `SKILL.md` 본문 또는 에이전트당 가시 스킬 본문 합계가 64 KiB를 초과. |
| `empty-file` | 프런트매터 제거 후 본문이 비어 있음. |
| `schema` | 알 수 없는 키이거나 `name`/`description`/`license`/`compatibility`/`metadata` 형식이 잘못됨. |

#### JSON Schema와 `bot validate` 비교

번들 스키마(`schemas/v1/*.schema.json`)는 단일 파일 구조, 닫힌 객체, 범위, 형식을 검사하고, `bot validate`와 `bot serve`는 파일 간 참조, 의미 규칙, 호스트 레지스트리를 검사합니다.

| 대상 | JSON Schema 검사 항목 | `bot validate` 전용 검사 항목 |
|---|---|---|
| `triggers.yaml` `preapproved` | 닫힌 `{max_per_day, targets}`, `1..=65535`, 고유 `{operation, target}` 1..=64개(1..=128자, 와일드카드/URL/공백/`..` 금지). | 비-beat `cron` + `target.tool` 전용; `input`/`private`/`human_approval` 금지; 대상 도구 `effects: true` 필수(`preapproval_unenforceable`). |
| `triggers.yaml` `human_approval` | 닫힌 `{approvers}`, 고유 `slack:<ws>/<U\|W...>` 1..=16개(<= 102자). | `on.event` + `target.tool` 전용; 정확한 `match`(`body.text` 또는 `body.reaction`); `private` 금지; 워크스페이스 일치 및 멤버 검사. |
| `triggers.yaml` `on.cron` | 공백으로 구분된 5필드 또는 7개 `@` 별칭. | 필드 범위(0-59, 0-23, 1-31, 1-12, 0-7), 스텝 `/n`, start<=end, DOM/DOW 전용 `?`, <= 256바이트(`bad-cron`). |
| `triggers.yaml` 기타 | 필수 키, `on`/`target` 유니온, 기본 `cron`/`response`/`private` 배치. | `private_pass`는 `private: true` 필수, `http.path` 정규화(`..`/`//`/공백/256B), 이벤트 이름, 봉투 루트, `sync`+tool, ID/`order` 고유성, 바인딩/도구/주체/`args`/`input` 검사. |
| `profile`/`tools`/`env`/`AGENT`/`SKILL` | 닫힌 객체, 버전, 네이밍/스코프/스키마 구문, `SKILL.md` 프런트매터 규칙. | `id`/스킬 `name` == 폴더명, 대소문자 정확한 IANA `timezone`, 역할/스코프/비밀/바인딩/에이전트/도구 참조, 예약 도구 ID 및 환경 변수, `skill-allowed-tools`, 64 KiB 스킬 합계, `tools.lock.yaml` 해시, 호스트 레지스트리 검사. |

- **예약된 내장 도구 ID:** 프로필의 `tools.yaml`에는 프레임워크 예약 ID 15개(`delegate`, `memory.share`, `memory.search`, `memory.save`, `memory.forget`, `schedule.create`, `schedule.list`, `schedule.cancel`, `schedule.deliver`, `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove`)를 선언할 수 없습니다(해당 스킬 네임스페이스의 다른 도구 이름은 허용).
- **참조, `env`, 권한:** `id != folder`, 정확하지 않은 IANA `timezone`, 미등록 역할/스코프, 중복 주체/바인딩 ID/입구, 누락된 `env.yaml` 키(`env-missing`), 예약된 로더/셸/프록시/호스트 환경 변수명(`env-reserved`), env와 비밀 이름 충돌(`env-secret-conflict`), 비서비스 cron 주체, 승인 필요 cron 도구의 사람 `approver` 누락, 일반 핸들러 뒤에 배치되거나 승인 필요/`effects: true` 도구를 호출하는 `private` 트리거, `input`/`args` 충돌, 비도구 `sync` 응답, 호스트 `limits.tool_call_max_secs`를 초과하는 `timeout_secs`(`tool-timeout-over-cap`)를 거부합니다.
- **권한 공식 및 해시:**
  - 멤버 스코프 = `(역할 스코프 합집합) ∩ policy.scopes`; 실행 스코프 = `멤버 스코프 ∩ 에이전트 상한`(부모 상한 ∩ `policy.agents`).
  - `irreversible` 도구와 스코프가 `policy.approve`에 포함된 `write` 도구는 승인이 필요합니다. `destination: {recipient, content}`를 가진 도구는 스냅샷에서 `irreversible`로 승격됩니다(`destination: "reply"`는 선언된 등급 유지).
  - 메모리 시드 본문 한도는 8,192 UTF-8 바이트이며, 콘텐츠 해시는 `u64` 빅엔디언 길이 접두사가 붙은 `(owner, visibility, body)`의 `SHA-256`입니다.
  - 도구의 `env: [KEY]` 키는 도구 리비전에 포함되며, 런타임에 브로커는 선언된 이름과 값이 고정 리비전과 현재 리비전에서 일치하는지 검증합니다.

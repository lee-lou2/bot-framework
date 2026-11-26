# 에이전트를 추가하려면 프로필 아래에 역할 폴더와 AGENT.md를 둔다.

`BOT_PROFILES`는 여러 프로필을 담는 디렉터리다. 개인 비서 예시의 구조는 다음과 같다.

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

profile.yaml과 각 AGENT.md가 필수다. 스킬을 만들면 SKILL.md를 둔다. 나머지는 선택이다. 루트 에이전트는 하나다. 직계 폴더만 위임할 수 있다. 에이전트 ID는 main/research 같은 계보 경로, 도구 ID는 agenda.list처럼 스킬 이름과 도구 이름을 합친 값이다. 각 조각은 최대 63자의 kebab-case이며 `[a-z][a-z0-9]*(-[a-z0-9]+)*`를 따른다.

## 가장 작은 프로필

아래 두 파일이 전문이다. 운영자가 personal에 허용한 assistant 모델과 로컬 owner 계정으로 CLI 대화를 시작할 수 있다.

**profile.yaml**

```yaml
version: 1
id: personal
model: assistant
roles: {owner: [run]}
members: [{identity: "local:owner", roles: [owner]}]
bindings: [{id: desk, adapter: cli, account: owner, agent: main}]
```

**agents/main/AGENT.md**

```markdown
사용자의 오늘 할 일을 한국어로 짧게 정리한다.
```

| profile.yaml 필드 | 필수 또는 생략 시 값 |
|---|---|
| version, id, model | 필수. model은 운영자가 이 프로필에 허용한 제공자의 모델 별칭 |
| roles, members, bindings | 필수. 등록되지 않은 주체·입구는 거절 |
| timezone, secrets | UTC, 빈 목록 |
| policy.scopes, policy.agents, policy.approve | 역할에 선언된 스코프의 합집합, 부모 상한 상속, 빈 목록 |

### 에이전트의 파일 읽기 도구

기본 설정에서는 모델 CLI의 내장 도구를 모두 끈다(Claude Code의 `--tools ""`). 브로커가 허가한 MCP 도구는 그대로 사용할 수 있다. 소스 파일을 살펴볼 역할은 자신의 AGENT.md 맨 위에 필요한 기능만 명시한다.

```markdown
---
builtin: [read]
---
맡겨진 소스에서 관련 파일과 근거를 찾는다.
```

`read`는 파일 내용, `grep`은 본문 검색, `glob`은 파일명 검색을 허용한다. 생략 또는 `[]`은 내장 도구 0개다. 자식은 자기 파일에서 다시 선택한다. 중복, 셸·쓰기·웹 도구, 그 밖의 이름은 검증 오류다. 이 필드는 호스트 경로를 추가로 마운트하지 않는다. 호스트는 읽기 격리를 지원하는 모델 샌드박스에서만 내장 도구를 허용한다. 모델 VM에 연결되는 호스트 폴더는 이 시도의 읽기 전용 입력과 쓰기 가능한 작업 폴더다. 내장 도구를 켜도 모델이 읽는 곳은 입력 폴더뿐이고, 이 시도의 세션 토큰은 어느 폴더에도 파일로 없다.

v2.0 호스트가 지원하는 모델 제공자는 `demo`, `claude-cli`, API 직접 호출(`anthropic-api`·`openai-api`)이다. API 직접 호출에는 내장 파일 도구가 없어 `builtin`을 선언한 에이전트는 모델을 부르기 전에 `unsupported_builtin`으로 거절한다. 고정한 Claude Code 2.1.331의 `--bare` 모드에서는 `read`만 사용할 수 있다. 이 실행기에서 `grep`이나 `glob`을 요청하면 모델을 시작하기 전에 거절한다. 프로필 작성자는 에이전트 폴더의 `AGENT.md`에 지침을 쓴다. 워커가 상속된 지침을 조립해 Claude의 시스템 지침으로 전달한다. [Claude Code 실행 방식](architecture.md#claude-code가-브로커에-연결하는-방법)

## 새 프로필의 호스트 설정

내장 데모 호스트는 `personal`에 모델 별칭 `assistant`를 허용한다. 새 프로필 `notes`에 같은 모델을 쓰려면 호스트 파일의 `models.notes`에 별칭을 넣는다. 이 목록이 비어 있으면 `bot validate`가 `model-not-allowed`로 거절한다.

다음은 `notes` 하나로 일정 데모를 띄우는 `host.json` 예시다. `uid`를 `id -u`의 숫자로, 서버 `root`를 예시 서버의 절대 경로로 바꾼다. `admin.bind`는 관리 화면을 기본 포트 8787 대신 8790에 연다.

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

`BOT_PROFILES`를 `notes/`의 상위 디렉터리로, `BOT_HOST_CONFIG`를 이 파일의 절대 경로로 설정한 뒤 `bot validate`를 실행한다. README의 원래 데모에서 포트만 바꾸려면 위 예시의 `models` 키에 있는 `notes`를 `personal`로 바꾸고 `BOT_PROFILES`는 예시 경로를 사용한다. 두 프로필에서 CLI 입구를 동시에 열려면 각각 다른 OS 사용자와 계정 별칭을 연결해야 한다.

데모 모델은 일정 조회에 `agenda.list`, 가상 삭제에 `agenda.list`와 `agenda.delete`를 호출한다. 이 흐름을 확인하려면 예시의 `agenda` 스킬과 역할 권한을 함께 사용한다. 위임 관계는 `bot map --profile notes`에서 확인할 수 있다. 실제 자식 호출을 시험하려면 위임을 호출하는 모델이 필요하다.

`bot validate`는 규칙·지식·기억 시드의 파일 형식과 참조를 점검한다. 데모 응답은 고정되어 있으므로, 이 자료가 답변에 쓰이는지 확인하려면 실제 모델에 해당 내용을 묻는다. 비밀이 아닌 `env.yaml` 설정은 도구가 선언한 환경 변수로 별도로 전달된다.

## 예약을 쓰는 개인 비서

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

- **profile.yaml**에서 id는 폴더명, timezone은 IANA 시간대, model은 호스트 모델 별칭이다. roles는 허용 스코프, members는 검증된 신원과 역할이다. secrets에는 허용할 도구 자격 이름만 적는다. policy.scopes는 전체 상한, agents는 지정한 경로의 추가 제한, approve는 승인이 필요한 write 스코프다. irreversible은 항상 승인받는다.
- **bindings**는 외부 입구를 한 에이전트에 연결한다. CLI의 account는 호스트가 OS 사용자에 연결한 별칭이다. Slack은 `{id: team, adapter: slack, workspace: home, channel: planning, agent: main}`(받을 사건을 고르려면 `events`, [아래](#slack-채널-이벤트-고르기)), HTTP는 `{id: inbox, adapter: http, endpoint: reminders, agent: main}`처럼 쓴다. 별칭은 호스트에 등록한 실제 워크스페이스·채널·인증된 HTTP 입구를 가리킨다. 활성 프로필 전체에서 같은 입구를 두 번 쓸 수 없다.
- **rules.md → AGENT.md** 순으로 부모에서 자식까지 지침을 합친다. 조상과 현재 역할의 skills·knowledge만 보인다. 지식 검색은 profile/knowledge와 루트부터 현재 에이전트까지의 knowledge에서만 한다. 한 요청에 최대 8개 문서·합계 32 KiB를 읽고 출처를 붙인다. AGENT.md 첫 YAML front matter의 선택 필드 `description`은 부모가 위임 대상에게서 기대할 일을 보여 주는 한 줄이다. 생략하면 해당 역할의 폴더 이름을 쓴다. `model: assistant`로 역할 모델을 바꿀 수 있다. 생략하면 부모, 루트는 프로필 모델을 쓴다. 스킬 이름 충돌은 오류다. env.yaml은 비비밀 문자열 지도이며 자식 값이 우선한다. 셸 치환과 호스트 환경 상속은 없다.
- **memory/**는 최초 기동용 기억이다. 경로·내용 해시로 한 번 등록한다. 각 항목에 owner와 visibility(private 기본, profile은 명시적 공유)를 기록한다. 시드 Markdown의 YAML front matter에는 owner가 필수이고 visibility를 생략하면 private이다. 검색은 프로필·에이전트 범위에서 현재 요청자에게 보이는 행만 읽는다. 모델은 회상한 기억 본문을 출처가 표시된 사용자 입력의 참고 자료로 받는다. private은 본인 DM·CLI에서만, 채널·HTTP·예약은 profile만 읽는다. 위임한 자식도 같은 요청자와 대화 종류를 유지한다. 실행 중 생성한 기억의 owner는 인증된 요청자로 고정한다. 수정·삭제는 소유자만 가능하며 profile로 공유할 때는 소유자가 대상·내용을 승인 카드에서 확인해야 한다.

## 일정 스킬과 예약

### SKILL.md front matter와 크기 상한

`SKILL.md` 맨 위에는 선택적으로 Agent Skills 형식의 YAML front matter(`---`로 열고 `---`로 닫는다)를 둘 수 있다([skill.schema.json](../schemas/v1/skill.schema.json)). front matter가 없는 기존 `SKILL.md`는 파일 전체를 본문으로 읽는다. 맨 위 `---`를 닫지 않으면 오타나 잘못된 메타데이터가 프롬프트로 흘러가지 않도록 `front-matter-unclosed` 오류로 거절한다.

| front matter 키 | 규칙 |
|---|---|
| `name` | 선택. 1~64자 소문자·숫자·하이픈(`-`, 앞뒤 하이픈·`--` 금지). 적으면 스킬 폴더 이름과 같아야 한다(`skill-name-mismatch`). |
| `description` | 선택. 1~1024자(공백만 불가). 구조체 메타데이터에 보존한다. |
| `license` | 선택. 1자 이상 문자열(공백만 불가). |
| `compatibility` | 선택. 1~500자 문자열(공백만 불가). |
| `metadata` | 선택. 문자열→문자열 매핑(키는 1자 이상 문자열, 값은 문자열만). |

- **거절하는 키**: `allowed-tools`는 전용 오류 `skill-allowed-tools`로 거절하고, 위 다섯 키 밖의 미지 키는 `schema`로 거절한다. 스킬 문서는 권한을 부여할 수 없으며([보안](security.md#보장-범위)), 모든 도구 선언과 권한 상한은 `tools.yaml`과 `profile.yaml`의 `roles`·`policy`에서만 정한다.
- **프롬프트와 빈 본문**: 모델 프롬프트에는 front matter를 제거한 본문만 전달한다. front matter 뒤 본문이 비어 있으면 `empty-file`로 거절한다.
- **크기 상한**: 에이전트 하나에게 보이는 스킬 본문(front matter 제외) 합계는 최대 **32 KiB(32,768바이트)**다(한 요청의 지식 문서 상한 32 KiB와 같은 수준). 단일 `SKILL.md` 본문이나 상속을 합친 에이전트별 합계가 이를 넘으면 실행 중 조용히 자르지 않고 검증·활성화 단계에서 `skill-too-large`로 거절한다. 긴 참고 자료는 `knowledge/`로 옮기거나 필요한 하위 에이전트 아래로 스킬을 나눈다.

tools.yaml의 server는 운영자가 설치한 실행 파일 또는 이미지의 고정된 별칭이다. tools의 각 항목이 호출별 환경과 네트워크 상한을 정한다. 데모 서버는 합성 일정만 사용한다. 제3자에게 입력을 보내는 도구(검색 포함)는 external: true로 선언한다. 아래 로컬 도구의 기본값은 false다.

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

실제 일정 서버라면 `secrets: {CALENDAR_TOKEN: calendar-reader}`, `egress: ["calendar.example:443"]`처럼 선언한다. calendar-reader는 profile.yaml의 secrets에도 있어야 한다. 호스트가 확인한 프로필 아래의 이름만 해석한다. 운영자가 프로필별 egress 상한을 지정하면 선언과의 교집합만 허용한다.

`input`을 생략하면 같은 폴더의 `tools.lock.yaml`에서 입력 스키마를 가져온다. 이 파일은 `bot mcp sync <스킬 폴더>`가 서버의 `tools/list`에서 선언한 도구만 받아 `{id, name, description, input_schema, sha256}`로 적으며(도구의 선택 필드 `name`은 서버가 내세우는 이름이고 생략하면 `id`다) [plugins.md](plugins.md#도구-정의-잠금-toolslockyaml)가 절차를 설명한다. 인라인 `input`이 있으면 그것이 우선이고, 둘 다 없으면 활성화를 거절한다. 잠금의 `sha256`은 내용에서 다시 계산해 맞지 않으면 거절하며, 도구 판본에 들어가 실행 때 서버의 정의와 대조한다. `servers.<별칭>`이 원격 서버(`transport: "http"`)면 호스트가 `url`의 `호스트:포트`를 알려 주고, 그 도구는 `effects`·`secrets`·`env`를 선언할 수 없으며 `egress`에 그 `호스트:포트`가 정확히 있어야 한다.

```yaml
version: 1
triggers:
  - id: morning
    "on": {cron: "* * * * *"}
    principal: "service:morning"
    target: {tool: agenda.list, args: {}}
```

triggers.yaml의 on은 5필드 cron, `{event: message, binding: team}`(Slack 채널 바인딩은 `thread-reply`·`reaction`도), `{http: {method: POST, path: /agenda}, binding: inbox}` 중 하나다. cron은 서비스 주체를 지정하며 승인 대상 도구라면 trigger의 approver에 등록된 사람 신원을 지정한다. 이벤트와 HTTP는 바인딩에서 검증한 주체를 그대로 쓴다. target은 고정 도구와 인자 또는 소유 에이전트에 보낼 `{instruction: "..."}`다. 이벤트 본문은 별도 입력으로 검증한다.

## OpenAI 모델 바인딩

클라이언트에 공개할 모델 이름과 요청을 처리할 에이전트를 프로필에 묶는다. 다음은 기존 프로필의 `bindings`에 추가할 항목이다.

```json
{"id":"assistant","adapter":"http","endpoint":"openai","model":"assistant","agent":"main"}
```

호스트의 `gateway.http.endpoints.openai`에는 아래처럼 입구 종류와 키를 등록한다. `automation-api-key`는 이 프로필 비밀 보관소의 자격 이름이다. 운영자는 난수 API 키를 보관소에 넣고 클라이언트에 발급한다. 프로필의 `members`에는 `http:openai/build`와 필요한 역할을 등록하고, 해당 에이전트에 `run`을 허용한다.

```json
{"kind":"openai","keys":{"automation":{"secret":"automation-api-key","subject":"build"}},"wait_ms":120000,"max_active_runs":32}
```

같은 입구에는 모델을 여러 개 둘 수 있다. 공개 모델 이름은 소문자 slug(최대 63자)이며 입구 안에서 유일하다. 한 입구는 활성 프로필 하나가 소유한다. 키가 가리키는 주체를 멤버에서 빼면 호출은 403으로 거절된다. 모델을 선택하면 연결한 에이전트가 직접 요청을 받는다. 트리거가 이 바인딩을 참조하면 프로필 검증이 실패한다. 키 슬롯·주체·승인 경로·에이전트 매핑 변경은 명시적 활성화 대상이다. 키 값만 교체하면 판본은 유지된다.

승인 매핑은 키를 발급하기 전에도 등록할 수 있다. 해당 멤버를 가리키는 키 슬롯이 없으면 `bot validate`에 `approver-key-unknown` 경고가 표시된다. 이 경고가 있어도 활성화할 수 있다. 한 멤버의 마지막 슬롯을 제거한 설정을 활성화하면 기존 키는 401을 받는다. 다른 멤버의 키와 승인 매핑은 계속 사용할 수 있다.

다른 사람이 일반 도구의 실행을 승인해야 한다면 이 바인딩에 아래 `approvers` 항목을 추가한다. Slack의 `team`은 같은 프로필에서 같은 워크스페이스에 연결한 Slack 채널 바인딩이다. 로컬 승인자는 `{"principal":"local:owner"}` 형태로 지정한다. 승인자는 현재 프로필의 사람 멤버여야 한다. 토큰을 발급받은 HTTP 멤버는 계속 자신의 역할로 실행한다.

```json
{
  "id": "assistant",
  "adapter": "http",
  "endpoint": "openai",
  "model": "assistant",
  "agent": "main",
  "approvers": {
    "http:openai/build": {
      "principal": "slack:home/U123",
      "binding": "team"
    }
  }
}
```

기존 웹훅은 호스트에 `kind:"webhook"`을 쓰고 프로필 바인딩에는 `model`을 두지 않는다. 옛 JWT 설정은 종류를 생략해도 웹훅으로 읽는다. 한 별칭에는 두 형식을 섞을 수 없으며, 새 API의 `/v1`과 충돌하는 기존 웹훅 이름은 함께 활성화하기 전에 바꿔야 한다. [SDK 연결·오류·재조회](gateway.md#openai-호환-api)

## 이벤트 하나를 도구 하나에 연결하기

게이트웨이는 서명 등 어댑터 인증을 검증한 뒤 봉투를 만든다. 트리거가 읽을 수 있는 필드는 request_id, binding, principal, conversation(종류·ID·thread), attachments(접근 권한이 묶인 참조), body(검증한 본문)다. HTTP 어댑터는 중복 접수를 판정할 실제 메서드와 입구 아래 경로도 봉투에 기록한다. principal과 대화 주소는 인증된 어댑터에서만 채운다.

DM 바인딩 예시는 `{id: direct, adapter: slack, workspace: home, dm: true, agent: main}`이다. 이 바인딩의 트리거 `{id: secure, on: {event: message, binding: direct}, private: true, target: {tool: inbox.receive}, input: {text: body.text}}`는 일반 큐에 쓰기 전에 분기한다. 본문은 지정 도구에만 전달하고 큐·감사·모델 맥락에는 원문을 남기지 않는다. private은 DM 직접 도구에서만 허용하고 일반 처리기가 먼저 잡을 수 있으면 설정을 거절한다. 전용 도구 실패 시 일반 대화로 넘기지 않는다.

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

같은 바인딩·사건(HTTP는 method/path)의 후보를 order 오름차순으로 검사해 첫 일치 하나만 접수한다. order는 기본 0이고 같은 후보군에서 같은 순위는 검증 오류다. match는 점 경로와 리터럴 값의 동등 비교를 모두 만족해야 한다. 생략하면 항상 일치한다. input은 도구 인자명에서 봉투의 점 경로를 가리킨다. 경로가 없거나 타입이 맞지 않거나 고정 args와 키가 겹치면 호출하지 않는다. 식·셸·템플릿은 평가하지 않는다.

### Slack 채널 이벤트 고르기

채널 바인딩의 `events`가 그 채널에서 받을 사건을 정한다. 생략하면 `[mention]`(기존 동작)이다. 명시하면 비어 있지 않고 중복이 없어야 하며, 선언을 바꾸면 활성화 판본이 바뀐다.

```yaml
bindings:
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main,
     events: [mention, thread_reply, reaction]}
```

| events 값 | 트리거 `on.event` | 받는 것 | 본문 필드 |
|---|---|---|---|
| `mention` | `message` | 등록 채널에서 봇을 멘션한 글 | `text` |
| `thread_reply` | `thread-reply` | 봇 참여가 증명된 스레드의 사람 답글 | `text`, `channel`, `ts`, `thread_ts` |
| `reaction` | `reaction` | 등록 채널의 메시지에 사람이 단 반응 | `reaction`, `channel`, `item_ts`, `item_user` |

DM 바인딩에는 `events: [dm]`만 쓸 수 있고 생략과 같다. CLI·HTTP 바인딩에는 `events`가 없다. 트리거의 `on.event`를 바인딩이 받지 않으면(`event-not-enabled`) 활성화를 거절하고, `thread-reply`·`reaction` 트리거는 채널 바인딩에만 달 수 있다(`binding-kind-mismatch`). 이벤트는 요청자(글쓴이·반응한 사람)가 프로필 멤버일 때만 접수한다. 수신 요건과 Slack 앱 설정은 [Slack 입구](gateway.md#slack-입구)에 있다.

```yaml
triggers:
  - id: ack
    "on": {event: reaction, binding: team}
    match: {body.reaction: white_check_mark}
    target: {tool: reports.ack}
    input: {item: body.item_ts}
```

### 처리하지 않은 이벤트 넘기기

Slack 이벤트에 직접 도구 트리거가 여럿 일치하면 접수가 일치한 직접 트리거를 order 순으로 **후보**로 고정한다. 에이전트 지시 트리거를 처음 만나면 그것이 기본 대화가 되고 뒤는 닿지 않는다. 지시 트리거가 없으면 기본 대화는 원래 바인딩의 에이전트가 받는 사건 입력이다: 멘션·답글은 글 그대로이고, 반응은 검증한 반응 이름과 글 ts만 담은 고정 형식이다. 기본 대화는 에이전트 실행이라 요청자에게 `run` 스코프가 있어야 하며(사슬을 만들 때와 마지막 후보가 넘길 때 현재 판본에서 다시 확인한다), 없으면 마지막 후보가 넘겨도 대화 없이 조용히 끝난다. 도구가 `_meta["bot/pass"]: true`로 «처리하지 않음»을 돌려주면 다음 후보로, 마지막 후보도 넘기면 기본 대화로 간다([표식 규칙](plugins.md#처리하지-않은-이벤트-넘기기-botpass)). 넘김은 다음 후보에 승인·쓰기 허가·멤버 권한을 주지 않는다 — 후보마다 현재 권한과 활성 판본을 다시 확인한다. 일반 결과(오류 포함)는 첫 일치에서 끝나며 다음 후보를 부르지 않는다. 처리한 도구 또는 기본 대화만 사용자에게 답한다. 선택·넘김 감사에는 후보 트리거 ID와 위치만 남는다(`trigger.select`·`call.pass`·`trigger.next`). 민감 DM의 `private: true`는 DM 이벤트의 직접 도구 트리거에 선언한다. 여기에 `private_pass: true`를 함께 선언하면 선택된 도구의 유효한 빈 PASS에서 원래 본문과 첨부 참조를 기본 DM 대화의 일반 run에 저장한다. 이 넘김은 기본 DM 대화로 바로 이어진다.

HTTP response 기본값 accepted는 커밋 후 202와 request_id를 준다. `{mode: sync}`는 고정 형식 `{status, body}`의 도구 결과를 HTTP 상태와 본문으로 사용한다. 대기 기한을 넘기면 202로 전환한다. 같은 인증 주체가 Location의 `/{endpoint}/_requests/{id}`를 GET하면 최초 접수 때 저장한 방식으로 결과를 받는다. 프로필을 나중에 바꿔도 같은 요청의 응답 방식은 유지된다. 상태는 허용된 최종 응답 코드, 본문은 크기가 제한된 JSON이다.

## 사전 승인한 정적 호출과 지정한 사람의 승인

위험한 도구를 사람 없이 정해진 시각에 한정해 쓰려면, 운영자가 범위를 정해 **활성화할 때** 사전 승인한다. 모델 없이 고정 도구와 고정 인자로 직접 시작하는 `cron` 트리거에만 붙인다.

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

- `max_per_day`는 1..=65535다. `targets`는 비어 있지 않은 중복 없는 `{operation, target}` 목록(최대 64)이다. `operation`은 고정 작업 종류, `target`은 운영자가 정한 정확한 대상 ID나 자격의 **이름**이다. 와일드카드·접두어·URL·공백·자격 값은 쓸 수 없다. 여러 대상에 쓰려면 각각 열거한다.
- `input` 매핑·템플릿·`instruction`·`private`·`beat`·`human_approval`과 함께 쓰거나 이벤트·HTTP 트리거에 쓰면 `invalid_preapproval`이다. 도구가 `effects: true`가 아니면(모든 쓰기가 `effect.request`를 거친다고 보증할 수 없다) `preapproval_unenforceable`이다. 승인이 필요한 도구라도 이 트리거에 한해 사람 `approver`를 생략할 수 있다. 서비스 주체의 멤버십·스코프·정책·자격·설치·egress 검증은 그대로다.
- 승인 ID는 운영자의 명시적 활성화(`bot serve --activate`)에서만 발급·갱신·철회하고 범위를 한 줄로 보여 준다. 처음 게시를 `--activate` 없이 하거나 그냥 재시작·합류하면 승인을 만들지도 지우지도 않는다(승인이 없으면 호출은 `preapproval_stale`로 닫힌다). 같은 집합을 `--activate`로 다시 시작하면 아직 없는 승인을 지금 발급한다. 승인은 프로필·에이전트·트리거, 트리거 원천·주체·실행 스코프, 도구 ID, 설치한 실행물 판본, 고정 인자의 JCS SHA-256, 정렬된 허용 쌍, 상한, 대상 매핑(선언한 자격 이름·선택 환경·egress·목적지)의 지문에 묶인다. 하나라도 바뀌면 옛 ID는 즉시 효력을 잃고 새 활성화가 새 ID를 발급한다. 같은 집합에 합류하거나 재시작하면 기존 ID를 읽는다. 설정을 과거 판본으로 되돌려도 철회된 ID는 살아나지 않고 새 ID가 나온다. 선언을 지우면 철회한다. **한계:** 목적지를 바꿀 때는 자격 이름을 바꿔 새 승인을 받는다 — 같은 이름의 값 교체는 감지하지 않는다(지문에는 자격 이름만 들어가고 값은 보지 않는다). 값은 비밀 보관소에 쓸 수 있는 운영자만 바꿀 수 있어 이 경계 밖이다.
- 이 승인은 접수한 그 발생의 직접 호출 하나에만 쓴다. 모델·위임한 자식·wake·사용자 예약·다른 트리거·이벤트 본문 속 트리거 ID는 물려받지 못한다. 호출을 시작할 때와 새 쓰기 허가마다 활성 집합·승인 ID·정지·권한·claim을 다시 대조한다.
- 승인 호출의 **모든 쓰기**는 `effect.request{step,args:{operation,target,params}}`를 쓴다. `(operation, target)`이 허용 쌍과 정확히 같아야 하고 `params`(객체)에 자격을 뺀 쓰기 내용이 든다. 새 외부 쓰기(새 `send:true`)를 허가할 때마다 UTC 하루 한도를 한 번 쓴다. 읽기 점검·재전달·상태 조회는 0회, 운영자 미실행 판정 뒤 다시 보내기는 1회다. 한도·정지·허용 쌍 밖은 `denied`, 철회·교체·옛 정지 세대는 `stale`, 모양이 다르면 `invalid`, 같은 단계의 다른 지문은 `conflict`이고 어느 쪽도 발송은 0이다. 자정에 밀린 쓰기는 자동으로 다시 보내지 않는다.
- 트리거는 관리 화면·API로 정지·재개한다([관리 API](admin.md#사전-승인-관리)). 정지는 재기동·재배포 뒤에도 이어진다. 발생은 **예정 시각**으로 판정한다: 마지막 정지·재개 전환보다 앞서 예정된 발생(정지 중에 예정됐거나, 밀려서 재개 뒤에 접수되는 발생)은 건너뛰고(감사 한 줄) 재개 뒤에 예정된 새 발생부터 받는다. 재개해도 정지 전에 시작한 호출의 새 허가는 되살아나지 않는다.

지정한 사람의 메시지나 반응이 승인 한 단계를 대신하게 하려면 직접 도구 이벤트 트리거에 `human_approval`을 둔다.

```yaml
- id: confirm
  "on": {binding: team, event: reaction}
  match: {body.reaction: white_check_mark}
  input: {item: body.item_ts}
  target: {tool: reports.notify}
  human_approval:
    approvers: ["slack:work/U123"]
```

- `human_approval`은 멘션·DM 메시지(`on.event: message`), 봇이 참여한 스레드의 답글(`thread-reply`), 반응(`reaction`, 행위자는 반응한 사람) 트리거에 쓴다. 아무 이벤트나 승인이 되지 않도록 **정확한 확인 문구나 이모지를 `match`로 고정해야 한다**: 메시지·답글은 `match: {body.text: <문구>}`, 반응은 `match: {body.reaction: <이모지>}`(비어 있지 않은 문자열). 없거나 다른 필드만 고정하면 `invalid_human_approval`이다.
- `approvers`는 이 바인딩 워크스페이스의 Slack 사람 신원이며 프로필 멤버여야 한다(중복·서비스·HTTP 키·로컬 신원·메시지 작성 대상은 안 된다). `invalid_human_approval`로 거절한다.
- 입구가 인증한 **이번** 이벤트의 행위자만 증거가 된다. 증거는 접수와 같은 커밋에 첫 후보의 트리거·바인딩·이벤트 ID와 함께 run에 묶이고, 호출을 시작하는 커밋에서 승인 소비와 함께 정확히 한 번 쓴다(증거 ID·원본 이벤트·주체·도구·인자 지문이 같은 커밋에 남는다). 같은 이벤트를 다른 후보·run·재전달이 다시 쓸 수 없다. 지금·고정 판본이 모두 그 사람을 지정하고 멤버여야 하며, 어긋나면 증거는 쓰이지 않고(`human_approval_mismatch`) 기존 카드 규칙을 따른다.
- 모델·위임·넘김(PASS)으로 이어진 다른 후보는 이 증거를 쓰지 못한다. 민감 회상 뒤 확인·private 트리거·소유자 전용 승인 같은 독립적인 요건은 대신하지 않는다.

## 검증과 변경

스키마 위치는 `schemas/v1/{profile,agent,skill,tools,triggers,env}.schema.json`이다. `bot validate`와 `bot serve`가 같은 검증기를 쓴다. 미지원 버전·미등록 서버·알 수 없는 필드·중복 키·잘못된 참조·폴더 밖 경로·링크·특수 파일·과대 입력을 거부한다. 프로필 트리는 운영자가 배포하는 읽기 전용 입력이다.

검증한 바이트로 새 판본을 만들고 모든 바인딩을 한 번에 교체한다. 실패하면 이전 판본을 사용한다. 실행은 접수 시점의 지침·도구 판본에 묶이며, 이후 호출·승인·게시 직전에는 현재 권한과의 교집합을 적용한다. 재기동 시 옛 판본을 읽는 데에는 아래 제약이 있다. 철회된 서버나 바인딩으로 재개할 수 없다.

### 재배포 뒤 판본 복원 계약

**동작.** 첫 기동, 같은 집합에 합류하는 기동, `--activate` 교체 모두 역할을 열기 전에 아래 계약대로 번들을 보관하고 옛 판본을 복원한다. 복원할 수 없으면 시작하지 않으며 활성 집합·카드·시도 횟수는 그대로다. 실제 PG와 여러 OS 프로세스 시험(`serve_restore`·`serve_restore_bundles`·`serve_restore_set`·`claude_route_gone`)이 이 계약과 [수락 기준](reliability.md#재배포-복원-수락-기준)을 확인한다.

**복원 계약**은 다음과 같다.

프로필을 게시할 때 검증한 원본 번들을 PostgreSQL에 `(프로필 ID, 판본 SHA-256)`으로 보관한다. 새 활성 집합이 보일 때는 그 집합의 모든 번들이 이미 저장되어 있어야 한다. 같은 키로 같은 바이트를 다시 보관할 수 있다. 바이트가 다르면 교체를 거절한다.

**복원 범위는 이번 기동이 사용할 활성 집합의 프로필 ID로 정한다.** 첫 게시와 `--activate` 교체에서는 게시할 집합, 같은 집합에 합류할 때는 저장소에서 확인한 집합을 기준으로 삼는다. 역할을 열기 전에 그 프로필들이 참조하는 다음 판본을 복원한다.

- 각 프로필이 게시할 판본과 끝나지 않은 실행, 운영자가 재개할 수 있는 dead letter의 고정 판본. 대기 중이거나 승인 후 미소비인 카드도 해당 실행의 판본을 쓴다.
- 정적 cron의 열린 구간, 그리고 닫혔어도 커서가 구간 끝까지 이르지 않은 구간의 판본. 발생이 이미 접수됐다면 해당 실행이 참조를 이어받는다.
- 활성 사용자 예약이 참조하는 생성 실행의 판본. 새로 발화하는 실행의 판본은 아래 표대로 발화 때 정한다.

필수 판본은 게시할 프로필별 판본과 해당 프로필에 남은 작업의 고정 판본을 합쳐 구한다. 교체 전 판본을 참조하는 작업이나 예약 구간이 없으면 그 판본의 번들은 필요하지 않다. 준비가 끝나도 옛 워커가 새 실행을 접수할 수 있다. 저장소는 게시를 확정하는 트랜잭션에서 활성 집합을 잠근 뒤 참조를 다시 읽고, 필수 판본 모두가 이미 검증·보관됐는지 확인한다. 아직 검증하지 않은 필수 판본이 추가됐다면 교체를 보류하고 다시 준비한다. 같은 집합에 합류할 때도(처음부터 같은 집합이 게시돼 있었거나, 게시하려다 다른 프로세스가 같은 집합을 먼저 게시해 충돌한 경우) 역할을 열기 전에 현재 집합과 필수 판본을 확인한다. 준비한 목록에 없는 판본이 있으면 다시 준비해 복원한 뒤에만 열고, 복원하지 못하면 시작하지 않는다. 게시가 보인 뒤에는 옛 판본으로 새 실행을 접수하는 쪽이 `Stale`로 거절되므로 필수 판본은 더 늘지 않는다.

새 프로세스는 번들의 구조·프로필 ID·해시를 검증한 뒤 `Registry::adopt`로 등록하고, 복원한 스냅샷의 강한 참조를 프로세스 종료까지 유지한다. 복원한 옛 판본은 `lookup`으로만 조회할 수 있다. 다음 기동은 그때 필요한 판본만 메모리에 올린다. v2.0은 DB 번들을 자동 삭제하지 않는다.

프로필 전체가 집합에서 빠지면 그 프로필의 미소진 구간·실행·카드를 따라가 번들을 복원하지 않는다. 그 번들이 없어도 나머지 프로필은 시작할 수 있다. 저장된 기록은 보존되며, 제외한 프로필의 옛 권한으로 모델·도구를 실행하거나 승인을 소비하거나 회신을 발송할 수 없다. 프로필은 남고 정적 트리거만 삭제한 경우에는 전환 전 미소진 구간이 계속 복원 대상이다.

같은 프로필 ID를 다시 등록하면 그 시점에 남아 있는 참조로 필수 판본을 다시 계산한다. 재등록 자체가 끝난 실행이나 무효화된 카드를 다시 열지는 않는다. 필수 번들이 없거나 손상됐으면 프로필 ID와 SHA-256 전체 64자리를 알려 기동·교체를 중단한다. 기존 활성 집합, 카드 상태, 실행 시도 횟수는 보존한다.

**구버전 DB의 누락 번들 보충.** `bot profile-bundle put <프로필 폴더>`는 폴더명과 프로필 ID, 파일 구조와 크기 한도를 검증한다. 파일을 경로순으로 인코딩한 기존 정규 번들의 SHA-256을 계산해 `BOT_DATABASE_URL`이 가리키는 DB에 보관하고, 프로필 ID와 전체 해시를 출력한다. 이 작업은 호스트 등록부나 모델·도구 자격 없이 실행한다.

기동 오류에 표시된 ID·해시와 보충 결과가 일치하면 원래 기동 명령을 다시 실행한다. 같은 원본을 여러 번 보관해도 번들은 하나다. 다른 판본의 폴더는 그 판본으로만 보관된다. 기존 키의 저장 바이트와 충돌하면 덮어쓰기 없이 실패한다. 보충 자체로는 활성 판본이나 실행·승인 상태가 바뀌지 않는다.

| 대상 | 실행과 재판정 |
|---|---|
| 접수된 실행·재개 | 접수 때 고정한 지침·도구 선언을 읽고 현재 권한과 교집합을 취한다. 고정 판본에 애초에 없는 도구는 `unknown_tool`, 당시 존재했으나 현재 철회된 도구는 `denied`이며 실제 실행은 0회다. |
| 정적 cron | 예정 시각이 속한 구간의 판본·대상으로 접수한다. 재시작 중 놓친 발생도 같은 규칙을 따르고, 발화가 겹치면 기존 발생 키로 수렴한다. |
| 사용자 예약·후속 확인 | 발화 시 현재 멤버·바인딩·수신 경로와 생성 당시의 권한 상한을 확인한다. 새 실행은 그때의 프로필 판본을 고정한다. |
| 대기 승인 | 원래 호출·인자에 대해 현재 권한과 카드의 승인자·위험·정책·도구 및 설치 판본을 다시 대조한다. 조건이 같으면 같은 카드를 한 번 소비한다. 철회된 도구의 옛 승인 클릭은 `stale`이다. 전송 카드의 수신자가 사라진 경우는 기존 미발송 규칙에 따라 원래 대화에 안내 한 건을 남긴다. |

**조회 계약.** bot_core::profile::ProfileCatalog는 같은 활성화 시점의 전체 맵(active), 현재 프로필(current), 지정 판본(lookup)을 제공한다. bot-profile이 검증한 스냅샷을 만들고 다른 역할은 Arc로 읽는다. ProfileSource::validate는 BOT_PROFILES 전체 집합의 판본을 돌려주며 Accept.revision에는 해당 프로필 하나의 판본을 기록한다.

프로필 판본은 정렬한 상대 경로와 검증한 파일 바이트를 길이 구분해 SHA-256으로 계산한다. 정책 판본은 roles·members·policy와 승인자 매핑, 도구 판본은 서버 별칭을 포함한 정규화된 도구 선언에 묶는다. bot-profile은 원본 검증 바이트 묶음을 만들고 복원 때 구조·해시·프로필 ID를 재확인한다. 호스트 등록부는 활성화 때 대조하고, 옛 판본 실행도 현재 권한과 현재 설치 이미지·모델·자격·네트워크 상한 검사를 거친다. 승인 후 설치 이미지가 바뀌면 이전 승인을 무효화한다.

cron 전용 `beat: true`는 상태 확인 신호의 중복을 합친다. 생략하면 모든 발생을 개별 처리한다. 호스트는 프로필 활성 집합의 판본을 저장소에 원자적으로 게시한다. 시작 중이거나 호스트 설정이 반영되지 않은 동안 해당 입구는 접수를 보류한다.

BOT_HOST_CONFIG는 CLI가 읽는 JSON이다. gateway 키는 게이트웨이가 소유하며 CLI는 알 수 없는 최상위 키를 거절한다. 프로필 멤버 신원은 `local:<account>`, `slack:<workspace-alias>/<user-id>`, `http:<endpoint-alias>/<sub>` 또는 예약 전용 `service:<name>`이다. local·slack만 사람 승인자가 될 수 있다. Slack 사용자 ID는 U 또는 W 뒤에 대문자·숫자가 하나 이상 붙고 전체 길이는 최대 32자다. http·service는 서비스 주체이며 미지원 형식은 거절한다. 네임스페이스와 별칭은 인증된 입구에서 고정한다.

개발용 env 보관소는 두 ID의 소문자를 유지하고 각 하이픈을 밑줄로 바꾼다. personal의 calendar-reader는 `BOT_SECRET__personal__calendar_reader`다. 각 조각에는 단일 밑줄만 생기므로 이중 밑줄은 항상 두 ID의 경계다. 진단에는 값이 포함되지 않는다.

## bot-profile 검증 규칙

**폴더.** BOT_PROFILES 바로 아래에는 프로필 폴더만 둔다(루트 자체는 링크여도 된다). 트리 안의 링크·소켓·FIFO·장치·하드링크 파일, UTF-8이 아니거나 제어·방향 전환 문자가 든 이름과 내용, 대소문자만 다른 이름을 거절한다. 알려진 이름만 받는다. 프로필은 profile.yaml·rules.md·agents·knowledge·skills, 에이전트는 AGENT.md·env.yaml·triggers.yaml·memory·knowledge·skills·agents, 스킬은 SKILL.md·tools.yaml·tools.lock.yaml(원격 서버의 도구 정의 잠금)이다. knowledge는 `.md`·`.txt`, memory는 `.md`만 둔다. `.git`·`.DS_Store`·`.gitkeep`은 읽지도 해시하지도 않는다. 기본 한도는 프로필 64개, 프로필당 파일 2,000개·16 MiB, 파일 1 MiB(YAML 256 KiB), 경로 깊이 24, 에이전트 128개·8단계, 에이전트별 보이는 스킬 본문 합계 32 KiB(front matter 제외)다. 한도를 넘으면 일부만 받지 않고 전체를 거절한다.

**YAML과 스키마.** 앵커·별칭·태그·다중 문서·중복 키·복합 키·무한대·NaN, 깊이 32·항목 2만 개 초과를 거절한다. 평문은 YAML 1.2 코어 스키마로 읽어 `yes/no/on/off`는 문자열이고 키는 글자 그대로 문자열이다. 번들 스키마로 검증한다. cron은 5칸 표현과 `@hourly`·`@daily`·`@midnight`·`@weekly`·`@monthly`·`@yearly`·`@annually`, 일·요일 칸의 `?`를 받으며 `bot_core::cron::Cron`(검증과 스케줄러 공용)이 정규 5칸으로 바꾼다. AGENT.md의 front matter는 [agent.schema.json](../schemas/v1/agent.schema.json)의 `model`, `description`(한 줄 200자 이하, 상속하지 않으며 생략하면 경로의 마지막 이름), `builtin`(모델 실행기의 읽기 전용 내장 도구 `read`·`grep`·`glob`, 기본은 빈 목록, 상속하지 않으며 중복·미등록은 거절)만 받는다. SKILL.md의 선택적 front matter는 [skill.schema.json](../schemas/v1/skill.schema.json)의 `name`·`description`·`license`·`compatibility`·`metadata`만 받으며, `allowed-tools`·미지 키·미닫힘 `---`·폴더명과 다른 `name`·빈 본문·에이전트별 본문 32 KiB 초과를 거절한다. 도구 input은 [plugins.md](plugins.md)의 어휘만 받으며, pattern은 `\d \w \s \b`·인라인 플래그·lookaround 없이 엔진마다 같게 읽히는 문법만 쓴다. 쓸 수 없는 키워드·패턴은 활성화를 거절한다.

| SKILL.md 검증 오류 코드 | 원인 |
|---|---|
| `front-matter-unclosed` | 맨 위 `---`로 연 front matter에 닫는 `---` 줄이 없음 |
| `skill-allowed-tools` | front matter에 `allowed-tools`를 적음(권한은 `tools.yaml`과 `profile.yaml` 정책에서만 선언) |
| `skill-name-mismatch` | front matter의 `name`이 스킬 폴더 이름과 다름 |
| `skill-too-large` | 단일 SKILL.md 본문 또는 에이전트별 보이는 스킬 본문 합계(front matter 제외)가 32 KiB 초과 |
| `empty-file` | front matter를 뺀 SKILL.md 본문이 비어 있음 |
| `schema` | 알 수 없는 키, `name` 문자·길이 규칙 위반, `description`(1~1024자)·`compatibility`(1~500자)·`license`·`metadata` 형식 위반 |

**참조와 충돌.** 활성화 전에 다음을 거절한다: id≠폴더명, 대소문자까지 정확하지 않은 IANA 시간대, 알 수 없는 역할·스코프(특히 `policy.approve` 오타), 같은 신원·바인딩 id·입구(활성 프로필 전체), 없는 에이전트·바인딩·도구, 호스트 내장 도구 ID `memory.share`·`schedule.create`·`schedule.list`·`schedule.cancel`·`schedule.deliver` 선언(정확히 이 ID들만 예약하며 `memory`·`schedule` 스킬의 다른 도구나 다른 스킬의 `share`·`create`는 허용, 새·복원 프로필 공통), 도구 `env` 이름이 그 도구를 부르는 에이전트의 상속 env.yaml에 없음(`env-missing`, 부모·자식 모두 호출할 수 있으면 둘 다 본다)·로더·셸·프록시·호스트 예약 이름(`env-reserved`)·비밀 환경 이름과 충돌(`env-secret-conflict`)·중복이나 잘못된 이름(스키마), 보이지 않는 스킬 이름 충돌, profile.yaml에 없는 비밀 이름, 기억 시드 owner가 멤버가 아님, cron 주체가 서비스가 아니거나 도구 스코프가 없음, 승인 대상 도구의 cron에 approver가 없음(승인자는 `local:*`·`slack:*` 사람 신원만, `service:*`·`http:*`는 불가), 같은 후보군의 같은 order, private 트리거가 DM 바인딩이 아니거나 일반 트리거보다 뒤이거나 승인 대상 도구 또는 다단계(`effects: true`) 도구를 부름(승인 카드·단계 영수증에 민감한 본문이 남음), `input`과 고정 args의 키 충돌·스키마에 없는 인자·빠진 필수 인자·args 단독 호출의 스키마 위반, `sync` 응답인데 도구 대상이 아님. 호스트 등록부 대조(미등록 서버·모델·입구 별칭, egress 상한, 도구 호출 시간 한도의 천장 `tool-timeout-over-cap`)는 활성화 때만 한다. 같은 바이트의 판본은 호스트가 바뀌어도 같고, 옛 판본 복원은 구조·참조만 다시 확인한다.

**권한 계산.** 멤버의 스코프는 역할 스코프의 합집합 ∩ `policy.scopes`, 실행의 스코프는 그 값 ∩ 에이전트의 `scopes`(부모 상한 ∩ `policy.agents`)다. 도구는 필요한 스코프 전체가 들어 있어야 한다. 승인이 필요한 도구는 irreversible, 스코프가 `policy.approve`에 있는 write다. 수신자를 인자로 정하는 `destination`은 스냅샷의 `Tool.class`가 이미 irreversible로 올라간 값이며(선언 등급은 도구 판본 해시에 남는다), reply 목적지는 선언 그대로다. 기억 시드의 등록 해시(sha256)는 CLI가 owner·visibility·본문을 길이 구분한 UTF-8로 계산해야 소유자 변경이 반영된다.

cron 문법과 날짜·시각 일치 판정은 bot-core의 단일 파서를 사용한다. 판본 검증과 예약 실행 모두 5필드를 기준으로 하며, 기존 예약의 @hourly·@daily·@midnight·@weekly·@monthly·@yearly·@annually와 일/요일 칸의 단독 ?를 같은 5필드 표현으로 정규화한다. 시간대 변환·DST·다음 발생 탐색은 런타임이 담당한다.

기억 시드의 내용 해시는 owner, visibility(소문자 private 또는 profile), 검증된 본문 순서로 각 UTF-8 값의 바이트 길이(u64 big-endian)와 바이트를 이어 SHA-256으로 계산한다. 상대 경로·에이전트·프로필은 별도 시드 식별자다. 같은 내용의 재적용은 멱등이며, owner·visibility만 달라져도 변경을 식별한다. 기존 운영 기억의 소유권과 공유 승인은 시드 해시만으로 바꿀 수 없다.

기억 시드 본문은 UTF-8 기준 8,192바이트까지 허용한다. `bot validate`와 저장소는 같은 규칙을 적용한다. 새 판본과 시드는 함께 게시되므로 시드 하나가 실패하면 이전 판본과 기억을 유지한다.

### 도구에 전달할 일반 설정

에이전트의 `env.yaml`에 `CALENDAR_NAME: personal`을 쓰고, 해당 도구의 `tools.yaml` 항목에 `env: [CALENDAR_NAME]`을 쓴다. 기본은 빈 목록이다. 각 에이전트에서 상속을 마친 뒤 모든 이름이 존재해야 한다. 중복, 비밀의 환경 이름과 충돌, 로더·셸·프록시·호스트 예약 이름은 검증 오류다. 이름 목록은 도구 판본에 포함한다. 브로커는 고정 판본과 현재 판본에서 선언한 이름·선택한 값이 같은지 확인한다. 다르면 이전 실행의 도구 시작을 거절한다.

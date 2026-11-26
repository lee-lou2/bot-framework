# 첫 도구 만들기

스킬은 지침(`SKILL.md`)만으로 동작할 수도 있고, 외부 작업이 필요할 때 MCP 서버에 연결한 도구(`tools.yaml`)를 붙일 수 있다. 도구 실행 코드는 프로필 폴더 밖에 독립 MCP stdio 서버로 설치하고, 프로필에는 선언과 권한 상한만 둔다.

`bot add tool`은 스킬 폴더의 `tools.yaml` 항목만 만들고 권한(`profile.yaml`)과 호스트 설정(`host.json`)은 자동으로 바꾸지 않는다. 이 문서는 첫 읽기 도구를 붙이고 검증한 뒤, 사람 승인이 필요한 쓰기 도구로 확장하는 전체 순서를 다룬다. 완성된 참조 구현은 [demo-agenda](../examples/servers/demo-agenda/bin/server)와 [일정 스킬 선언](../examples/profiles/personal/agents/main/skills/agenda/tools.yaml)에 있다.

## 네 곳 체크리스트

첫 도구가 `bot validate`를 통과하고 실행되려면 아래 네 곳을 맞춰야 한다.

| 순서 | 위치 | 맞추는 것 |
|---|---|---|
| 1 | 설치 폴더 + `host.json` (`servers.<별칭>`) | 실행 가능한 MCP stdio 서버 파일(`chmod +x`), 절대 경로 `root`, 실행 인자 `command` |
| 2 | 스킬 폴더 `tools.yaml` | `bot add tool <스킬>.<도구>`로 선언한 `server`, `id`, `class`, `scopes`, `input`(원격 HTTP 서버면 `tools.lock.yaml`) |
| 3 | `profile.yaml` (`roles` · `policy`) | 역할(`roles`)에 스코프 추가, 전체 상한(`policy.scopes`)과 하위 에이전트 상한(`policy.agents`) 허용, 필요 시 `policy.approve` 지정 |
| 4 | (선택) `env.yaml` · `secrets` · `egress` | 비비밀 설정(`env.yaml` + 도구 `env`), 자격 이름(`profile.yaml` `secrets` + 도구 `secrets` + 비밀 보관소), 외부 접속(`egress`) |

## 1단계: 가장 작은 MCP stdio 서버 만들기

호스트는 `servers.<별칭>.root`를 읽기 전용 설치본으로 복사해 SHA-256 판본을 계산하고, 호출마다 빈 작업 폴더에서 새 프로세스로 실행한다. 아래는 Python 3 표준 라이브러리만으로 `initialize`·`ping`·`tools/list`·`tools/call`을 처리하는 최소 서버 예시다.

`/절대/경로/servers/notes/bin/server`로 저장하고 실행 권한(`chmod +x /절대/경로/servers/notes/bin/server`)을 준다.

```python
#!/usr/bin/env python3
"""가장 작은 MCP stdio 도구 서버 예시(Python 표준 라이브러리만 사용)."""
import json, sys

TOOLS = [
    {
        "name": "lookup",
        "description": "키로 고정 메모를 조회한다(읽기 전용 예시).",
        "inputSchema": {
            "type": "object",
            "required": ["key"],
            "properties": {"key": {"type": "string"}},
            "additionalProperties": False,
        },
    }
]
NOTES = {"status": "배포 준비 완료", "owner": "main"}

def send(mid, **body):
    line = json.dumps({"jsonrpc": "2.0", "id": mid, **body}, ensure_ascii=False) + "\n"
    sys.stdout.buffer.write(line.encode("utf-8"))
    sys.stdout.buffer.flush()

for raw in sys.stdin.buffer:
    line = raw.decode("utf-8", errors="replace").strip()
    if not line:
        continue
    msg = json.loads(line)
    if "id" not in msg:
        continue  # notifications/initialized 같은 알림에는 답하지 않는다
    mid, method, params = msg["id"], msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        send(mid, result={
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "notes", "version": "0.1.0"},
        })
    elif method == "ping":
        send(mid, result={})
    elif method == "tools/list":
        send(mid, result={"tools": TOOLS})
    elif method == "tools/call" and params.get("name") == "lookup":
        key = (params.get("arguments") or {}).get("key", "")
        payload = {"key": key, "value": NOTES.get(key, "없음")}
        send(mid, result={"content": [{"type": "text", "text": json.dumps(payload, ensure_ascii=False)}], "isError": False})
    else:
        send(mid, error={"code": -32601, "message": "method or tool not found"})
```

- `root` 안에는 링크나 특수 파일을 둘 수 없다. 비밀 값과 실행 데이터는 설치 폴더 밖에 둔다.
- 입력 크기 제한, `_meta`의 논리 호출 ID(`bot/profile-id`·`bot/run-id`·`bot/call-id`)로 재시도 안전한 영수증 만들기, 오류 응답 형식은 참조 구현 [demo-agenda](../examples/servers/demo-agenda/bin/server)를 참고한다.

## 2단계: 스킬과 `tools.yaml` 선언 만들기

`bot init my-bot`으로 만든 프로필 폴더에서 스킬을 먼저 만들고 `bot add tool`로 도구를 선언한다.

```sh
bot add skill notes --dir ./my-bot/profiles/my-bot
bot add tool notes.lookup --class read --server notes --dir ./my-bot/profiles/my-bot
```

생성된 `skills/notes/tools.yaml`에서 `input`을 서버의 `inputSchema`와 정확히 맞춘다(브로커가 실행 시 서버가 내세우는 스키마와 대조한다).

```yaml
version: 1
server: notes
tools:
  - id: lookup
    class: read
    scopes: [notes.lookup]
    secrets: {}
    egress: []
    input:
      type: object
      required: [key]
      properties: {key: {type: string}}
      additionalProperties: false
```

- 서버가 내세우는 도구 이름이 `get_note`처럼 snake_case라면 `name: get_note`를 함께 적는다([도구 정의 잠금](plugins.md#도구-정의-잠금-toolslockyaml)).
- 이미 운영 중인 원격 HTTP MCP 서버를 붙일 때는 `input`을 생략하고 `bot mcp sync <프로필>/skills/notes`로 `tools.lock.yaml`을 만든다.

## 3단계: `host.json`·`profile.yaml`·(선택) 환경과 비밀 연결

`bot add tool`이 출력한 안내대로 두 파일을 직접 고친다.

**`host.json` (`servers` 절 추가):**

```json
{
  "servers": {
    "notes": {
      "root": "/절대/경로/servers/notes",
      "command": ["bin/server", "--stdio"]
    }
  }
}
```

**`profile.yaml` (필요한 역할과 상한에만 스코프 추가):**

```yaml
roles:
  owner: [run, notes.lookup]
policy:
  scopes: [run, notes.lookup]
  # 특정 하위 에이전트의 스킬이라면 policy.agents의 해당 경로에도 스코프를 더한다
```

도구에 설정·자격·외부 접속이 필요하면 아래를 함께 맞춘다(필요 없으면 비워 둔다).

- **일반 설정**: 에이전트의 `env.yaml`에 `NOTES_NAMESPACE: team`을 쓰고, 도구 항목에 `env: [NOTES_NAMESPACE]`를 적는다.
- **비밀**: `profile.yaml`의 `secrets: [notes-api-key]`, 도구 항목의 `secrets: {NOTES_API_KEY: notes-api-key}`를 맞추고, 개발용 보관소면 환경 변수 `BOT_SECRET__my_bot__notes_api_key`에 값을 넣는다.
- **외부 접속**: 도구 항목에 `egress: ["api.example.com:443"]`을 적는다(호스트가 `egress` 상한을 두면 교집합만 허용).

모두 맞춘 뒤 DB·네트워크 접속 없이 검증한다.

```sh
export BOT_PROFILES="$PWD/my-bot/profiles"
export BOT_HOST_CONFIG="$PWD/my-bot/host.json"
bot validate
```

`검증 통과`와 함께 `서버 notes: 판본 <해시>` 줄이 나오고 경고가 0건이면 준비 완료다.

## 4단계: 승인이 필요한 쓰기 도구로 바꾸기

외부 상태를 바꾸는 도구는 위험 등급을 `write` 또는 `irreversible`로 나눈다.

```sh
bot add tool notes.save --class write --dir ./my-bot/profiles/my-bot
```

1. **서버 구현**: `TOOLS` 목록과 `tools/call` 분기에 `save`를 추가한다. `tools.yaml`의 `input`도 `save`의 `inputSchema`에 맞춘다.
2. **승인 규칙 선택**:
   - **`class: write` + `policy.approve`**: `profile.yaml`의 `roles`·`policy.scopes`에 `notes.save`를 더하고, **`policy.approve: [notes.save]`**에 넣으면 호출 전에 반드시 사람 승인 카드를 띄운다(`policy.approve`에 넣지 않으면 권한 있는 멤버가 바로 실행).
   - **`class: irreversible`**: `policy.approve`에 적지 않아도 **항상** 사람 승인을 거친다([demo-agenda의 `agenda.delete`](../examples/profiles/personal/agents/main/skills/agenda/tools.yaml)와 같다).
3. **여러 단계 쓰기라면**: 중단 뒤 중복 쓰기를 막아야 하는 도구는 `tools.yaml`에 `effects: true`를 선언하고 호스트의 `effect.request`/`effect.status` 콜백을 사용한다([선택적 콜백](plugins.md#여러-번-쓰는-작업의-선택적-콜백)).
4. **다시 검증**: `bot validate`를 실행해 스코프·승인 정책·서버 판본을 확인하고, 이미 실행 중인 서버에는 `bot serve --role all --activate`로 새 판본을 게시한다. 서버 코드가 바뀌면 설치 판본이 달라지므로 이전 승인 카드는 새 코드에 쓰이지 않는다.

더 자세한 MCP 프로토콜·원격 서버·잠금·콜백 규칙은 [MCP 도구 명세](plugins.md)와 [프로필 명세](profile-spec.md)에 있다.

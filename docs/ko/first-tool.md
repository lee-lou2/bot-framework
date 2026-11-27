# 첫 번째 도구 만들기

[English](../first-tool.md) | 한국어

스킬은 지침(`SKILL.md`)만으로 실행할 수도 있고, 외부 MCP 서버가 뒷받침하는 도구(`tools.yaml`)를 붙일 수도 있습니다. 도구 코드는 프로필 디렉터리 밖에 독립 실행형 MCP stdio 서버로 두고, 프로필에는 선언과 권한 상한만 적습니다.

`bot add tool`은 `tools.yaml`에 항목을 생성하거나 추가하며 `profile.yaml`이나 `host.json`은 자동으로 수정하지 않습니다. 이 가이드에서는 최소 읽기 전용 도구를 연결하고 검증한 뒤, 승인이 필요한 쓰기 도구로 확장합니다. 전체 참고 예제는 [`demo-agenda`](../../examples/servers/demo-agenda/bin/server)와 [`examples/profiles/personal/agents/main/skills/agenda/tools.yaml`](../../examples/profiles/personal/agents/main/skills/agenda/tools.yaml)을 확인하세요.

## 4단계 체크리스트

`bot validate`와 런타임 실행이 모두 성공하려면 다음 네 항목이 맞아떨어져야 합니다.

| 단계 | 위치 | 설정 내용 |
|---|---|---|
| 1 | 서버 디렉터리 + `host.json` (`servers.<alias>`) | 실행 가능한 MCP stdio 바이너리/스크립트(`chmod +x`), 절대 경로 `root`, `command` 인자 |
| 2 | 스킬 `tools.yaml` | `bot add tool <skill>.<tool>`로 `server`, `id`, `class`, `scopes`, `input` 선언(원격 HTTP는 `tools.lock.yaml`) |
| 3 | `profile.yaml` (`roles`, `policy`) | `roles`에 스코프 부여, `policy.scopes`와 `policy.agents`에 허용, 쓰기 승인이 필요하면 `policy.approve` 추가 |
| 4 | 선택(`env.yaml`, `secrets`, `egress`) | 비비밀 설정(`env.yaml` + 도구 `env`), 비밀 이름(`profile.yaml` `secrets` + 도구 `secrets` + 비밀 저장소), 외부 통신 `egress` |

## 1단계: 최소 MCP stdio 서버 만들기

기동 시 호스트는 `servers.<alias>.root`를 읽기 전용 설치 디렉터리로 복사하고 내용을 SHA-256 리비전으로 해시한 뒤, 매 호출을 깨끗한 빈 작업 공간에서 실행합니다. 아래 Python 3 표준 라이브러리 전용 서버를 `/absolute/path/servers/notes/bin/server`에 저장하고 `chmod +x /absolute/path/servers/notes/bin/server`를 실행하세요.

```python
#!/usr/bin/env python3
"""Minimal MCP stdio tool server (Python stdlib only)."""
import json, sys

TOOLS = [
    {
        "name": "lookup",
        "description": "Look up a fixed note by key (read-only example).",
        "inputSchema": {
            "type": "object",
            "required": ["key"],
            "properties": {"key": {"type": "string"}},
            "additionalProperties": False,
        },
    }
]
NOTES = {"status": "ready for deploy", "owner": "main"}

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
        continue  # Ignore notifications such as notifications/initialized
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
        payload = {"key": key, "value": NOTES.get(key, "not found")}
        send(mid, result={"content": [{"type": "text", "text": json.dumps(payload, ensure_ascii=False)}], "isError": False})
    else:
        send(mid, error={"code": -32601, "message": "method or tool not found"})
```

- `root` 안에는 심볼릭 링크, 특수 파일, 비밀 값, 런타임 데이터를 절대 두지 마세요.
- 줄 길이 제한, `_meta`(`bot/profile-id`, `bot/run-id`, `bot/call-id`) 기반의 결정적 재시도 안전 영수증, 오류 포맷팅 예시는 [`demo-agenda`](../../examples/servers/demo-agenda/bin/server)를 참고하세요.

## 2단계: 스킬과 `tools.yaml` 생성하기

`bot init`으로 작업 공간을 만들고 스킬을 추가한 뒤 도구를 선언합니다.

```sh
bot init my-bot
bot add skill notes --dir ./my-bot/profiles/my-bot
bot add tool notes.lookup --class read --server notes --dir ./my-bot/profiles/my-bot
```

`my-bot/profiles/my-bot/skills/notes/tools.yaml`의 `input`을 서버의 `inputSchema`와 정확히 일치하도록 수정합니다(브로커가 매 호출마다 이 스키마를 검증합니다).

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

- 서버가 `get_note`처럼 kebab-case가 아닌 이름을 노출하면 도구 항목에 `name: get_note`를 추가합니다([plugins.md](plugins.md#도구-정의-잠금-toolslockyaml)).
- 기존 원격 Streamable HTTP MCP 서버라면 `input`을 생략하고 `bot mcp sync <profile>/skills/notes`를 실행해 `tools.lock.yaml`을 생성하세요.

## 3단계: `host.json`, `profile.yaml`, 선택 설정 연결하기

`bot add tool`이 출력한 안내에 따라 `host.json`과 `profile.yaml`을 수정합니다.

**`my-bot/host.json`**(`servers` 추가):

```json
{
  "servers": {
    "notes": {
      "root": "/absolute/path/servers/notes",
      "command": ["bin/server", "--stdio"]
    }
  }
}
```

**`my-bot/profiles/my-bot/profile.yaml`**(기본 메모리 스코프에 `notes.lookup` 추가):

```yaml
roles:
  owner: [run, memory.search, memory.save, memory.forget, notes.lookup]
policy:
  scopes: [run, memory.search, memory.save, memory.forget, notes.lookup]
  # 하위 에이전트에 한정하려면 policy.agents.<path> 아래에도 notes.lookup 추가
```

도구가 설정값, 자격 증명, 외부 네트워크 접근을 필요로 한다면 사용하는 항목만 선언합니다.

- **비비밀 설정:** 에이전트의 `env.yaml`에 `NOTES_NAMESPACE: team`을, 도구 항목에 `env: [NOTES_NAMESPACE]`를 추가합니다.
- **비밀:** `profile.yaml`에 `secrets: [notes-api-key]`를 선언하고 `tools.yaml`에 `secrets: {NOTES_API_KEY: notes-api-key}`를 매핑한 뒤 비밀 저장소에 값을 저장합니다(개발용 `env` 저장소는 `BOT_SECRET__my_bot__notes_api_key`).
- **외부 통신:** 도구 항목에 `egress: ["api.example.com:443"]`을 명시합니다(호스트 `egress` 상한과 교집합 적용).

DB나 네트워크 연결 없이 오프라인으로 검증하고 `bot doctor`를 실행합니다.

```sh
export BOT_PROFILES="$PWD/my-bot/profiles"
export BOT_HOST_CONFIG="$PWD/my-bot/host.json"
bot validate
bot doctor
```

`bot validate`가 경고 없이 `Server notes: revision <hash>`와 `Entrance and secret checks passed`를 출력하면 성공입니다.

## 4단계: 승인 게이트가 있는 쓰기 도구로 확장하기

상태를 변경하는 도구는 `write` 또는 `irreversible`로 분류합니다.

```sh
bot add tool notes.save --class write --dir ./my-bot/profiles/my-bot
```

1. **도구 구현:** 서버의 `TOOLS`와 `tools/call`에 `save`를 추가하고, `tools.yaml`의 `input`을 해당 `inputSchema`와 맞춥니다.
2. **승인 규칙 선택:**
   - **`class: write` + `policy.approve`:** `roles`와 `policy.scopes`에 `notes.save`를 추가합니다. `policy.approve: [notes.save]`에 넣으면 매 호출 전 사람 승인 카드가 필요하고, 생략하면 권한 있는 멤버가 바로 실행할 수 있습니다.
   - **`class: irreversible`:** `policy.approve`에 없어도 항상 사람 승인이 필요합니다([`examples/profiles/personal/agents/main/skills/agenda/tools.yaml`](../../examples/profiles/personal/agents/main/skills/agenda/tools.yaml)의 `agenda.delete` 참고).
3. **다단계 쓰기:** 장애 후 중복되어서는 안 되는 외부 쓰기를 여러 번 수행한다면 `effects: true`를 선언하고 `effect.request`/`effect.status` 콜백을 사용하세요([plugins.md](plugins.md#다단계-쓰기를-위한-선택적-콜백)).
4. **재검증 및 활성화:** `bot validate`를 실행한 뒤 `bot serve --role all --activate`로 운영 배포에 반영합니다. 서버 코드가 바뀌면 설치 리비전도 변경되어 이전 승인 카드는 무효화됩니다.

전체 프로토콜, 원격 HTTP, 잠금 파일, 콜백 명세는 [plugins.md](plugins.md)와 [profile-spec.md](profile-spec.md)를 참고하세요.

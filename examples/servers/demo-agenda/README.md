# demo-agenda — 합성 일정 도구 서버(시뮬레이션)

README 5분 시작이 쓰는 예시 도구 서버다. [`examples/profiles/personal`](../../profiles/personal)의 `agenda` 스킬(`tools.yaml`의 `list`·`delete`)이 이 서버를 가리킨다. Python 표준 라이브러리만 쓰고 외부 접속·자격·파일 쓰기가 없다.

**시뮬레이션이다.** 설치본은 읽기 전용이고 호출마다 빈 작업 공간에서 새 프로세스로 돌기 때문에 아무것도 저장하지 못한다.

| 도구 | 하는 일 |
|---|---|
| `list` | 내장된 합성 일정(`e-walk` 산책 07:30 · `e-meet` 팀 회의 14:00 · `e-read` 독서 21:00)을 돌려준다. |
| `delete` | 위 합성 ID만 받는다(다른 ID는 오류). **가상 삭제 영수증**(`sim-…`)을 돌려주며 결과에 `"simulated": true, "persisted": false`가 들어 있다. **영구 삭제가 아니다** — `list`의 결과는 그대로다. 같은 논리 호출(`_meta`의 `bot/profile-id`·`bot/run-id`·`bot/call-id` 셋이 같음)이면 재전송·인계에도 영수증이 같고, call-id만 같고 run이나 프로필이 다르면 영수증이 다르다(`bot/generation`은 영수증에 넣지 않는다). |

입력 스키마는 `tools.yaml`의 `input`과 똑같고, `crates/bot-profile/tests/demo_server.rs`가 MCP로 직접 주고받으며 대조한다. 사용자에게 보이는 문구(CLI·모델의 최종 답)는 "삭제를 시뮬레이션했다"는 뜻으로 써야 하고 영구 삭제를 단정하면 안 된다.

## 설치와 실행

호스트 설정(`BOT_HOST_CONFIG`)의 `servers`에 이 폴더를 설치 위치로, `command`를 `["bin/server", "--stdio"]`로 적는다.

```json
{"version": 1, "servers": {"demo-agenda": {"root": "/path/to/examples/servers/demo-agenda", "command": ["bin/server", "--stdio"]}}}
```

직접 확인(줄 하나가 JSON-RPC 메시지 하나):

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delete","arguments":{"event":"e-walk"},"_meta":{"bot/profile-id":"personal","bot/run-id":"r-1","bot/call-id":"c-1"}}}' \
  | bin/server --stdio
```

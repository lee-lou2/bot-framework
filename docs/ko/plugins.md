# MCP 도구 플러그인과 내장 도구

[English](../plugins.md) | 한국어

운영자는 `host.json`(`BOT_HOST_CONFIG`)에 도구 서버를 등록하고, 프로필은 `tools.yaml`에서 서버 별칭으로만 참조합니다. 서버는 별도 프레임워크 라이브러리 없이 표준 MCP(`initialize`, `tools/list`, `tools/call`)를 구현하는 언어 무관 프로세스입니다. 최소 단계별 가이드는 [first-tool.md](first-tool.md)를, 프로필 스키마 규칙은 [profile-spec.md](profile-spec.md)를 참고하세요.

## 로컬 도구 서버 (MCP stdio)

`host.json`의 `servers` 아래에 로컬 서버를 선언합니다.

```json
{
  "servers": {
    "agenda": {
      "root": "/opt/bot/servers/agenda",
      "command": ["bin/server", "--stdio"]
    }
  }
}
```

- `root`는 코드만 담긴 절대 경로 디렉터리이며, `command[0]`은 `root` 내부의 실행 가능한 일반 파일 상대 경로입니다. 나머지 인자는 셸 확장 없이 그대로 전달됩니다. 심볼릭 링크와 특수 파일은 거부됩니다. 비밀과 런타임 데이터는 `root` 밖에 두세요.
- **콘텐츠 주소 지정 설치:** 기동 시 호스트는 `root`를 읽기 전용 설치 디렉터리로 복사하고 파일 내용, 상대 경로, 실행 비트, 디렉터리, `command`, `state`로부터 결정적 SHA-256 리비전을 계산합니다([`crates/bot-core/src/install.rs`](../../crates/bot-core/src/install.rs)). 승인 카드와 브로커 실행이 이 리비전을 고정하므로 코드가 바뀌면 이전 카드는 무효화됩니다. 역할 분리 배포에서는 결정 호스트와 실행 호스트에 동일한 패키지를 배포해야 합니다.
- **서버 상태(`state`):** 기본값은 `"ephemeral"`(호출마다 새 임시 디렉터리)입니다. 호출 간 파일 유지가 필요한 서버는 `"required"`로 설정합니다(설치 리비전에 포함). 현재 `srt`는 영구 볼륨 어댑터가 없어 기동 시 `state: "required"`를 거부합니다. 향후 어댑터는 `BOT_STATE_DIR`을 통해 전용 `0700` `(profile, server)` 볼륨을 노출하고(도구 `env`로 덮어쓰기 불가), 프로필·모델 간 교차 접근과 심볼릭 링크 탈출을 차단하며, 디스크 할당량과 호스트 간 프로세스 쓰기 잠금을 강제해야 합니다.
- **프라이빗 egress(`private_egress`):** 사설망 서비스를 호출하는 로컬 도구를 위해 운영자는 `host.json`에 프로필별 목적지를 고정할 수 있습니다(예: `{"private_egress":{"personal":{"calendar.internal:443":"10.0.0.8:443"}}}`). 도구 `egress` ∩ 프로필 상한 검증 후 샌드박스 프록시는 고정된 IP와 포트로만 연결합니다.

## 원격 도구 서버 (Streamable HTTP)

로컬 파일을 설치하지 않고 이미 실행 중인 MCP 서버에 Streamable HTTP로 연결합니다.

```json
{
  "servers": {
    "docs": {
      "transport": "http",
      "url": "https://mcp.example/api",
      "auth": {"kind": "bearer", "secret": "docs-token"}
    }
  }
}
```

- **URL 및 HTTP 규칙:** `https`가 필수이며, `http`는 정규 IPv4 루프백 리터럴(`127.0.0.0/8`)에서만 허용됩니다. IPv6 리터럴(`[::1]` 포함), `user:pass@`, `#` 프래그먼트, 잘못된 포트, 비정규 숫자 IPv4 표기(`0x7f000001`, `2130706433`, `0177.0.0.1`, `127.1`, 끝 마침표)는 거부합니다. 리다이렉트와 숨겨진 재시도 없이 응답 크기와 타임아웃을 제한합니다.
- **Bearer 인증(`auth`):** 선택 사항이며 호출 프로필의 비밀 저장소에 있는 비밀 이름을 지정합니다(`Authorization` 헤더에만 사용, 로그/감사 미기록). 서버 응답 어디에서든(원본 본문, 디코딩 텍스트, 키, 숫자 인코딩, `nextCursor`, 세션 ID) 비밀 값이 감지되면 거부됩니다(탐색/목록 조회는 `response_credential`, 도구 호출은 `result_credential`). 비밀이 없으면 호출이 차단됩니다.
- **Egress 및 DNS 고정:** `url`의 정확한 `host:port`가 도구 `egress` ∩ 프로필 상한에 포함되어야 합니다. 매 연결마다 모든 DNS 응답을 검증해 사설·루프백·링크로컬 IP를 거부하고(전역 사설망 설정으로 완화 불가) 검증된 IP로 직접 연결합니다. 모든 아웃바운드 단계(`server/discover`, `tools/list`, 레거시 `initialize`, `tools/call`, 세션 종료) 직전에 브로커는 프로세스의 활성 리비전을 재확인하며 오래되었거나 읽을 수 없으면 전송 전에 중단합니다.
- **프로토콜 협상:** MCP `2026-07-28`(읽기 전용 `server/discover`로 탐색, `x-mcp-header` 인자를 `Mcp-Param-*` 헤더로 반복 전송)과 레거시 `2025-11-25`, `2025-06-18`, `2025-03-26`(`initialize` -> `initialized` 및 세션 ID 사용, `2025-06-18`부터 `MCP-Protocol-Version` 전송)을 지원합니다. 버전 탐색이나 재시도에 쓰기 `tools/call`을 절대 사용하지 않습니다.
- **제약 및 실패 의미론:** 원격 도구는 `effects: true`, `secrets`, `env`를 선언할 수 없으며(활성화 시 거부), 아웃바운드 `_meta`에는 `bot/call-id`, `bot/profile-id`, `bot/run-id`만 실립니다(`bot/principal`, `bot/trigger`, `bot/conversation`, `bot/destination` 제외). JSON-RPC 요청 ID가 일치하는 `2xx` 응답만 확정 결과로 인정하며, 연결 끊김·타임아웃·비`2xx` 응답(`401`/`403`/`5xx` 포함)은 실행 결과를 불명확 상태로 남깁니다(쓰기는 보류, 읽기는 일시적 재시도). 설치 리비전은 `url`, `transport`, bearer 비밀 이름을 묶습니다.

### 도구 정의 잠금 (`tools.lock.yaml`)

서버가 kebab-case가 아닌 도구 이름(예: `get_forecast`)을 노출하면 `tools.yaml` 항목에 `name`을 지정합니다(`[A-Za-z0-9_.-]+`, <= 128자, 기본값은 `id`). 모델·스코프·승인은 `skill.id`를 사용하고, MCP `tools/list`, `tools/call`, `Mcp-Name`은 `name`을 사용합니다. 잠금 불일치는 `lock-name-mismatch`로 실패합니다.

`tools.yaml`에서 `input`을 생략하면 로더가 같은 폴더의 `tools.lock.yaml`에서 입력 스키마와 설명을 읽습니다(모델에 보이는 설명 우선순위: 잠금 설명 -> 스키마 `description` -> 도구 ID, 최대 4 KiB UTF-8; 인라인 `input`이 있으면 잠금을 무시; 둘 다 없으면 `missing-input`으로 실패). 배포 전에 동기화된 `description` 텍스트에 프롬프트 주입 지시가 없는지 반드시 검토하세요.

```sh
bot mcp sync <skill-dir-under-BOT_PROFILES>
```

- `bot validate`와 동일한 로더로 디렉터리 구조, 심볼릭 링크 부재, `profile.yaml`(`id == folder`), 대상 `SKILL.md`(프런트매터, 이름 일치, 단일 파일 64 KiB 상한)를 검증하며 해당 프로필의 비밀 저장소에서만 자격 증명을 읽습니다.
- 모든 `tools/list` 페이지를 가져와(최대 16페이지 / 4,096개 도구; 잘못되거나 반복되는 `nextCursor`, 조회 도중 `list_changed` 거부), `tools.yaml`에 선언된 도구만 필터링하고(누락·중복 서버 이름 실패), 수락 전 전체 응답에서 자격 증명 유출을 검사한 뒤, 프로필 어휘 기준으로 입력 스키마를 검증하고 결정적 `{id, name, description, input_schema, sha256}` 항목을 기록합니다. 여기서 `sha256`은 `{"name","description","inputSchema"}`의 RFC 8785 정규 JSON 다이제스트입니다.
- 현재 원격(`transport: "http"`) 서버를 지원합니다. 실행 후 `bot validate`로 전체 프로필 참조와 에이전트당 64 KiB 스킬 합계를 확인하세요.
- 실행 시점에 브로커는 HTTP와 stdio 서버 모두에서 호출 전 서버의 실시간 도구 정의 다이제스트를 잠금 파일과 비교합니다(`tool_definition_changed`, `tool_missing`).

## 도구 선언과 표준 호출 흐름

`tools.yaml`의 각 도구는 `id`, `class`(`read`, `write`, `irreversible`), `scopes`(>= 1)가 필수이며, `name`, `external`, `effects`, `env`, `secrets`, `egress`, `timeout_secs`, `destination`, `input`은 선택 사항입니다.

1. **인가 및 승인:** 브로커가 도구 이름, 리비전, 인자를 고정하고 스코프·위험 등급·속도 제한을 검사한 뒤, 필요한 사람 승인이 완료될 때까지 대기합니다.
2. **격리:** 선언된 `secrets`와 `env` 키(`env` 이름과 값이 고정 리비전 및 현재 리비전과 일치하는지 검증), 그리고 호스트가 주입한 프록시·콜백 URL만 담은 빈 환경에서 호출마다 새 샌드박스를 시작합니다.
3. **호출:** 협상된 MCP 버전과 도구 스키마를 검증한 뒤 호스트 소유의 `params._meta`(예: `{"bot/call-id":"c-17","bot/conversation":{"id":"owner","kind":"cli","thread":null},"bot/generation":4,"bot/principal":"local:owner","bot/profile-id":"personal","bot/run-id":"r-3","bot/trigger":"message"}`, `destination: "reply"` 도구는 `bot/destination` 포함)와 함께 `tools/call`을 전송합니다. `stdout`은 MCP, `stderr`는 크기 제한 및 마스킹이 적용되며 부모 FD는 상속되지 않습니다.
4. **기록 및 종료:** 논리적 호출 ID로 결과를 영속 저장하고 프로세스를 종료합니다. 다운스트림 멱등성 키는 `[profile, run, call(, step)]`을 해시합니다(`generation`과 claim 제외). 안전한 읽기 확인 없이 결과가 불명확하면 호출을 보류(hold)합니다.

- **식별자, 기원, 대화 위치:** `bot/principal`은 위임·승인·재개 전반에서 인증된 요청자(`local:<account>`, `slack:<ws>/<user>`, `http:<endpoint>/<sub>`, `service:<name>`)를 유지하며 모델이 `_meta`를 위조할 수 없습니다. `bot/trigger`는 근원 기원(`message` — 프라이빗 DM 포함 CLI/Slack 사람 입력, `http`, `schedule` — 정적 cron 또는 사용자 예약, `wake`, `unknown`)을 기록합니다. 사용자 답장을 기다리는 대화형 도구는 `message`만 허용해야 합니다. `bot/conversation`은 호스트가 검증한 읽기 전용 대화 위치(`{"id","kind","thread"}`)를 모든 로컬 stdio 도구(`read` 포함)에 전달하며, `destination: "reply"` 변경 도구에만 열리는 `bot/destination`과 분리됩니다.
- **타임아웃 및 한도:** 기본 타임아웃은 `60s`이며, `timeout_secs`는 호스트 `limits.tool_call_max_secs`(기본 `60`, 최대 `900`) 범위 내에서 `1..=900`을 허용합니다(초과 시 활성화는 `tool-timeout-over-cap`, 호출 전 차단은 `tool_timeout_over_cap`). 샌드박스 마감은 `max(300s, timeout + 240s)`, 프록시 터널 마감은 `600s`(`timeout_secs > 600`이면 그에 맞춰 상향)입니다. 콜백 프레임은 64 KiB 상한입니다. 전송 후 `timeout`은 불명확 상태입니다(쓰기는 자동 재시도 없이 보류; [reliability.md](reliability.md) 참고).
- **입력 스키마 어휘:** 지원 키워드는 `type`, `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`, `minItems`, `maxItems`, `minLength`, `maxLength`, `minimum`, `maximum`, `pattern`, `anyOf`, `oneOf`, `allOf`, `title`, `description`, `default` 및 비검증 주석(`$schema`, `$comment`, `examples`, `format`, `deprecated`, `readOnly`, `writeOnly`)입니다. 로컬 `$defs`/`definitions`와 `#/$defs/<name>`/`#/definitions/<name>` `$ref`는 컴파일 시 전개되며(퍼센트 디코딩 후 JSON Pointer `~1`/`~0` 적용), `$ref` 형제 키는 주석 키워드만 허용되고 재귀/외부/누락 참조나 전개 노드 20,000개 초과는 거부됩니다. `pattern`은 `\d`/`\w`/`\s`/`\b`, 인라인 플래그, lookaround가 없는 이식 가능한 Rust 정규식 문법을 사용합니다.
- **외부 전송 및 목적지 선언:** 검색을 포함해 제3자에게 입력을 보내는 모든 도구에는 `external: true`(기본 `false`)를 설정해야 합니다. 프라이빗 메모리를 회상한 뒤 `external` 도구를 호출하면 승인이 격상됩니다. `destination: "reply"`는 원래 대화를 `_meta["bot/destination"]`에 주입합니다. `destination: {recipient: "args.<path>", content: "args.<path>"}`는 인자로 지정된 수신자를 대상으로 하며 도구의 유효 등급을 자동으로 `irreversible`로 승격합니다.

## 다단계 쓰기를 위한 선택적 콜백

여러 외부 쓰기를 수행하는 도구는 `effects: true`를 선언해 `BOT_EFFECT_URL`(`http://bot-effects.invalid/effect`, 추가 `egress` 항목 없이 호출별 프록시가 로컬 처리; 지원 시 양방향 FD 위 NDJSON)에 단계 진행 상황을 기록합니다. 승인은 여전히 호출 경계에서 한 번만 이루어집니다.

```jsonl
{"v":1,"id":"0","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"}}
{"v":1,"id":"0","state":"none","send":false}
{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary","target":"inbox"}}
{"v":1,"id":"1","state":"started","effect":"e-17","send":true}
{"v":1,"id":"2","method":"effect.status","effect":"e-17","outcome":"confirmed","receipt":"file-17"}
{"v":1,"id":"2","state":"confirmed","receipt":"file-17","send":false}
```

- **`effect.request`:** 인자 다이제스트를 `(call_id, step)`에 묶습니다. 새 단계를 선점하거나 운영자의 `not_executed` 판정을 원자적으로 소비할 때만 `send:true`를 반환합니다. 기존 단계에 다른 인자를 보내면 `conflict`를 반환합니다.
- **사전 승인 호출:** `preapproved`가 설정된 정적 cron 도구([profile-spec.md](profile-spec.md#사전-승인-정적-호출과-지정-사람-승인))는 모든 쓰기에 `args: {"operation":"...","target":"...","params":{...}}`를 보내야 합니다. 새로운 `send:true` 부여는 허용된 `(operation, target)`과 정확히 일치해야 하며 UTC 일일 할당량 1을 소모합니다(범위/할당량 초과/일시 정지는 `denied`, 폐기/교체/이전 정지 세대는 `stale`, 형식 오류는 `invalid`, 유효 Grant 내 인자 불일치는 `conflict`).
- **`effect.status`:** `{step, args}` 조회는 단계를 선점하지 않으며(`none`, `started`, `held`, `rejected`, 또는 `receipt`가 포함된 `confirmed`; 항상 `send:false`), `effect` + `outcome`(`confirmed`/`rejected`/`held`)으로 조회/보고하면 최종 단계 상태를 기록합니다. 단계 조회와 `effect`/`outcome`을 섞으면 `invalid`를 반환합니다. `unavailable` 시 쓰지 말고 상태 조회만 재시도하세요.
- **자격 증명 마스킹 검사:** 영수증, 보류 사유, 단계 이름은 저장 전에 스캔됩니다. 단계 이름에서 자격 증명 유출이 감지되면 `invalid`를 반환하고, 영수증이나 보류 사유에서 감지되면 원본 텍스트를 저장하지 않은 채 `receipt_credential` 또는 `reason_credential`로 단계를 보류합니다.
- **운영자 `not_executed` 판정 이후 복구:** 상태 조회가 `resolution: "not_executed"`를 반환합니다(도구는 `resolution`을 보낼 수 없음).

```json
{"v":1,"id":"7","state":"held","effect":"e-17","resolution":"not_executed","send":false}
```

1. `effect.status{step, args}`를 호출합니다. `confirmed`면 `receipt`를 재사용하고, `rejected`면 종료하며, `started`나 미해결 `held`면 쓰지 않고 외부 시스템에서 확인합니다.
2. `none` 또는 `resolution: "not_executed"`이면 다운스트림 시스템에서 읽기 전용으로 확인합니다(이 지점에서 중단되어도 판정은 소비되지 않음).
3. 여전히 쓰기가 필요하면 `effect.request{step, args}`를 호출하고 **`send:true`일 때만** 쓴 뒤 결과를 보고합니다.

## 모델에 노출되는 내장 도구

프레임워크 예약 도구 ID 15개(`delegate`, `memory.share`, `memory.search`, `memory.save`, `memory.forget`, `schedule.create`, `schedule.list`, `schedule.cancel`, `schedule.deliver`, `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, `mcp.remove`)는 `tools.yaml`에 선언할 수 없습니다.

- `anthropic-api`, `openai-api` 및 VM CLI 러너는 에이전트 스코프 상한 내에 있을 때 `memory.share`, `memory.search`, `memory.save`, `memory.forget`, `schedule.create`, `schedule.list`, `schedule.cancel`을 노출하며, 직계 자식이 있는 에이전트에는 `delegate`(`{"agent":"main/research","instruction":"Summarize today's agenda"}`)를 추가로 노출합니다. `schedule.deliver`는 호스트가 전달 초안 승인용으로 내부 사용합니다.
- 프로필 `super_admins`가 해당 스코프를 보유하면 아래 6개 실시간 관리 도구도 함께 노출됩니다(비슈퍼관리자에게는 숨겨지며 직접 호출 시 `not_super_admin`으로 실패). VM CLI 러너(`claude-cli`, `codex-cli`, `agy-cli`)는 점 표기 이름을 밑줄(`memory_share`, `skill_create`)로 노출하며 API 러너는 요청별로 별칭을 매핑합니다.

| 슈퍼관리자 도구 | 등급 | 동작 |
|---|---|---|
| `skill.inspect` | `read` | 에이전트 트리, 설명, 스킬 바이트 예산(64 KiB 상한), 스킬 목록(또는 단일 스킬 본문), MCP 서버, 비밀 백엔드, 비밀 **이름**(값 제외)을 조회. |
| `skill.create` | `write` | 프로필 전체 또는 에이전트 범위 `SKILL.md`를 생성하고 검증한 뒤 실시간 활성화. |
| `skill.update` | `write` | 기존 스킬의 `description`이나 본문을 수정하고(`tools.yaml`/`tools.lock.yaml` 보존) 검증한 뒤 실시간 활성화. |
| `skill.delete` | `write` | 스킬 폴더를 삭제하고 미사용 도구 스코프를 정리한 뒤 검증 및 실시간 활성화. |
| `mcp.add` | `write` (`external`) | 원격 Streamable HTTP(`tools/list` 자동 잠금) 또는 로컬 stdio MCP 서버를 등록하고 비밀을 암호화 저장한 뒤 검증 및 실시간 활성화. |
| `mcp.remove` | `write` | MCP 서버와 해당 스킬 도구 바인딩, 미사용 스코프를 제거하고(`remove_secrets: true`면 미사용 암호화 비밀도 삭제) 검증 및 실시간 활성화. |

- 관리 변경은 원자적으로 처리됩니다. 검증, 설치, DB 오류가 발생하면 디스크 수정이 롤백됩니다. 성공 시 기존 `bot_grants`를 유지한 채 새 번들과 활성 리비전을 커밋하고 재시작 없이 프로세스 내 레지스트리·러너·샌드박스·결정 게이트를 핫 리로드합니다.
- 비밀 값(`secret_value`, `secret_values`)은 `Redactor`에 등록되고 SOPS(`sops set`) 또는 키체인(`bot/<profile>/<name>`)에 즉시 캐시 플러시와 함께 암호화 저장되며, 평문 파일이나 승인 카드에는 절대 기록되지 않습니다. `BOT_SECRETS=env`에서는 런타임 비밀 쓰기가 거부되므로(`secret_store_failed`), `BOT_SECRET__<PROFILE>__<NAME>`을 미리 설정하고 `secret_value`/`secret_values`를 생략하거나 `sops:<file>` / `keychain`을 사용해야 합니다.
- 브로커는 모든 내장 도구 호출마다 고정 리비전과 현재 권한을 검사합니다. `memory.share`는 소유자가 자신의 DM이나 `bot chat`에서 정확한 본문을 승인해야 합니다. `memory.search`는 턴 시작 회상과 동일한 가시성 및 `private_memory_read` 오염 규칙으로 하이브리드(임베딩 + 어휘) 회상을 수행합니다. `memory.save`(1:1 DM/CLI 비공개 전용, 소유자당 활성 500건 상한, 정확/근접 중복 자동 생략, 선택적 `kind: "core"|"fact"` 및 본인 비공개 항목에 대한 비파괴 `supersedes`)는 `policy.approve`에 `memory.save`가 포함된 경우에만 소유자 승인을 요구하며(승인 통과 후에만 임베딩 계산), 스코프가 허용된 경우 런타임이 간결한 `## Memory` 지침을 주입해 모델이 장기 사실을 능동적으로 저장하도록 돕고, `host.json`의 `memory_judge`를 설정하면 1:1 DM/CLI 사용자 턴의 저장·갱신·삭제 판단까지 자동화할 수 있습니다. `memory.forget`은 1:1 DM/CLI에서 본인 소유 기억 1건을 소프트 삭제합니다(본문과 임베딩 컬럼을 비우며, `policy.approve`에 `memory.forget`이 포함되면 소유자 승인을 요구). 확정된 거부는 호출 ID 기준으로 결정적으로 재생됩니다([reliability.md](reliability.md)).

### 예약과 후속 깨우기(Wake)

- **대화 예약:** `schedule.create`는 `instruction`, `at`(UTC RFC 3339, 최대 1,830일 이내) 또는 `cron`(5필드 + 선택적 IANA `timezone`), 선택적 `quiet`(기본 `false`; `<quiet>`를 반환한 조용한 실행은 회신 억제), 선택적 `destination: {binding, conversation: {kind, id, thread?}}`을 받습니다. `schedule.list`는 호출자의 활성 예약을 조회하고, `schedule.cancel`은 `{"id":"<schedule-id>"}`로 취소합니다.
- **수신자 규칙:** 기본값은 원래 대화입니다. `schedule.create` 권한이 있는 프로필 멤버는 같은 프로필에 등록된 다른 Slack 채널을 대상으로 지정할 수 있으며, 생성 시 소유자가 수신자와 지시를 승인하고 발화 시마다 `Delivery draft` 카드(`schedule.deliver`)를 승인해야 합니다. CLI 사용자는 현재 대화에만 예약할 수 있고, DM 바인딩은 임의 DM을 대상으로 할 수 없으며, HTTP 및 서비스 주체는 사용자 예약을 관리할 수 없습니다.
- **도구 개시 깨우기(`_meta["bot/wake"]`):** 성공한 비프라이빗 MCP 도구 결과에 `"_meta":{"bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"Check if files arrived","quiet":true}}`를 포함할 수 있습니다. `effects: true`, 도구 및 현재 요청자/에이전트의 `schedule.create` 스코프, 검증된 선행 단계 영수증이 필요합니다. 원래 대화로 깨우기 1건을 예약하고 모델 출력에서 `_meta["bot/wake"]`를 제거합니다. 외부 효과 이후 깨우기 생성이 실패하면 호출을 보류합니다.

### 처리하지 않은 이벤트 패스 (`bot/pass`)

인증된 Slack **직접 도구 트리거**에서는 도구가 이벤트를 넘겨 브로커가 다음 후보 트리거 또는 폴백 대화로 진행하게 할 수 있습니다(모델, 위임, HTTP, cron, wake 호출에서는 무시 및 제거됨).

```json
{"content":[],"isError":false,"_meta":{"bot/pass":true}}
```

- **엄격한 형식:** 최상위 `_meta["bot/pass"]: true`, 빈 `content`, `isError: false`가 필요하며 `structuredContent`나 추가 `_meta` 키(`bot/wake` 등)가 없어야 합니다. 여분의 필드가 있으면 다음으로 넘어가지 않고 `invalid_pass`로 후보를 종료합니다.
- **선행 부수 효과 금지:** 순수 읽기 이후에만 허용됩니다. 시작/보류/불명확 효과 단계, Outbox 회신, 대기 중인 승인이 하나라도 있으면 `pass_after_effect`로 종료합니다. `effects: true`가 없는 쓰기 등급 도구는 패스할 수 없으며, `effects: true` 도구는 기록된 쓰기가 0건일 때만 패스할 수 있습니다. 호스트는 효과 원장·Outbox·승인 기록으로만 쓰기를 감지하므로 **도구는 모든 외부 쓰기를 `effect.request`로 기록해야 하며 쓰기 후에는 절대 `bot/pass`를 반환해서는 안 됩니다.**
- **커밋 및 프라이버시:** PASS 확정, 호출 종결, 커서 전진, 감사 기록은 서로 다른 후보 호출 ID(`<run>:tool`, `<run>:tool:1`, ...)와 함께 원자적으로 커밋되며, 모든 출력에서 마커가 제거됩니다. `private: true` DM 트리거에서는 `private_pass: true`가 선언된 경우에만 유효한 빈 PASS가 원본 본문과 첨부 파일을 일반 DM 대화로 승격하며, 그렇지 않은 미허용 패스는 프라이빗 원장에 보류됩니다. 도구 오류, 승인 거부, 일시적 실패는 PASS가 아닙니다.

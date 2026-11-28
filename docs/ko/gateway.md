# 게이트웨이 운영

[English](../gateway.md) | 한국어

게이트웨이는 CLI, Slack, HTTP 요청을 인증하고 등록된 입구, 멤버, 트리거를 검증합니다. 주체(principal)와 응답 대화는 인증된 신원에서만 결정되며 요청 본문으로 덮어쓸 수 없습니다. 요청 ID는 PostgreSQL 접수(intake) 트랜잭션이 커밋된 뒤에만 반환되며, 동일 ID의 재전달은 같은 실행(run)으로 수렴합니다([신뢰성](reliability.md)).

푸시(push) 입구는 인증된 바인딩에서 응답 전달 어댑터(`transport`)를 고정합니다(Slack 채널과 DM은 `slack`을 공유). 아웃박스(outbox) 워커는 자신의 어댑터—분리 배포 시 담당 프로필/바인딩 범위 포함—와 일치하는 응답만 점유(claim)하며, 점유 후에도 현재 바인딩과 대상 권한을 재검증합니다. CLI와 HTTP는 아웃박스가 아닌 폴링으로 응답을 받습니다([신뢰성](reliability.md)).

## 호스트 레지스트리 (`BOT_HOST_CONFIG`, JSON)

`host.json`은 프로필 별칭을 OS 계정, 서비스, 실행 파일에 매핑합니다. `BOT_HOST_CONFIG`를 생략하면 예제 프로필용 내장 데모 기본값이 사용됩니다. 사용자 정의 프로필은 모델과 입구를 직접 등록해야 합니다([새 프로필의 호스트 설정](profile-spec.md#새-프로필의-호스트-설정)).

최상위 키: `version` (`1`), `gateway`, `servers`, `models`, `providers`, `egress`, `private_egress`, `super_admins`, `sandbox`, `limits`, `embeddings`, `memory_judge`, `admin`.

- `models` / `providers`: 프로필별 허용 모델 별칭을 `demo`, `claude-cli`, `codex-cli`, `agy-cli`, `anthropic-api`, `openai-api`에 바인딩합니다. 업스트림 엔드포인트는 자격 증명, 쿼리, 프래그먼트가 없는 `https://` URL이어야 합니다(직접 API 제공자는 루프백 IP 리터럴의 `http://`도 허용하며, 이 경우에만 `secret`을 생략할 수 있습니다).
- `egress` / `private_egress`: 프로필별 허용 외부 `host:port` 상한과 고정된 사설/로컬 대상 매핑입니다.
- `super_admins`: 프로필별 슈퍼 관리자 주체의 호스트 수준 상한입니다(비어 있지 않으면 활성화 지문에 포함).
- `servers`: 로컬 MCP 패키지(`root`, 명령) 또는 원격 HTTP 서버(`transport: "http"`, `url`)를 등록합니다. `bot validate`는 서버 파일과 명령에서 서버 리비전을 계산합니다. 도구 러너는 `BOT_INSTALLS` 아래 읽기 전용 설치본을 사용합니다. 기본 `state`는 `ephemeral`이며, `required` 서버는 현재 `srt` 호스트에서 시작할 수 없습니다([MCP 도구](plugins.md)).
- `sandbox` / `limits` / `embeddings` / `memory_judge` / `admin`: 모델 VM 및 샌드박스 설정(`instance`, `image`/`images`, `network` — [아키텍처](architecture.md) 참고), 도구 제한, 선택적 하이브리드 메모리 `/embeddings` 및 System One `memory_judge` 엔드포인트, Admin UI/관측성 설정(`bind`, `hosts`, `max_failures`, `trace`, `logs`, `issues` — [Admin UI](admin.md) 참고)입니다.

활성 세트(active set)는 프로필 리비전과 권한에 영향을 주는 호스트 설정(CLI 사용자 매핑, Slack 팀/채널, HTTP 인증, 허용 모델/업스트림/시크릿 이름, 서버 리비전, `egress`, `private_egress`, `super_admins`, VM `image`/`images`/네트워크, 비기본값 `limits.tool_call_max_secs`, 설정된 `embeddings`/`memory_judge`, 활성화된 `admin.trace`/`logs`/`issues`)을 함께 묶습니다. 시크릿 값은 절대 포함되지 않습니다. 호스트 매핑 변경을 적용하려면 `bot serve --activate`를 실행합니다.

### `gateway` 키

| 키 | 계약 |
|---|---|
| `cli.accounts.<alias>` | `{"os_user": "name"}` 또는 `{"uid": 501}` 중 정확히 하나만 지정합니다. `cli.socket`은 100바이트 이하의 절대 경로여야 합니다. |
| `slack.workspaces.<alias>` | `team_id`, 선택적 `app_id`(Socket Mode hello 시 검증), `channels`(별칭 → 채널 ID; `D…` DM ID는 금지), `dm`(기본 `false`), `receive`, 시크릿 **이름** `bot_token`, `app_token`, `signing_secret`. |
| `slack.dm_sent_notice` | 소유자 DM 승인 카드 게시 후 원 채널에 남기는 기본 `"Approval request sent via DM"` 안내를 대체하는 선택적 한 줄 문구(200자 이하, 제어 문자 금지, 활성화 지문 제외). |
| `http.listen`, `http.remote` | 기본값 `127.0.0.1:8788`(Admin UI의 `127.0.0.1:8787`과 구분). 평문 HTTP이므로 비루프백 바인딩은 TLS 프록시 뒤에서 명시적 `http.remote: true`(기본 `false`)가 필요하며, 그렇지 않으면 `bot validate`와 `bot serve`가 `gateway.http.listen`을 거부하고 아무 포트도 열지 않습니다. |
| `http.endpoints.<alias>` (웹훅) | `kind: "webhook"`(또는 생략): `issuer`, `audience`, `key`(HS256 시크릿 이름), `sync_wait_ms`(기본 10초, 1–60000 ms), `max_token_age_secs`(≥ 1). |
| `http.endpoints.<alias>` (OpenAI) | `kind: "openai"`: `keys`(슬롯 → `{secret, subject}` 매핑 1–16개), `wait_ms`(기본 120초, 1–300000 ms), `max_active_runs`(기본 32, 1–256). 웹훅과 별도 별칭을 사용해야 합니다(OpenAI 엔드포인트가 하나라도 설정되면 `v1`은 예약됨). |
| `limits` | `body_bytes`(기본값 OpenAI 256 KiB, 그 외 입구 64 KiB; 최대 1 MiB), `attachments`(기본 `8`), `per_entrance_per_min`(기본 `120`), `per_principal_per_min`(기본 `30`). |
| `admin_url` | 일반 승인 카드에 링크할 선택적 Admin UI URL(Slack 소유자 예약 승인 DM에는 미사용). |

Slack 워크스페이스 `receive` 규칙:

- `receive`는 `socket`(기본값)만 허용합니다. `signing_secret`이 설정되어 있어도 `slack.workspaces.<alias>.receive`에 `http`를 지정하면 `bot validate`와 `bot serve`가 거부합니다.
- `socket` 모드에는 `app_token`이 필수입니다.
- `signing_secret`은 형식만 검사하며 런타임에는 사용하지 않지만 활성 세트에 포함됩니다(변경 시 `--activate` 필요).

`host.json`에 시크릿 값을 넣지 마세요. 이름은 바인딩 소유 프로필의 `bot/<profile>/<name>`에서만 해석됩니다(전역 폴백 없음; 토큰 값이 같아도 권한이 합쳐지지 않음).

## 주체와 입구

- **주체 형식**: `local:<account>`, `slack:<workspace-alias>/<Slack-user-ID>`, `http:<endpoint-alias>/<sub-or-subject>`. 내부 구분 문자로 다른 주체를 위장할 수 없습니다.
- **입구 유일성**: 활성 프로필 전체에서 같은 입구를 중복 바인딩할 수 없습니다(별칭과 해석된 ID 모두). CLI는 `(OS 사용자, 프로필)`로 입구를 선택합니다.
- **멤버십과 스코프**: 비멤버는 거부됩니다(Slack은 조용히 ack, HTTP는 `403`, CLI는 `forbidden`). 요청자 스코프는 `(역할 스코프 합집합) ∩ 정책 상한`이며, 에이전트 실행을 시작하려면 `run`이 필요합니다. 승인자는 기본적으로 사람 주체(CLI/Slack)이며, HTTP 서비스 주체는 승인자가 될 수 없습니다.
- **트리거**: 같은 바인딩과 이벤트(또는 HTTP 메서드/경로)의 후보는 `order` 순으로 평가하며 첫 일치가 선택됩니다. 봉투(envelope) 매처(`body.…`, `conversation.…`)는 점 경로 조회와 리터럴 일치만 지원합니다.
- **비공개 트리거**: 일반 접수/큐 적재 전에 `PrivateIntake`로만 라우팅되며 실패 시 일반 채팅으로 폴백하지 않습니다. 선택된 `private_pass: true` 트리거가 유효한 빈 PASS를 반환하면 원문 텍스트와 첨부 참조가 해당 바인딩의 일반 DM 접수로 전달됩니다. 영구 거부(형식/권한)만 ack되며 진행 중, 시작 전 재시도, 충돌, 장애 신호는 재전달을 위해 unack 상태로 남습니다.

## CLI 입구

- **상호 피어 검증**: Unix 도메인 소켓(`0700` 디렉터리, `0600` 소켓)을 사용하며 **피어 UID**로 클라이언트를 인증합니다. `bot chat`은 텍스트를 보내기 전에 소켓 디렉터리가 현재 사용자 소유이고 권한이 `0700`이며 심볼릭 링크가 아닌지, 소켓 파일이 현재 사용자 소유인지, 피어 프로세스 UID가 자신과 같은지 검증합니다. 하나라도 어긋나면 재시도 코드 `3`이 아닌 종료 코드 `1`로 실패하여 공유 디렉터리의 소켓 스푸핑을 차단합니다.
- **소켓 경로 우선순위**: `BOT_SOCKET` → `gateway.cli.socket` → `<XDG_RUNTIME_DIR 또는 TMPDIR>/bot-<uid>/gateway.sock` → `$HOME/.local/state/bot/run/gateway.sock`(공유 `/tmp`는 절대 미사용). `$HOME`이 절대 경로가 아니면 시작/클라이언트가 `BOT_SOCKET`을 안내하며 실패합니다.
- **와이어 프로토콜**: 연결당 요청 JSON 한 줄(`{"v":1,"op":"chat","request":ID,"text":…,"profile"?,"thread"?,"wait"?}` 또는 재조회/재연결용 `op:"result"`)과 여러 줄의 응답 JSON(`accepted` → 선택적 `state` → `result`, 또는 고정 `code`와 메시지가 담긴 `error`)을 주고받습니다. 재시도 시 같은 요청 ID를 재사용하세요(`bot chat`은 생략 시 자동 생성).
- **연결 종료와 인증**: 소켓을 닫아도 실행은 취소되지 않습니다. 상태나 결과를 내보내기 전에 현재 멤버십을 재확인합니다. CLI 응답은 아웃박스가 아니라 폴링으로 조회합니다.

## HTTP 입구

이 절은 JWT 웹훅 엔드포인트(`kind: "webhook"` 또는 생략)를 다룹니다. SDK 경로는 [OpenAI 호환 API](#openai-호환-api)를 참고하세요.

평문 HTTP를 제공하므로 기본적으로 루프백에만 바인딩합니다. 외부 트래픽을 받으려면 TLS 리버스 프록시와 `gateway.http.remote: true`가 필요합니다.

- **경로**: `POST|PUT|PATCH|DELETE|GET /{endpoint-alias}{trigger-path}`.
- **필수 헤더**: `Authorization: Bearer <HS256 JWT>`(`iss`, `aud`, `exp`, `nbf` 검증; `alg: HS256`만 허용; 키 ≥ 32바이트) 및 `Idempotency-Key`(재시도 시 재사용).
- **라우팅 및 인증 순서**: 미등록 별칭은 인증 전에 `404`를 반환합니다. 등록된 엔드포인트는 경로 조회 전에 인증하며 실패 시 사유를 노출하지 않고 `401`을 반환합니다. 인증과 접수는 단일 라우팅 테이블을 공유하고, 키 캐시는 `(profile, binding, revision, secret name)` 단위로 격리됩니다.

| 응답 | 조건 |
|---|---|
| `202` + `Location` | 기본(`accepted`) 또는 `sync` 타임아웃; 본문 `{request_id, state}`. |
| 도구 원본 `{status, body}` | `sync` 모드에서 도구가 `status` ∈ `{200, 201, 204, 400, 403, 404, 409, 422}`인 `{status, body}`를 반환한 경우. |
| `400`, `401`, `403`, `404`, `405`, `409`, `413`, `415`, `429` (+`Retry-After`), `503` | 잘못된 요청, 인증, 권한, 미발견, 메서드, 동일 키 본문 충돌, 본문 크기 초과, 미디어 타입, 속도 제한, 저장소 장애. |
| `502` | `sync` 도구 출력이 `{status, body}` 형식과 맞지 않는 경우. |

상태 폴링(`GET /{endpoint}/_requests/{id}`)은 원 요청 주체에게만 결과를 반환하며 새 도구 호출을 시작하지 않습니다. 응답 모드(`sync` vs `accepted`)는 최초 접수 시 저장되어 이후 트리거가 바뀌어도 재전달과 폴링에서 유지됩니다(기록이 없는 레거시 행은 `accepted` 반환). 상태나 결과를 반환하기 전에 현재 멤버십과 활성 리비전을 재검증합니다.

## OpenAI 호환 API

`gateway.http.endpoints.<alias>`에 `kind: "openai"`를 설정하고 프로필에서 공개 모델 이름을 에이전트에 바인딩합니다([OpenAI 모델 바인딩](profile-spec.md#openai-모델-바인딩)). 사용 가능한 모델 이름은 `models.list()`로 조회할 수 있습니다.

```python
import os
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8788/v1", api_key=os.environ["BOT_API_KEY"])
model = client.models.list().data[0].id
reply = client.chat.completions.create(
    model=model,
    messages=[{"role": "user", "content": "Show today's agenda"}],
    extra_headers={"Idempotency-Key": "agenda-request-1"},
)
print(reply.choices[0].message.content)
```

- **멱등성**: 기존 요청을 폴링하려면 원래의 `messages`, `model`, `Idempotency-Key`를 그대로 재사용하고, 새 작업에는 새 키를 사용하세요. 생략하면 서버가 임의 키를 생성해 `Idempotency-Key` 응답 헤더로 돌려줍니다(첫 응답을 잃으면 복구할 수 없으므로 외부 쓰기가 가능한 요청에는 항상 키를 직접 지정하세요). 키 없이 재전송된 SDK 재시도는 `400 idempotency_key_required`로 실패합니다. 본문이 같은 별개 요청끼리는 병합되지 않습니다.
- **인증과 제한**: 모든 경로에 `Authorization: Bearer <API key>`(32–256바이트 ASCII 출력 가능 문자)가 필요합니다. 키 슬롯은 `secret` 이름을 HTTP `subject`에 매핑합니다. 키를 교체할 때는 같은 `subject`에 새 슬롯을 추가한 뒤 기존 키를 폐기하세요. 게이트웨이가 요청마다 `SecretStore`를 읽으므로 교체가 즉시 반영됩니다. 모델 목록, 완료 요청, SSE 모두에 현재 멤버십이 적용됩니다. 기본값은 루프백 전용입니다(원격 접근은 `gateway.http.remote: true` + TLS 프록시; 기본 CORS 없음). 요청 본문 상한은 기본 256 KiB(`gateway.limits.body_bytes`로 최대 1 MiB까지 조정 가능; 웹훅 기본 64 KiB)이며 읽기 타임아웃은 최대 10초(남은 요청 기한 내로 제한)입니다.

| 메서드와 경로 | 응답 |
|---|---|
| `GET /v1/models` | 인증된 멤버가 이 입구에서 실행할 수 있는 모델 목록(`created`는 현재 프로필 리비전이 처음 저장된 Unix 초). |
| `GET /v1/models/{id}` | 권한이 있는 단일 모델; 없는 모델과 권한 없는 모델 모두 `404`를 반환합니다. |
| `POST /v1/chat/completions` | `choices[0].message.content`에 최종 응답 반환(출력 제한으로 잘리면 `finish_reason: "length"`; 최종 응답을 저장하지 못하고 제한에 걸리면 `422 run_failed`). `stream: true`는 SSE를 반환합니다. |

### 대화와 승인

- 각 요청은 독립 대화입니다. 클라이언트가 이전 턴을 `messages`에 다시 담아 보냅니다. 마지막 `user` 턴이 프롬프트가 되며 앞선 턴은 순서대로 한 번 컨텍스트로 전달됩니다. 클라이언트가 보낸 `system`, `developer`, `assistant` 턴은 신뢰할 수 없는 사용자 데이터로 취급되며, 서버가 프로필 지침과 권한을 강제합니다.
- 승인이 필요한 도구는 바인딩의 지정 승인자(위임된 하위 실행에도 상속)가 승인한 뒤에만 실행됩니다. 서버는 접수 시점과 현재 프로필 사이에서 승인자와 전달 경로가 일치하는지 검증합니다. Slack 승인자는 1:1 DM에서, 로컬 승인자는 인증된 Admin UI에서 결정하며 HTTP 키는 승인할 수 없습니다. 승인자가 없거나 권한이 회수되었거나 경로가 바뀌었거나 DM 전달에 실패하면 HTTP 응답은 해당 작업이 실행되지 않았음을 알립니다.

### 대기, 폴링, 스트리밍

- `wait_ms`는 인증 이후 본문 읽기, 접수, 대기를 모두 포괄하는 단일 기한입니다(기본 120초, 범위 `1..=300000` ms). 작업 예산은 `wait_ms - min(2000, wait_ms / 4)` ms이며, 나머지는 최종 권한 검증과 응답 전송을 위해 남겨 둡니다. `422 request_outcome_unknown`을 피하려면 수 초 이상의 값을 권장합니다.
- 대기 예산이 만료될 때까지 실행이 진행 중이면 서버가 권한을 재확인한 뒤 HTTP `200`과 함께 `"Request is still in progress. Query again with the same Idempotency-Key and request."`를 반환합니다. 사람 승인 대기 상태가 되면 즉시 HTTP `200`과 `"Awaiting approval. Query again with the same Idempotency-Key and request."`를 반환합니다. 두 경우 모두 실행은 계속 유지됩니다.
- 같은 키와 본문으로 다시 POST하면 현재 상태나 최종 응답을 반환합니다. 같은 키에 다른 본문을 보내면 `400 idempotency_key_conflict`, 확인 불가능한 저장소 오류는 `422 request_outcome_unknown`을 반환합니다. 폴링에는 원 주체와 엔드포인트에 속한 키가 필요합니다.
- `stream: true`는 `text/event-stream`(`Cache-Control: no-cache`, `X-Accel-Buffering: no`)을 반환하고 대기 중 최대 15초마다 `: keep-alive` 주석을 보낸 뒤, assistant 역할 청크, 최종 응답 텍스트 청크, finish reason, `data: [DONE]`을 스트리밍합니다(이 버전에서는 생성 중 토큰 스트리밍을 지원하지 않습니다). 스트림이 열린 뒤 오류가 나면 오류 객체를 보내고 연결을 닫습니다.
- `usage`(`prompt_tokens`, `completion_tokens`, `total_tokens`)는 실행 내 모든 모델 호출이 입력/출력 토큰 수를 보고했을 때만 포함되며 그렇지 않으면 생략됩니다. SSE에서 `stream_options.include_usage: true`를 지정하면 일반 청크에는 `usage: null`이 담기고, 전체 집계치가 있을 때 `[DONE]` 직전에 빈 choices와 `usage`를 담은 최종 청크가 전송됩니다.

### 지원 요청 파라미터

| 파라미터 | 동작 |
|---|---|
| `model`, `messages` | 필수. 역할은 `system`, `developer`, `user`, `assistant`; 문자열 또는 텍스트 파트 배열(`\n`으로 결합). 최소 하나의 `user` 턴이 있어야 하며 마지막 `user` 턴에는 공백 외 내용이 있어야 합니다. |
| `stream`, `stream_options.include_usage`, `n` | 기본 비스트리밍; `include_usage`는 `stream: true`와 함께만 유효; 단일 선택지(`n: 1`). |
| `temperature`, `top_p`, `seed`, `user`, `metadata` | 타입/범위만 검증하며 실행, 신원, 감사에는 반영하지 않습니다. |
| `store`, `response_format` | 생략, `store: false`, 평문 텍스트 형식만 허용(접수/복구 상태는 별도 저장). |
| `max_tokens`, `max_completion_tokens`, `stop` | 요청별 제한/중지 시퀀스는 미지원(`400`). |
| `tools`, `functions`, `tool_choice`, 이미지/오디오/파일, `logprobs` 등 | `400 unsupported_parameter`(서버에서 프로필이 인가한 도구만 존재). |

### 오류, 속도 제한, 재시도

오류는 `{"error":{"message":"…","type":"…","param":null,"code":"…"}}` 형식을 따릅니다. `param`은 서버가 아는 필드 경로(최상위는 `null`, 중첩 필드는 `messages[0]` 등 알려진 상위 경로)만 보고하며 호출자가 만든 이름이나 값은 되돌려 보내지 않습니다.

| HTTP | `type` | `code` |
|---|---|---|
| `400` | `invalid_request_error` | `invalid_request`, `unsupported_parameter`, `idempotency_key_conflict`, `idempotency_key_required` |
| `401` / `403` | `authentication_error` / `permission_error` | `invalid_api_key` / `access_denied` |
| `404` / `405` | `invalid_request_error` | `model_not_found`, `not_found` / `method_not_allowed` |
| `413` / `415` | `invalid_request_error` | `request_too_large` / `unsupported_media_type` |
| `408` / `429` / `503` | `invalid_request_error` / `rate_limit_error` / `server_error` | 접수 전 `request_timeout` / `rate_limit_exceeded`(`Retry-After` 초) / `service_unavailable` |
| `422` | `server_error` | `request_outcome_unknown`(동일 키+본문으로 폴링), `run_failed`, `run_cancelled` |

- `max_active_runs`에 도달하면 `429`와 `Retry-After: 1`을 반환하며, 속도 제한은 남은 초의 올림값을 반환합니다. 이미 접수된 키를 폴링할 때는 활성 실행 슬롯을 소비하지 않습니다.
- **인증 실패 제한**: 게이트웨이 프로세스별로 직접 피어 IP 기준 추적합니다(`X-Forwarded-For` 무시). 60초 내 `Authorization` 실패가 20회에 도달하면 키 조회 전에 해당 IP의 모든 모델/채팅 요청(유효한 키와 폴링 포함)을 60초간 차단합니다(`429` + `Retry-After`). 헤더 누락과 저장소 장애는 횟수에 포함하지 않습니다. v2.0 고정 값이며, 필요하면 공유 TLS 프록시에서 사용자별 제한을 적용하세요.
- 인증 후 접수된 실행은 SDK가 자동 재시도하는 `408`/`409`/`429`/`5xx`를 반환하지 않으며, 커밋이 불확실한 경우 `422`와 `x-should-retry: false`를 반환합니다. 공식 [Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) 및 [Models](https://developers.openai.com/api/reference/resources/models/methods/list) 명세를 참고하세요. 향후 Responses API와 토큰 스트리밍도 동일한 인증·권한·결과 모델을 공유하며 외부에 안전한 텍스트만 순서대로 스트리밍합니다.

## Slack 입구

### 앱 설정

v2.0은 Slack 이벤트와 버튼 클릭을 오직 [Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/)로만 수신합니다(HTTP Request URL 수신 미지원). 배포에 필요한 항목만 설정하세요.

| 기능 | Slack 앱 설정 | 사용 게이트웨이 API / 이벤트 |
|---|---|---|
| 연결 (필수) | **Socket Mode** 활성화; **Basic Information → App-Level Tokens**에서 `connections:write` 권한으로 App-Level Token 발급. | `apps.connections.open`(앱 토큰); `auth.test`(봇 토큰)로 팀과 봇 사용자 검증. |
| 메시지 및 승인 카드 | **OAuth & Permissions**에서 봇 스코프 `chat:write` 추가; 대상 채널에 봇 초대. | `chat.postMessage`, `chat.update`(버튼 상태 갱신), `chat.postEphemeral`(클릭 안내). |
| 채널 멘션 | 봇 스코프 `app_mentions:read`; **Event Subscriptions** 활성화 후 봇 이벤트 `app_mention` 구독. | 등록된 채널의 사람 [`app_mention`](https://docs.slack.dev/reference/events/app_mention/) 게시물. |
| 스레드 답글 (`thread_reply`) | 공개 채널: `channels:history` + `message.channels`. 비공개 채널: `groups:history` + `message.groups`. 봇 초대 및 바인딩 `events`에 `thread_reply` 추가. | **봇이 참여한 스레드**의 사람 답글만 수신([`message.channels`](https://docs.slack.dev/reference/events/message.channels/), [`message.groups`](https://docs.slack.dev/reference/events/message.groups/)). |
| 리액션 (`reaction`) | 봇 스코프 `reactions:read`, `users:read`; 봇 이벤트 `reaction_added`; 바인딩 `events`에 `reaction` 추가. | 등록된 채널의 `item.type=message`에 달린 활성 사용자 리액션([`reaction_added`](https://docs.slack.dev/reference/events/reaction_added/), [`users.info`](https://docs.slack.dev/reference/methods/users.info/)). |
| 1:1 DM | 봇 스코프 `im:history`; 봇 이벤트 `message.im`; **App Home → Messages Tab**에서 사용자 메시지 허용; 워크스페이스 `dm: true` 및 DM 바인딩 추가. | [`message.im`](https://docs.slack.dev/reference/events/message.im/) (`type: message`, `channel_type: im`). |
| 승인 / 중지 버튼 | **Interactivity & Shortcuts → Interactivity** 활성화. | Socket Mode를 통한 `block_actions`. |
| 채널 승인용 소유자 DM | 봇 스코프 `im:write`(및 `chat:write`, Interactivity). | [`conversations.open`](https://docs.slack.dev/reference/methods/conversations.open/)으로 소유자와 1:1 DM 열기(DM 접수가 꺼져 있어도 동작). |
| 응답 파일 첨부 | 봇 스코프 `files:write`. | [`files.getUploadURLExternal`](https://docs.slack.dev/reference/methods/files.getUploadURLExternal/) → 바이트 업로드 → `files.completeUploadExternal`. |
| 진행 표시 및 중지 (선택) | 봇 스코프 `reactions:write`(👀/✅/❌용); 스트리밍 진행 표시 및 중지 카드를 위해 `chat:write`·Interactivity와 Slack AI App/스트리밍 활성화. | `reactions.add`/`remove`, `chat.startStream`/`appendStream`/`stopStream`/`delete`, `agent_session_stopped`. |

워크스페이스에 앱을 설치(스코프 변경 후 재설치)하고 봇/앱 토큰을 프로필의 `SecretStore`에 저장한 뒤, `gateway.slack.workspaces.<alias>`에 `team_id`, `channels`, `bot_token`, `app_token` 이름을 설정합니다.

### 접수, 승인, 전달

- **인증된 설치**: `(profile, workspace alias)`당 Socket Mode 세션 하나를 유지합니다. hello 시 `app_id`를, `auth.test`로 `team_id`를 검증합니다. 이벤트는 해당 프로필 내부로만 라우팅되며(다른 프로필로 폴백 없음), 시작 시 두 프로필 간 `app_token` 중복 사용을 거부합니다.
- **이벤트 접수**: 채널 바인딩은 `events`로 수신 이벤트를 선택하며(기본 `app_mention`만), `thread_reply`와 `reaction`은 각각 트리거 `on.event` 값 `thread-reply`와 `reaction`에 매칭됩니다([프로필 명세](profile-spec.md)). DM은 사람 `message` 이벤트(수정, 삭제, 봇 제외)를 수락합니다. 요청 ID `<channel>.<ts>`는 이벤트 유형과 재전달 간 중복을 제거하며, 영속 커밋 뒤에만 ack합니다. 수정, 삭제, 봇/앱 메시지, 자기 리액션, 다른 팀, 미등록 채널, 잘못된 `ts`는 무시합니다.
- **스레드 답글**: 봇 참여 증거는 `(team, bot ID, profile, binding, channel, root ts)`이며 봇 게시 영수증이나 소켓에서 관측된 인증된 자기 게시물에서만 기록됩니다(수신 `thread_ts`는 절대 신뢰하지 않음). 봇 자기 게시물은 증거만 기록하고 실행을 트리거하지 않습니다. 미검증 스레드는 ack 후 건너뛰고(`thread_unverified`), 증거 저장소 장애는 재시도를 위해 unack로 남깁니다. 이 버전 이전의 레거시 스레드는 봇이 다시 한 번 글을 올리면 대상이 됩니다(백필 없음).
- **단일 접수 중복 제거**: 같은 게시물에 대해 `app_mention`과 `message.channels`가 서로 다른 `event_id`로 도착할 수 있습니다. 분류는 메시지 내용을 기준으로 하므로(`mention`이 활성화되어 있으면 봇 멘션 우선, 아니면 `thread_reply`) 도착 순서가 결과를 바꾸지 않습니다. 또한 게시물은 `(team, channel, ts)`, 리액션은 `(team, event_id)`의 Slack 이벤트 신원으로 중복을 제거해 바인딩이나 프로필 변경 후 재전달도 첫 실행으로 수렴합니다(업그레이드 마이그레이션이 기존 게시물의 임시 키를 백필).
- **리액션**: 행위자(`event.user`)가 요청자이며 프로필 멤버여야 합니다. 대상은 `item.channel` / `item.ts`이고, 매칭되지 않은 리액션은 접수 없이 ack합니다. 트리거 본문에는 고정 필드 `reaction`, `channel`, `item_ts`, `item_user`가 담기며 요청 ID는 Slack `event_id`로 설정됩니다(신원과 증거는 본문에서 재구성하지 않음).
  - *사람 여부 확인*: 모든 멤버 리액션마다 `users.info`를 호출합니다(비활성화 사용자를 즉시 차단하기 위해 긍정 캐시 없음). `id` 일치, `is_bot: false`, `deleted` 생략 또는 불리언 `false`(`null`/문자열 거부)를 요구하며, 자기 자신과 `USLACKBOT`을 제외합니다(`is_app_user`는 거부 사유 아님). 비멤버에는 `users.info`를 호출하지 않습니다.
  - *실패 시 차단(fail-closed) ack*: `users:read` 누락, 속도 제한, 오류, 2초 타임아웃, 형식 오류 시 고정 사유(`actor_not_human`, `actor_scope_missing`, `actor_unverified`)를 기록하고 후보나 폴백 채팅을 실행하지 않은 채 Slack 3초 제한 내에 ack합니다. 멘션과 DM에는 `users:read`가 필요하지 않습니다.
  - *응답 스레드*: 리액션 대상 메시지가 검증된 봇 참여 스레드의 루트일 때만 스레드 답글로, 그 외에는 채널 최상위로 응답합니다.
- **미처리 이벤트 넘기기 (`bot/pass`)**: 인증된 Slack 이벤트의 직접 도구 트리거는 빈 `content`와 함께 `_meta["bot/pass"]: true`를 반환할 수 있습니다. 접수 시 매칭된 직접 트리거를 `order` 순으로 고정하고, 각 PASS마다 판정·커서 이동·감사를 원자적으로 커밋한 뒤 다음 후보로 진행하며, 마지막에는 기본 채팅으로 한 번 폴백합니다(첫 에이전트 지시 트리거, 없으면 바인딩 에이전트; 멘션/답글은 원문 텍스트, 리액션은 `{"event":"reaction_added","item_ts":…,"reaction":…}`). 폴백은 체인 생성 시와 마지막 후보가 PASS할 때 **현재 리비전과 실행에 고정된 리비전 양쪽**에서 `run` 스코프를 검증합니다(없거나 회수되면 조용히 종료). 읽기 전용 분류 호출만 PASS할 수 있습니다([MCP 도구](plugins.md)).
- **첨부 파일**: 3단계 외부 업로드(URL → 바이트 → 완료)를 사용하며, 승인 카드와 첨부 파일은 한 응답에 합치지 않습니다. 푸시 대화(채널/DM)에서는 루트 에이전트 응답 앞부분의 `<attachment name="attachment.md">...</attachment>` 블록을 같은 스레드의 후속 `text/plain` 첨부 응답으로 분리합니다(남은 본문이 비어 있으면 첨부 내용이 본문이 되며, 위임된 하위 실행·CLI·HTTP는 원문을 유지).
- **꼬리 주석**: Slack 응답 본문 끝의 `※` 줄은 브로드캐스트 멘션을 무력화한 `context` 블록(`mrkdwn`)으로 분리 표시합니다. Slack의 3,000자 블록 한도를 넘는 주석은 잘라내지 않고 본문 `markdown` 블록에 유지합니다(마크다운 블록 한도 초과 시 평문 폴백).

### 진행 표시와 중지

1:1 DM과 채널 봇 멘션의 일반 수락 실행(`Admission::Accepted`)은 실시간 진행 상태를 표시하고 스레드 내 취소를 지원합니다(`crates/bot-gateway/src/slack/ux.rs`, `crates/bot-gateway/src/slack/ux/cards.rs`, `crates/bot-gateway/src/slack/ux/stop.rs`, `crates/bot-gateway/src/slack/ux/stream.rs`). 멘션 없는 `thread_reply`, `reaction_added`, 비공개 DM에는 진행 UI와 중지 처리가 적용되지 않습니다.

- **메모리 내 감시**: 최대 30분(`WATCH_LIMIT`) 동안 실행을 추적합니다. 게이트웨이 재시작, 만료, 완료 이후에는 Agent 중지 버튼이 동작하지 않고, 중지 명령은 일반 질문으로 접수되며, 만료된 확인 카드 버튼은 `STOP_GONE`(`"This task has already finished or the prompt has expired."`)으로 갱신됩니다.
- **진행 표시**: 👀(`eyes`) 리액션을 달고 `chat.startStream` / `chat.appendStream` / `chat.stopStream`으로 진행 요약을 스트리밍하며(`chat.delete` 전에 `task_update`를 `complete`로 마감), 성공 시 ✅(`white_check_mark`), 실패/취소 시 ❌(`x`)로 교체합니다. 같은 스레드에 여러 요청이 동시에 돌 때는 한 번에 하나의 진행 카드만 유지하고, 표시 중이던 요청이 끝나면 `session_status: "processing"`을 유지한 채 다음 활성 요청으로 순차 인계해 모든 요청이 끝날 때까지 상태를 이어서 표시합니다. 순수 장식용(API 실패가 실행이나 전달을 막지 않음)이며 고정 상태 문구, 에이전트 약칭, 도구 이름만 표시하고 사용자 텍스트·시크릿·인자·초안은 절대 노출하지 않습니다.
- **중지 경로**: (1) Slack Agent 중지 버튼(`agent_session_stopped`)이 확인 카드를 게시; (2) 활성 스레드의 단독 중지 명령(`"stop"`, `"cancel"`, `"abort"`, `"halt"`, `"stop please"`, `"cancel please"` — 채널에서는 봇 멘션 필요, 1:1 DM에서는 불필요; 추가 텍스트가 있으면 새 요청으로 접수)이 즉시 중지; (3) 확인 카드 버튼 **Stop here**(`bot_stop_confirm`)와 **Continue**(`bot_stop_continue`)로 최종 확정.
- **권한과 실행**: 요청자 또는 현재 리비전의 `admin`/`owner` 역할 멤버만 중지할 수 있습니다(그 외에는 `chat.postEphemeral`로 임시 안내 표시). 확정된 중지는 실행과 하위 트리 전체에 `RunAdmin::cancel`을 호출하고 `slack_user_stop`을 감사 기록하며 ❌와 함께 중지 카드를 게시합니다. 확인 전에 이미 완료되었다면 완료 사실을 알립니다.

## 시크릿 저장소와 명령

`BOT_SECRETS`로 시크릿 백엔드를 선택합니다.

- `env` (기본값, 개발용): `BOT_SECRET__<PROFILE>__<NAME>` 환경 변수를 읽습니다(`-`는 `_`로 치환).
- `keychain` (macOS): 서비스 `bot/<profile>`, 계정 `<name>`.
- `sops:<file>`: `sops --decrypt` JSON 출력에서 `bot.<profile>.<name>`을 읽습니다.

핸들은 생성 시 단일 프로필에 고정되어 다른 프로필을 읽을 수 없습니다. 프로세스는 시작 시 선언된 모든 시크릿 이름을 마스킹기(redactor)에 미리 적재하고 `BOT_SECRETS_REREAD_SECS`(기본 `60`, `0`은 비활성화 — `security`를 호출하는 `keychain`이나 30초 캐시를 쓰는 `sops`에서는 값을 높이는 것을 권장)마다 갱신합니다. 런타임 역할은 마스킹으로 감싼 저장소(`Observe.secrets`)만 받으며, 원시 저장소는 컴파일 단계에서 거부됩니다.

### CLI 명령

| 명령 | 동작 |
|---|---|
| `bot validate` | 프로필, 호스트 레지스트리, 입구 유일성, 필수 시크릿(선언·도구·원격 HTTP bearer·입구·관측성 및 허용된 `claude-cli`/`codex-cli`/`agy-cli`/`anthropic-api`/`openai-api`, `embeddings`, `memory_judge` 키 — 누락 시 이름과 환경 변수 보고), HTTP 키 길이(≥ 32바이트), 메모리 시드 본문 한도(8 KiB), 에이전트 `builtin` 호환성, 도구 서버 설치본을 오프라인 검증(DB/Slack 미접속). 모든 검사를 통과할 때만 활성화 리비전과 서버 리비전을 출력합니다. |
| `bot chat [--profile P] [--thread T] [--request-id ID] [--no-wait] "text"` | 채팅 요청을 보냅니다(예: `bot chat --profile personal "Show today's agenda"`, `bot chat --profile personal --request-id walk-1 "Delete the walk event"`). 연결이 끊겼다면 `bot chat --profile P --attach --request-id ID`로 재연결합니다. 종료 코드 `3`은 재시도 가능함을 뜻합니다. 모델/도구 출력이 OSC 52 클립보드 쓰기, 창 제목 변경, 화면 지우기를 일으키지 않도록 터미널 제어 문자(ESC, C1, BEL, CR, DEL 등)를 `\u{1b}` 형태로 이스케이프합니다(`\r\n`은 `\n`으로 정규화). |
| `bot map [--json] [--profile P]` | 구조 맵을 출력합니다(이름, 분류, 개수만 포함). |
| `bot init <name> [--template personal\|team\|company] [--dir <dir>]` | `bot validate`를 통과하는 `<name>/`(데모 모델과 현재 OS 사용자 CLI 계정이 들어간 `host.json` + `profiles/<name>/`)을 생성합니다. 소문자 영숫자+하이픈 이름만 허용하며 덮어쓰기나 심볼릭 링크 추적을 하지 않고, `$TMPDIR` 사본에서 검증한 뒤 `--dir` 아래 임시 디렉터리를 원자적으로 이동합니다(최종 umask 권한으로 한 번만 열기; `BOT_*` 환경 변수 무시). |
| `bot add agent <lineage> [--dir <dir>]` · `bot add skill <name> [--agent <lineage>] [--dir <dir>]` | `AGENT.md` 또는 `SKILL.md`를 생성합니다(`main/cs` → `agents/main/agents/cs/`; `--agent` 생략 시 프로필 전역 스킬). 스킬 이름을 `skill-creator`로 지정하면 슈퍼 관리자용 실시간 관리 `SKILL.md` 전체를 생성합니다. `tools.yaml`을 만들거나 `profile.yaml`을 수정하지 않으며, 상위 누락·중복·가시 스킬 이름 충돌을 거부하고, 단일 열린 디렉터리 fd 기준으로 동작해 실패 시 빈 디렉터리만 정리합니다. |
| `bot add tool <skill>.<tool> [--class read\|write\|irreversible] [--server <alias>] [--agent <lineage>] [--dir <dir>]` | `tools.schema.json`과 잠금 규칙을 검증한 뒤 `<skill>/tools.yaml`을 생성하거나 추가합니다(기본 `--class read`, `--server`는 기존 서버 또는 스킬 이름). `profile.yaml`과 `host.json`은 수정하지 않고 필요한 `servers`/`roles`/`policy`/`approve` 스니펫을 출력합니다. 잘못된 이름, 예약된 내장 ID, 중복, 스킬 누락/모호함, 서버 불일치, 심볼릭 링크를 거부합니다([첫 도구 만들기](first-tool.md)). |
| `bot doctor [--host <host.json>] [--profiles <dir>] [--check-db]` | 런타임 검증기를 사용해 호스트 설정, 모델 제공자, 도구 서버, 프로필, 입구, 필수 시크릿 **이름**, DB URL 구성을 영문으로 요약합니다(기본 오프라인: 네트워크·마이그레이션·서버 실행 없음; 원격 DNS/연결 미검사; `sops`는 `SKIP`). `--check-db`는 5초 읽기 전용 DB 핑을 수행합니다. 시크릿 값·DB URL·원시 DB 오류는 절대 출력하지 않으며 `SKIP` ≠ `PASS`, `FAIL`이 하나라도 있으면 종료 코드 `1`로 종료합니다. |
| `bot mcp sync <skill-dir>` | 원격 `transport: "http"` 서버에서 `tools.yaml`에 선언된 도구의 `tools/list`를 가져와 어휘 및 시크릿 유출 검사 후 결정적 `tools.lock.yaml`(이름, 설명, 입력 스키마, sha256)을 기록합니다. 브로커는 호출 전에 해시를 검증합니다([MCP 도구](plugins.md)). |
| `bot profile-bundle put <profile-dir>` | 진행 중인 작업이 여전히 참조하는 과거 리비전을 `bot serve`가 복원하지 못할 때 과거 프로필 디렉터리 번들을 저장소에 백필합니다. 자체 해시되며 호스트 레지스트리를 무시하고 불변입니다(손상된 행은 수동 삭제 후 재등록). |
| `bot serve --role <roles> [--activate]` | 시작 순서: 검증 → 읽기 전용 활성 리비전 확인 → 현재 번들 저장 및 진행 중인 실행·재개 가능 데드 레터·대기 카드·정적 크론 윈도우·활성 사용자 예약에 필요한 과거 리비전 복원 → 역할 검사 및 리스너 바인딩 → 활성 리비전 + `bot_store::check_seed` 메모리 시드 원자적 커밋 → 접수 개시. 커밋 전 실패 시 활성 리비전·실행·카드·시도는 그대로 유지됩니다. 동일 리비전이면 조용히 합류하고, 비어 있으면 초기화하며, `--activate`가 있을 때만 CAS 교체합니다. |

### 환경 변수와 로깅

| 변수 | 용도 |
|---|---|
| `BOT_PROFILES`, `BOT_HOST_CONFIG` | 프로필 루트 디렉터리와 `host.json` 경로(미설정 시 데모 설정). |
| `BOT_DATABASE_URL`, `BOT_DB_MAX_CONNECTIONS` | PostgreSQL DSN과 커넥션 풀 상한(기본 `10`, 범위 `1..=200`). |
| `BOT_SOCKET`, `BOT_INSTALLS` | CLI Unix 소켓 경로 재정의 및 읽기 전용 도구 서버 설치 루트. |
| `BOT_SECRETS`, `BOT_SECRETS_REREAD_SECS` | 시크릿 저장소 백엔드와 마스킹기 갱신 주기(초, 기본 `60`, `0`은 비활성화). |
| `BOT_ADMIN_TOKEN_FILE` | 비대화형 Admin UI 운영자 시크릿 전달용 절대 경로(`0600`). |
| `BOT_LOG`, `BOT_LOG_FORMAT` | 로그 레벨(`error`, 기본 `warn`, `info`, `debug`, `trace`)과 stderr 형식(기본 `text`, 줄 단위 JSON `json`: `{ts_ms, level, target, message, fields}`). |

접수, 전달, 승인은 식별자와 고정 어휘 코드만 담은 `bot_core::trace` 스팬을 방출하며(`bot.private`는 비공개 DM 표시) 본문·인자·논스·영수증은 절대 남기지 않습니다. `BOT_LOG`는 워크스페이스 크레이트에만 적용됩니다. 외부 HTTP/WebSocket/TLS 크레이트는 `warn`으로 상한이 고정되고 `sqlx`는 완전히 꺼져 있어 `trace` 레벨에서도 원시 프레임, 비공개 DM 텍스트, 알 수 없는 DSN 파라미터가 유출되지 않습니다(`crates/bot-cli/src/logging.rs`). `admin.logs`가 켜져 있으면 필터링된 로그가 OTLP `/v1/logs`로도 전송되며(`admin.logs.level`은 `BOT_LOG` 범위 내로 제한), `admin.issues`가 켜져 있으면 오류·패닉·비정상 실패 스팬이 이슈 트래커로 보고됩니다.

## 역할 실행

`--role all`은 `gateway`, `scheduler`, `worker`, `admin`을 함께 실행합니다. 분리 배포는 PostgreSQL과 동일한 활성 세트를 공유합니다.

- **게이트웨이 (Gateway)**: 접수와 전달을 담당합니다. 비공개 DM 트리거는 대상 도구의 `srt` 러너와 프로필별 `private-intake` 지문 키가 필요하며, 원문 텍스트는 지정 도구에만 전달되고 불확실한 결과는 운영자 보류(hold)가 됩니다.
- **워커 (Worker)**: 모델과 내장 도구 브로커를 실행합니다. 허용된 모델 별칭(`claude-cli`, `codex-cli`, `agy-cli`, `anthropic-api`, `openai-api`, 키 없는 루프백 제외)이나 설정된 `embeddings` / `memory_judge` 엔드포인트 중 하나라도 제공자 키가 없으면 활성 리비전 게시를 거부합니다. 누락된 도구 시크릿은 이름만 경고하고, 호출은 승인 카드를 열기 전에 실패하며 `secret_missing`을 감사 기록합니다(`bot validate`는 둘 다 보고; [신뢰성](reliability.md) 참고).
- **스케줄러 (Scheduler)**: 실행 시점이 된 작업을 큐에 넣습니다. 정적 트리거는 어느 역할이 게시하든 활성 리비전 및 메모리 시드와 함께 원자적으로 커밋됩니다. 제거된 트리거 윈도우는 전환 시점에 닫히며 이전 발생분은 해당 윈도우에 남습니다([프로필 명세](profile-spec.md)).
- **관리자 (Admin)**: 기본 주소는 `127.0.0.1:8787`입니다(HTTP 접수 기본값은 `127.0.0.1:8788`). 인증된 데이터 응답은 전송 직전에 활성 세트를 재검증합니다(드리프트/장애 시 `503` + 고정 오류 코드; [Admin UI](admin.md#시작-절차) 참고).

Slack 버튼과 Admin UI는 동일한 브로커 승인 경로를 공유합니다. 외부 도구의 설치 리비전을 검증할 수 없으면 승인 카드는 대기 상태를 유지하고 일시적 오류를 반환합니다.

### Slack 예약 전달 대상

`schedule.create` 권한이 있는 멤버는 프로필에 등록된 모든 채널을 예약 전달 대상으로 선택할 수 있습니다(제한하려면 레지스트리에서 제거). Slack 사용자는 등록된 채널을 별칭, 채널 ID, 정규 경로로 지정할 수 있으며, CLI는 원본 대화만 대상으로 삼고 HTTP 주체는 사용자 예약 도구를 호출할 수 없습니다.

1. **생성**: 레지스트리에서 바인딩과 `team_id/channel_id`를 고정합니다. 소유자는 경로, 스레드, 지침을 검토하고 승인해야 합니다(실시간 Slack 채널 멤버십 조회 없음).
2. **발동**: 현재 레지스트리에서 경로를 재검증합니다. 제거된 경로는 예약을 닫고 원본 대화에 한 번 알리며, 일시적 오류는 재시도합니다.
3. **전달**: 첨부 없이 `<quiet>`만 반환하는 조용한 실행은 원본 응답을 생략합니다. 명시적 대상 초안은 별도의 소유자 승인(`Delivery draft`)이 필요하며, 아웃박스 적재 전과 Slack 어댑터가 채널 ID로 게시하기 직전에 다시 검증됩니다([신뢰성](reliability.md)).

모든 Slack 채널 기원 승인(일반 도구 호출, 위임 하위 실행, 예약)의 경우 v2.0은 채널 바인딩의 Slack 설치를 통해 소유자와 1:1 DM을 열고(원본이 이미 DM이면 기존 DM 재사용), DM 카드에 원 요청 채널·스레드 링크를 함께 표시하며, DM 전달을 확인한 뒤 채널에 고정 안내 문구를 한 번 게시합니다. 이 경로는 승인 전용이며 일반 응답 라우팅이나 DM 접수를 변경하지 않습니다([신뢰성](reliability.md)).

### 배포 중 진행 중인 모델 요청

VM CLI 워커(`claude-cli`, `codex-cli`, `agy-cli`)는 앵커 VM 게이트웨이 주소에 릴레이를 바인딩하고 리스닝 포트를 모델 VM에 전달합니다. 릴레이는 매 업스트림 요청 전에 활성 세트를 확인합니다(구버전은 `403`, 장애/타임아웃은 `503`이며 업스트림으로 절대 전송하지 않음). 열린 스트림은 1초마다 재확인하며, 첫 예정 확인 시점으로부터 10초 안에 해소되지 않으면 고정 오류 이벤트와 함께 종료됩니다([아키텍처](architecture.md), [신뢰성](reliability.md)).

# 게이트웨이 운영

게이트웨이는 CLI·Slack·HTTP 요청을 인증하고 등록된 입구, 멤버, 트리거를 확인한다. 주체와 회신 대화는 인증한 값으로 정한다. 요청 본문이 이 값을 바꿀 수 없다. PostgreSQL에 접수가 커밋된 뒤 요청 ID를 돌려준다. 같은 ID로 재전달하면 같은 실행에 연결된다. [접수와 장애 복구](reliability.md)

## 호스트 등록부(`BOT_HOST_CONFIG`, JSON)

호스트 파일은 프로필의 별칭을 실제 계정, 서비스, 실행 파일에 연결한다. `BOT_HOST_CONFIG`를 생략하면 예시 프로필용 데모 설정을 쓴다. 임의 프로필을 추가할 때는 호스트 파일에 모델과 입구를 등록한다. [새 프로필 설정 예시](profile-spec.md#새-프로필의-호스트-설정)

최상위 키는 `version`(1), `gateway`, `servers`, `models`, `providers`, `egress`, `sandbox`, `limits`, `admin`이다. `models`는 프로필별 허용 별칭을 정하고, `providers`는 각 별칭에 `demo`, `claude-cli`, API 직접 호출(`anthropic-api`·`openai-api`)을 연결한다. 상류 주소에는 자격·쿼리·조각이 없는 HTTPS URL을 쓴다(API 직접 호출은 loopback IP 리터럴에 한해 `http`도 받고 그때만 `secret`을 생략할 수 있다). `egress`는 프로필별로 허용할 외부 호스트와 포트를 제한한다. [모델·샌드박스 설정](architecture.md#호스트-설정과-역할-조립)

`servers`에는 패키지 루트와 실행 명령을 등록한다. `bot validate`에 표시되는 판본은 서버 파일과 실행 명령에서 계산된다. 도구를 실행하는 역할은 `BOT_INSTALLS` 아래 읽기 전용 설치본을 쓴다. 기본 상태 모드는 `ephemeral`이다. 호출 간 상태가 필요한 `required` 서버는 현재 srt 호스트에서 시작할 수 없다. [도구 서버 설치](plugins.md#도구-서버-설치)

활성 집합에는 프로필 판본과 권한에 영향을 주는 호스트 설정을 함께 묶는다. 예를 들어 CLI 사용자 매핑, Slack 팀·채널, HTTP 인증 설정, 허용 모델·상류 목적지와 자격 이름, 도구 설치 판본, egress 상한, 모델 VM 이미지와 망 이름이 들어간다. 비밀 값은 넣지 않는다. 매핑을 바꾼 뒤에는 `bot serve --activate`로 교체한다.

아래는 `gateway` 하위 설정이다.

| 키 | 내용 |
|---|---|
| `cli.accounts.<별칭>` | `{"os_user": "이름"}` 또는 `{"uid": 501}` 중 정확히 하나. `cli.socket`은 절대 경로(100바이트 이하) |
| `slack.workspaces.<별칭>` | `team_id`·`app_id`(선택, 소켓 hello 확인)·`channels`(별칭→채널 ID)·`dm`(DM 입구를 열지, 기본 거짓)·`receive`(`socket`만 — `http`는 Events API·버튼을 받는 수신기와 서명·재전송 검사가 생길 때까지 `bot validate`와 `serve`가 워크스페이스 이름(`slack.workspaces.<별칭>.receive`)을 밝혀 거절한다. 서명 비밀을 줘도 같다)와 자격 **이름** `bot_token`·`app_token`(소켓 수신에 필요)·`signing_secret`(지금은 쓰지 않는다) |
| `http.listen`·`http.remote`·`http.endpoints.<별칭>` | 기본 `127.0.0.1:8788`(관리 화면의 기본 8787과 겹치지 않는다). 이 입구는 TLS 없는 평문 HTTP라서 **루프백이 아닌 주소는 `http.remote: true`로 명시해야** 열린다(기본 거짓) — 없으면 `bot validate`와 `bot serve`가 `gateway.http.listen`을 밝혀 거절하고 아무것도 열지 않는다. TLS를 맡는 프록시 뒤에서 켠다. 웹훅(`kind:"webhook"`)에는 `issuer`·`audience`·`key`(HS256 키의 자격 이름)·`sync_wait_ms`(기본 10초)·`max_token_age_secs` |
| `http.endpoints.<별칭>` (OpenAI) | `kind:"openai"`, 키 슬롯 `keys`, 최대 대기 `wait_ms`(기본 120초), 미완료 실행 상한 `max_active_runs`(기본 32). JWT 웹훅과 다른 별칭을 사용한다. |
| `limits` | `body_bytes`(기본 OpenAI 256 KiB, 그 밖의 입구 64 KiB)·`attachments`(8)·`per_entrance_per_min`(120)·`per_principal_per_min`(30) |
| `admin_url` | 일반 승인 카드용 관리 화면 주소(선택). Slack 소유자의 예약 승인 DM에는 사용하지 않는다 |

자격 값은 이 파일에 쓰지 않는다. 이름만 적고 값은 **바인딩을 가진 프로필**의 `bot/<profile>/<name>`에서 읽는다(전역 폴백 없음, 토큰 값이 같아도 권한을 합치지 않는다).

## 주체와 입구

- 주체 문자열: `local:<account>` · `slack:<워크스페이스 별칭>/<Slack 사용자 ID>` · `http:<입구 별칭>/<토큰의 sub>`. 구분자를 넣어 다른 주체인 척할 수 없다.
- 활성 프로필 전체에서 같은 입구를 두 번 쓸 수 없다(별칭 수준과 실제 ID 수준 모두). CLI는 (OS 사용자, 프로필)로 입구를 고른다.
- 멤버가 아니면 거절한다(Slack은 조용히 ack, HTTP는 403, CLI는 `forbidden`). 요청자 스코프는 `역할 스코프의 합집합 ∩ 정책 상한`이고, 에이전트 실행(`run`)은 이 스코프가 있어야 시작한다. 승인자는 사람 주체(CLI·Slack)만 기본이 되고 HTTP 서비스 주체는 지정하지 않는다.
- 트리거는 같은 바인딩·사건(HTTP는 method·path)의 후보를 order 순으로 검사해 첫 일치 하나만 접수한다. 봉투 경로(`body.…`·`conversation.…`)는 점 경로 조회와 리터럴 동등 비교뿐이다. 민감(private) 트리거는 일반 접수·큐에 쓰기 전에 전용 입구(`PrivateIntake`)로만 가고, 실패해도 일반 대화로 넘어가지 않는다. 민감 입력은 영구 거절(형식·권한)만 확정(ack)이고, 진행 중·시작 전 재시도·충돌·장애 신호는 모두 ack하지 않아 재전달된다.

## CLI 입구

유닉스 소켓(폴더 0700·소켓 0600)과 **피어 uid**로 OS 사용자를 확인한다. **클라이언트도 서버를 확인한다**: `bot chat`은 접속하기 전에 소켓 폴더가 본인 소유의 0700 폴더(링크 아님)이고 소켓 파일이 본인 소유의 소켓인지 점검하고(서버의 `bind`와 같은 규칙), 접속한 뒤 반대편 프로세스의 uid가 자기 uid인지 확인한 다음에야 대화 글을 보낸다. 어긋나면 접속하지 않거나 아무것도 보내지 않고 이유를 밝혀 실패한다(종료 코드 1, 재시도 코드 3이 아니다) — 공용 폴더에 다른 사용자가 먼저 만들어 둔 가짜 소켓이 대화를 가로채거나 가짜 답을 보여 줄 수 없다. 소켓 위치는 `BOT_SOCKET` > 호스트 `gateway.cli.socket` > `<XDG_RUNTIME_DIR 또는 TMPDIR>/bot-<uid>/gateway.sock`이고, 런타임 폴더가 둘 다 없으면(리눅스의 cron·su·컨테이너 등) 공용 `/tmp`가 아니라 홈 아래 `$HOME/.local/state/bot/run/gateway.sock`을 쓴다. 홈도 절대 경로로 알 수 없으면 위치를 정할 수 없다고 이름을 밝혀 `BOT_SOCKET` 지정을 요구하며 거절한다. 한 연결에 요청 한 줄(JSON)과 응답 여러 줄이다. `{"v":1,"op":"chat","request":ID,"text":…,"profile"?,"thread"?,"wait"?}` 또는 `op:"result"`(같은 요청 ID로 결과 재조회·재부착). 응답은 `accepted` → (`state`) → `result`, 오류는 `error`(`code`·고정 문구). 요청 ID는 호출자가 한 번 정해 재시도에도 같게 쓴다(`bot chat`이 만들어 준다). 연결이 끊겨도 run은 계속되고 결과·상태를 돌려주기 직전마다 지금의 멤버 권한을 다시 확인한다. CLI·HTTP 회신은 outbox가 아니라 상태 조회로 전달한다.

## HTTP 입구

아래는 JWT 웹훅의 요청·응답 규칙이다. SDK의 세 경로는 [OpenAI 호환 API](#openai-호환-api)를 따른다.

이 입구는 TLS를 직접 처리하지 않아 베어러 토큰이 평문으로 오간다: 기본은 루프백에서만 열리고, 밖에서 받으려면 TLS를 맡는 프록시 뒤에 두고 `http.remote`를 명시한다(위 표). `POST|PUT|PATCH|DELETE|GET /{입구 별칭}{트리거 path}`. `Authorization: Bearer <HS256 토큰>`(발급자·대상·만료·nbf 확인, `alg`는 HS256만, 키는 32바이트 이상)과 `Idempotency-Key`(재시도에도 같은 값)가 필요하다. 등록되지 않은 별칭은 인증 전에 404이고, 등록된 입구는 경로가 없어도 먼저 인증한다(401, 이유는 알리지 않는다). 인증과 접수는 같은 라우팅 표를 쓰고 키 캐시는 (프로필·바인딩·판본·자격 이름)별이다.

| 응답 | 조건 |
|---|---|
| 202 + `Location` | 기본(`accepted`) 또는 `sync` 기한 초과. 본문 `{request_id, state}` |
| 도구 결과 그대로 | `sync`: 도구 결과 `{status, body}`(status는 200·201·204·400·403·404·409·422)을 그대로 |
| 400·401·403·404·405·409·413·415·429(+`Retry-After`)·503 | 각각 잘못된 요청·인증·권한·없음·method·같은 키 다른 내용·본문 한도·미디어 형식·속도·저장소 장애 |
| 502 | 도구 결과가 고정 형식이 아님 |

상태 조회 `GET /{입구}/_requests/{id}`는 같은 주체의 결과만 돌려주고 새 도구 호출을 만들지 않는다. 응답 방식은 호출자가 고르지 않는다 — 최초 접수 때 저장소에 저장한 방식(`sync`·`accepted`)만 쓰므로 POST 재전달·재조회가 최초 응답과 같은 모양이다(그 뒤 트리거를 바꿔도 같다). 방식 기록이 없는 이전 요청은 `accepted` 모양으로 돌려준다. 결과·상태를 내보내기 직전마다 지금의 멤버 권한과 활성 판본(위)을 다시 확인한다.

## OpenAI 호환 API

호스트 설정에서 HTTP 입구의 종류를 `openai`로 지정하고 프로필 바인딩에 공개 모델 이름과 에이전트를 연결한다. SDK에는 `base_url`과 운영자에게 발급받은 API 키를 설정한다. 모델 이름은 `models.list()`로 확인할 수 있다. [바인딩 예시](profile-spec.md#openai-모델-바인딩)

```python
import os
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8788/v1", api_key=os.environ["BOT_API_KEY"])
model = client.models.list().data[0].id
reply = client.chat.completions.create(
    model=model,
    messages=[{"role": "user", "content": "오늘 일정을 알려줘"}],
    extra_headers={"Idempotency-Key": "agenda-request-1"},
)
print(reply.choices[0].message.content)
```

한 가지 일을 다시 조회할 때는 원래 메시지·모델·키를 사용한다. 새 일을 시작할 때는 새 멱등키를 쓴다. 키를 생략하면 서버가 난수로 생성해 응답의 `Idempotency-Key` 헤더에 담는다. 키 없이 첫 응답을 통째로 잃으면 클라이언트는 생성된 키를 알 수 없다. 외부 쓰기가 가능한 작업에는 첫 요청부터 키를 지정한다. SDK가 키 없이 재시도라고 표시한 요청은 400 `idempotency_key_required`로 끝난다. 본문이 같다는 이유로 서로 다른 요청을 합치지 않는다.

| 메서드와 경로 | 응답 |
|---|---|
| `GET /v1/models` | 인증된 멤버가 이 입구에서 실행할 수 있는 모델 목록. 각 모델의 `created`는 현재 프로필 판본을 처음 보관한 Unix 초다. |
| `GET /v1/models/{id}` | 같은 권한 검사 뒤 모델 하나. 없는 모델과 권한 밖 모델은 같은 404다. |
| `POST /v1/chat/completions` | JSON의 `choices[0].message.content`에 최종 답이 온다. 저장된 최종 답의 일부가 출력 상한으로 잘렸으면 `finish_reason:"length"`다. 실행기가 최종 답을 남기지 못하고 출력 한도로 실패하면 422 `run_failed`다. `stream:true`면 SSE로 받는다. |

모든 경로에 `Authorization: Bearer <API 키>`가 필요하다. 호스트의 키 슬롯은 비밀 보관소 이름과 HTTP 멤버를 연결한다. 키를 회전할 때는 같은 멤버에 새 슬롯을 추가한 뒤 옛 키를 철회한다. 게이트웨이는 인증할 때마다 보관소를 읽으므로, 보관소가 새 값을 반환하는 시점부터 회전이 반영된다. 모델 목록·최종 답·SSE 전달에도 현재 멤버 권한이 적용된다. 기본 주소는 루프백이다. 외부에 열려면 `gateway.http.remote`를 활성화하고 TLS 프록시를 둔다. 기본 CORS 허용은 없다. 요청 본문은 기본 256 KiB까지 받는다. 호스트의 `gateway.limits.body_bytes`를 설정하면 그 값(최대 1 MiB)을 사용한다. 웹훅의 기본값은 64 KiB다. 본문을 읽는 시간은 최대 10초이며, 요청 기한이 더 짧으면 그 안에 읽어야 한다.

### 대화와 승인

각 요청은 새 대화다. 클라이언트는 이전 턴을 포함한 `messages`를 매번 보낸다. 마지막 user 턴을 현재 질문으로 쓰며 나머지 턴은 원래 순서의 대화 문맥으로 한 번만 전달한다. 서버는 클라이언트가 붙인 system·developer·assistant 역할까지 모두 사용자 제공 대화 자료로 취급한다. 프로필 지침과 실행 권한은 서버가 결정한다.

승인이 필요한 도구는 요청을 접수한 바인딩의 지정 승인자가 승인한 뒤에만 실행한다. 위임한 작업에도 같은 바인딩과 요청자가 적용된다. 서버는 접수 때와 현재 프로필에서 승인자와 전달 경로가 일치하는지 확인한다. Slack 승인자는 자신의 DM에서, 로컬 승인자는 본인 인증을 거친 관리 화면에서 결정한다. HTTP 키에는 승인 권한이 없다. 승인자가 없거나 경로가 바뀌거나 DM 전달을 확인할 수 없으면, HTTP 응답에 해당 도구를 실행하지 않았다는 안내가 온다.

### 대기와 스트리밍

`wait_ms`는 인증 직후부터 접수를 포함한 응답까지의 단일 기한이다(기본 120초, 허용 범위 1~300000밀리초). 본문 읽기·접수·결과 대기에 쓸 수 있는 시간은 `wait_ms − min(2000, wait_ms / 4)`밀리초다. 나머지는 최종 권한 확인과 응답 전송에 쓴다. 너무 짧으면 첫 요청의 접수 여부를 확인하지 못해 422 `request_outcome_unknown`을 받을 수 있으므로 운영에서는 수 초 이상을 권장한다.

마지막으로 확인한 작업이 진행 중인 채 대기 시간이 끝나면, 서버는 권한을 다시 확인하고 HTTP 200으로 ‘처리 중’ 안내를 보낸다. 사람이 승인할 차례가 되면 즉시 200으로 승인 대기를 알린다. 두 경우 모두 작업은 유지된다.

같은 키와 본문으로 다시 POST하면 현재 상태나 최종 답을 받는다. 같은 키에 다른 본문을 보내면 400이다. 저장소 오류로 작업 상태를 끝내 확인할 수 없으면 422 `request_outcome_unknown`을 받는다. 조회에는 처음 요청한 주체와 입구의 키가 필요하다.

`stream:true`는 `text/event-stream`을 반환한다. 접수 뒤 기다리는 동안 서버는 최대 15초 간격으로 keep-alive 주석을 보낸다. 첫 청크에는 assistant 역할이, 이어지는 청크에는 저장된 최종 답의 텍스트가 담긴다. 종료 사유를 보낸 뒤 `data: [DONE]`으로 끝난다. `Cache-Control: no-cache`와 `X-Accel-Buffering: no`를 사용한다. 스트림이 이미 열린 뒤 실패하면 오류 객체를 보내고 닫는다. 이 버전은 모델 생성 중간의 토큰을 전송하지 않는다.

한 실행의 모든 모델 호출에서 입력·출력 토큰 수를 확인해야 `usage`를 반환한다. 한 번이라도 계량이 빠지면 JSON 응답에서 `usage`를 생략한다. JSON에서는 `prompt_tokens`, `completion_tokens`, 그 합인 `total_tokens`를 받는다. SSE에서는 `stream_options.include_usage:true`일 때 일반 청크의 usage는 null이고, 측정값이 있으면 `[DONE]` 직전에 빈 `choices`와 사용량을 담은 청크가 온다. 측정할 수 없는 실행에는 통계 청크가 없다.

### 허용하는 요청 옵션

| 입력 | 처리 |
|---|---|
| `model`, `messages` | 필수. system·developer·user·assistant 역할과 문자열 또는 text 파트 배열을 받는다. 한 메시지의 text 파트는 줄바꿈(LF) 하나로 잇는다. user 턴이 하나 이상 있어야 하며, 마지막 user 턴에는 공백 이외의 글자가 필요하다. |
| `stream`, `stream_options.include_usage`, `n` | 기본 비스트리밍. include_usage는 스트림에서만 쓴다. 선택지는 하나다(`n:1`). |
| `temperature`, `top_p`, `seed`, `user`, `metadata` | 타입과 범위를 확인하되 실행 설정·신원·감사에는 사용하지 않는다. |
| `store`, `response_format` | 생략, `store:false`, 텍스트 형식만 지원한다. 접수·복구에 필요한 작업 기록은 별도로 보관한다. |
| `max_tokens`, `max_completion_tokens`, `stop` | 호출별 길이·중단 조건은 지원하지 않는다. 값을 지정하면 400이다. |
| tools·functions·tool_choice, 이미지·오디오·파일, logprobs 등 | 400 `unsupported_parameter`. 서버에는 프로필이 허용한 도구만 있다. |

### 오류와 재시도

오류는 `{"error":{"message":"…","type":"…","param":null,"code":"…"}}` 형식이다. `param`에는 서버가 아는 필드 경로만 담는다. 호출자가 임의로 붙인 필드 이름은 최상위에서 `null`, 중첩 위치에서는 `messages[0]`처럼 알려진 부모 경로까지만 표시한다. 오류 본문에는 호출자가 붙인 이름이나 입력값이 노출되지 않는다.

| HTTP | type | code |
|---|---|---|
| 400 | `invalid_request_error` | `invalid_request`, `unsupported_parameter`, `idempotency_key_conflict`, `idempotency_key_required` |
| 401 / 403 | `authentication_error` / `permission_error` | `invalid_api_key` / `access_denied` |
| 404 / 405 | `invalid_request_error` | `model_not_found`, `not_found` / `method_not_allowed` |
| 413 / 415 | `invalid_request_error` | `request_too_large` / `unsupported_media_type` |
| 408 / 429 / 503 | `invalid_request_error` / `rate_limit_error` / `server_error` | 접수 전 `request_timeout` / `rate_limit_exceeded`(Retry-After 초) / `service_unavailable` |
| 422 | `server_error` | `request_outcome_unknown`이면 같은 키·본문으로 조회한다. 이미 종료한 작업이면 `run_failed` 또는 `run_cancelled`다. |

새 작업이 동시 실행 상한에 걸리면 `Retry-After: 1`로 1초 뒤 재확인을 안내한다. 요청 속도 제한에 걸리면 제한이 풀리기까지 남은 초를 올림해 반환한다. 같은 키로 이미 접수한 작업을 조회할 때는 새 실행 슬롯을 쓰지 않는다.

**인증 실패 한도.** 게이트웨이 프로세스마다 직접 연결한 IP별로 센다. 60초 안에 `Authorization` 헤더로 인증을 20번 실패하면, 그다음부터 60초간 같은 IP의 모델·채팅 요청은 키를 확인하기 전에 429를 받는다. 정상 키를 보내거나 기존 작업을 재조회해도 차단된다. `Retry-After`는 남은 차단 시간(초)이다. 헤더를 생략한 요청과 보관소 장애는 실패 횟수에서 제외한다.

TLS 프록시 뒤에서는 같은 프록시 IP를 거치는 사용자들이 이 한도를 공유한다. 게이트웨이는 `X-Forwarded-For`를 출처 판정에 사용하지 않는다. 사용자별 제한이 필요하면 프록시에서 적용한다. v2.0의 인증 실패 한도는 고정값이다.

인증을 마치고 접수한 작업의 결과를 반환할 때는 SDK가 자동 재시도하는 408·409·429·5xx를 쓰지 않는다. 커밋 여부를 확인할 수 없으면 422와 `x-should-retry:false`를 반환한다. 기존 JWT 웹훅은 입구 별칭이 포함된 경로에서 동작한다. [Chat Completions 공식 규격](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) · [모델 목록 공식 규격](https://developers.openai.com/api/reference/resources/models/methods/list)

현재 제공하는 경로는 모델 목록·상세와 Chat Completions다. 후속 Responses API는 같은 인증·모델 권한·실행 결과 조회를 사용할 수 있다. 토큰 단위 스트리밍을 추가할 때는 외부에 공개할 수 있는 텍스트만 순서대로 전달한다.

## Slack 입구

### Slack 앱을 처음 설정할 때

v2.0은 Slack 이벤트와 버튼 클릭을 Socket Mode로만 받는다. Slack의 HTTP Request URL 수신은 지원하지 않는다. 아래 표에서 사용할 기능의 행만 설정한다. [Slack Socket Mode 설정](https://docs.slack.dev/apis/events-api/using-socket-mode/)

| 사용할 기능 | Slack 앱에서 설정할 항목 | 게이트웨이가 사용하는 API·이벤트 |
|---|---|---|
| 연결 (필수) | **Socket Mode**를 켜고 **Basic Information → App-Level Tokens**에서 `connections:write`를 가진 앱 토큰을 발급한다. | 앱 토큰으로 `apps.connections.open`을 호출한다. 설치 팀과 봇 사용자는 봇 토큰으로 `auth.test`를 호출해 확인한다. |
| 메시지와 승인 카드 보내기 | **OAuth & Permissions → Bot Token Scopes**에 `chat:write`를 추가하고 봇을 사용할 채널에 초대한다. | `chat.postMessage`로 게시하고, 버튼을 누르면 `chat.update`로 카드를 갱신하거나 `chat.postEphemeral`로 클릭한 사람에게 안내한다. |
| 채널에서 봇 멘션 받기 | 봇 권한 `app_mentions:read`. **Event Subscriptions**를 켜고 **Subscribe to bot events**에 `app_mention`을 추가한다. | 등록된 채널에서 사람이 봇을 멘션한 글을 받는다. [멘션 이벤트](https://docs.slack.dev/reference/events/app_mention/) |
| 스레드 답글 받기 (`thread_reply`) | 공개 채널은 봇 권한 `channels:history`와 봇 이벤트 `message.channels`, 비공개 채널은 `groups:history`와 `message.groups`를 추가하고 앱을 대상 채널에 참가시킨다. 프로필의 채널 바인딩에 `events: [.., thread_reply]`가 필요하다. | 등록된 채널의 `message` 이벤트 중 **봇이 참여한 스레드**의 사람 답글만 받는다. [공개 채널](https://docs.slack.dev/reference/events/message.channels/) · [비공개 채널](https://docs.slack.dev/reference/events/message.groups/) |
| 반응 받기 (`reaction`) | 봇 권한 `reactions:read`와 `users:read`. 봇 이벤트에 `reaction_added`를 추가한다. 프로필의 채널 바인딩에 `events: [.., reaction]`가 필요하다. | 등록된 채널의 메시지(`item.type=message`)에 **지금 활성인 사람 계정**이 단 반응을 받는다. 멤버 확인 뒤 반응한 사람을 `users.info`로 확인한다(아래). [반응 이벤트](https://docs.slack.dev/reference/events/reaction_added/) · [사용자 조회](https://docs.slack.dev/reference/methods/users.info/) |
| 사람이 쓴 1:1 DM 받기 | 봇 권한 `im:history`. 봇 이벤트에 `message.im`을 추가하고 **App Home → Messages Tab**에서 사용자 메시지 입력을 허용한다. 호스트의 해당 워크스페이스에도 `dm: true`와 프로필의 DM 바인딩이 필요하다. | `message.im` 구독은 `type: message`, `channel_type: im`인 이벤트를 전달한다. [DM 이벤트](https://docs.slack.dev/reference/events/message.im/) |
| 승인·거절 버튼 쓰기 | **Interactivity & Shortcuts → Interactivity**를 켠다. | Socket Mode로 `block_actions`를 받는다. |
| 채널에서 만든 예약의 승인 카드를 소유자 DM으로 보내기 | 봇 권한 `im:write`를 추가한다. 게시에는 위의 `chat:write`, 클릭에는 Interactivity가 필요하다. | `conversations.open`으로 소유자 한 명의 DM을 연다. 이 카드의 버튼은 일반 DM 구독을 꺼 둔 설치에서도 동작한다. [DM 열기](https://docs.slack.dev/reference/methods/conversations.open/) |
| 회신에 파일 첨부하기 | 봇 권한 `files:write`를 추가한다. | `files.getUploadURLExternal`로 업로드 주소를 받고, 파일을 올린 뒤 `files.completeUploadExternal`로 공유한다. [파일 업로드](https://docs.slack.dev/reference/methods/files.getUploadURLExternal/) |

권한을 정한 뒤 **OAuth & Permissions**에서 앱을 워크스페이스에 설치한다. 이미 설치한 앱의 권한을 바꿨다면 다시 설치한다. 앱 토큰과 봇 토큰은 프로필 비밀 보관소에 저장하고, `gateway.slack.workspaces.<별칭>`에는 각각 `app_token`·`bot_token`의 자격 이름과 `team_id`, 사용할 채널 ID를 적는다.

### 수신·승인·발송 동작

- **인증된 설치** 하나 = (프로필, 워크스페이스 별칭)의 자격으로 연 Socket Mode 세션 하나. hello의 앱 ID와 `auth.test`의 팀을 확인한다. 이 소켓으로 온 이벤트는 같은 프로필의 바인딩으로만 라우팅하고 밖으로 폴백하지 않는다. 같은 앱 토큰을 프로필 둘이 쓰면 이벤트가 나뉘어 한쪽이 놓치므로 기동을 거절한다.
- 받는 것: 채널은 바인딩의 `events`로 고른다(생략하면 봇 멘션 `app_mention`만). `thread_reply`는 봇이 참여한 스레드의 사람 답글, `reaction`은 사람이 단 반응이고 트리거에서는 각각 `on.event`의 `thread-reply`·`reaction`이다([프로필 명세](profile-spec.md#slack-채널-이벤트-고르기)). DM은 사람이 쓴 `message`(수정·삭제·봇 글 제외). 요청 ID는 `채널.ts`라 어떤 이벤트·재전달로 와도 같은 글은 한 요청이다. 접수가 확정됐을 때만 ack한다. 편집·삭제·봇·앱 글, 봇 자신의 반응, 다른 팀, 등록되지 않은 채널, 모양이 틀린 `ts`는 작업을 만들지 않는다.
- **스레드 답글**: 봇 참여 증거는 (설치 팀·봇 ID·프로필·바인딩·채널·루트 `ts`)다. 봇이 보낸 글의 게시 영수증, 또는 이 소켓이 인증한 자기 봇 글을 관찰했을 때만 기록한다(루트 글이면 그 `ts`, 스레드 안이면 `thread_ts`). 수신자가 준 `thread_ts`만으로는 들이지 않는다. 봇 자신의 글은 증거로만 쓰고 트리거를 실행하지 않는다. 증거가 없는 스레드의 답글은 받지 않고(ack만, `thread_unverified`), 증거 저장소를 읽지 못하면 ack하지 않아 Slack이 다시 보낸다. 게시 영수증은 이 버전에서 올린 글부터 증거가 되므로, 이 버전 이전에 열린 스레드는 봇이 거기에 다시 글을 올린 뒤부터 답글을 받는다(소급해 채우지 않는다).
- **같은 이벤트는 접수 하나**: `app_mention`과 `message.channels`는 같은 글을 서로 다른 `event_id`로 보낼 수 있다. 글의 **내용**으로 가르므로 도착 순서가 결과를 바꾸지 않는다: 봇 멘션이 있고 `mention`이 켜져 있으면 멘션(우선), 아니면 스레드 답글이다. 접수는 Slack 이벤트의 식별 — 글은 (팀·채널·글 ts), 반응은 (팀·`event_id`) — 으로도 한 건이라, 바인딩 이름이 바뀌거나 프로필이 바뀐 뒤의 재전달도 첫 접수의 run으로 수렴하고 두 번째 실행을 만들지 않는다. 이 식별이 생기기 전(이전 버전)에 받은 글도 업그레이드 마이그레이션이 임시 키를 달아 두어, 그 글이 다시 오면 새 접수 없이 그 run으로 수렴한다.
- **반응**: 반응한 사람(`event.user`)이 행위자이자 요청자이고 프로필 멤버여야 한다. 대상은 `item.channel`·`item.ts`다. 이 반응을 받는 트리거가 없으면 접수 없이 ack만 한다. 트리거 본문의 고정 필드는 `reaction`·`channel`·`item_ts`·`item_user`(원글 작성자, 행위자와 다를 수 있다)이고 요청 ID는 Slack `event_id`다. 멤버 신원과 증거는 본문에서 복원하지 않는다.
  - **사람 확인**: Slack은 반응 이벤트에 행위자가 봇인지 알려 주지 않는다. 멤버 확인 뒤 같은 설치의 봇 토큰으로 `users.info`를 **이벤트마다** 부르고(성공한 판정을 기억하지 않아, 비활성화한 계정은 바로 다음 반응부터 받지 않는다), 응답의 ID가 물은 ID와 같고 `is_bot`이 `false`이며 `deleted`가 생략됐거나 불리언 `false`인 계정만 받는다(`deleted`가 `null`·문자열이면 판정할 수 없어 받지 않는다). 봇 자신과 `USLACKBOT`은 따로 거른다. `is_app_user`는 거절 근거가 아니다. 멤버가 아닌 사람의 반응은 Slack을 부르지 않는다.
  - 사람이 아니거나 확인하지 못하면(`users:read` scope 부족·속도 제한·조회 실패·2초 시간 초과·응답의 ID나 필드를 알 수 없음) 후보·기본 대화·승인 증거 없이 고정 사유(`actor_not_human`·`actor_scope_missing`·`actor_unverified`)만 로그에 남기고 ack한다(Slack의 ack 기한 3초 안에 끝난다). 일반 멘션·DM에는 `users:read`가 필요 없다.
  - **회신 스레드**: 봇이 참여한 것으로 확인된 스레드(반응이 달린 글이 그 루트)에서만 그 스레드로 회신한다. 반응이 달린 글의 `ts`를 스레드 루트로 짐작하지 않는다 — 답글에 달린 반응이나 확인되지 않은 글의 반응은 채널 최상위로 회신한다.
- **처리하지 않은 이벤트 넘기기**: 인증된 Slack 이벤트의 직접 도구 트리거는 성공 결과의 `_meta["bot/pass"]: true`(빈 `content`)로 «내가 처리할 일이 아니다»를 알릴 수 있다. 접수가 일치한 직접 트리거를 order 순 후보로 고정하고, 호스트는 PASS 확정·호출 결론·커서 전진·감사를 한 PG 커밋으로 기록한 뒤 다음 후보를, 마지막 후보까지 넘기면 기본 대화를 정확히 한 번 부른다. 기본 대화는 첫 에이전트 지시 트리거이고, 없으면 원래 바인딩의 에이전트가 사건 입력을 받는다: 멘션·답글은 글 그대로, 반응은 검증한 반응 이름과 글 ts만 담은 고정 형식(`{"event":"reaction_added","item_ts":…,"reaction":…}`)이다. 기본 대화는 에이전트 실행이라 요청자에게 `run` 스코프가 있어야 한다 — 사슬을 만들 때와, 마지막 후보가 넘길 때 **현재 판본과 실행이 고정한 판본 둘 다에서** 다시 확인한다. 없으면(도구만 허용된 멤버, 접수 뒤에 `run`이 철회된 멤버) 마지막 후보가 넘겨도 대화 없이 조용히 끝난다. 판별용 읽기만 한 호출만 넘길 수 있다. [표식 규칙](plugins.md#처리하지-않은-이벤트-넘기기-botpass)
- 첨부는 외부 업로드 3단계(주소 → 바이트 → 완료)이고 게시는 완료 호출뿐이다. 승인 카드와 첨부는 한 회신에 함께 보내지 않는다. 푸시 대화(채널·DM) 루트 에이전트 답의 줄 맨 앞에 둔 `<첨부 이름="…">…</첨부>` 블록은 본문(마크다운)과 분리해 뒤따르는 `text/plain` 첨부 회신으로 같은 스레드에 올린다(본문이 비면 첨부 내용을 본문으로 올리고, 위임 자식·CLI·HTTP는 원문을 유지한다).

## 자격 보관소와 명령

`BOT_SECRETS`: `env`(기본·개발용, `BOT_SECRET__<프로필>__<이름>`에서 `-`를 `_`로) · `keychain`(macOS, 서비스 `bot/<프로필>`·계정 `<이름>`) · `sops:<파일>`(`sops --decrypt`한 JSON의 `bot.<프로필>.<이름>`). 핸들은 만들 때 프로필이 고정되어 다른 프로필을 읽을 수 없다. 프로세스는 선언된 비밀 이름 전체를 시작 때 읽어 가림 목록에 넣고, 이후 `BOT_SECRETS_REREAD_SECS`초마다(기본 60, 0이면 끈다) 다시 읽어 목록을 새로 한다. 키체인은 읽을 때마다 `security`를, SOPS는 30초 기억이 끝났을 때 `sops`를 부르므로 그에 맞게 늘려 쓴다. 역할의 입력은 가림 목록을 거치는 보관소(`Observe.secrets`)만 받는다 — 날 보관소를 넘기면 컴파일이 되지 않는다.

| 명령 | 하는 일 |
|---|---|
| `bot validate` | 프로필·호스트 등록부·입구 표·필요한 자격(프로필이 선언한 것·도구가 받는 것·입구가 쓰는 것·프로필이 허용한 `claude-cli` 별칭의 모델 서비스 키 — 없으면 이름과 환경 변수명)·HTTP 키 길이(32바이트 이상)·기억 시드의 저장소 상한(본문 8 KiB)·도구 서버 설치본을 DB·Slack 접속 없이 점검하고 활성화 판본과 서버 판본을 보여 준다(문제가 하나라도 있으면 «통과»를 알리지 않고 문제만 보여 준다) |
| `bot chat [--profile P] [--thread T] [--request-id ID] [--no-wait] "글"` | 새 요청을 보낸다. 연결이 끊기면 `bot chat --profile P --attach --request-id ID`로 같은 실행의 결과를 받는다. 종료 코드 3이면 재시도할 수 있다. 터미널에 찍는 결과·오류 문구는 모델·도구가 만든 글이므로, 줄바꿈과 탭을 뺀 제어 문자(ESC 시퀀스·8비트 C1·BEL·CR·DEL 등)를 `\u{1b}` 같은 보이는 표기로 바꿔서 찍는다 — OSC 52(클립보드 쓰기)·창 제목·화면 지우기가 터미널에서 실행되지 않는다(`\r\n`은 줄바꿈 하나). |
| `bot map [--json] [--profile P]` | 구조 지도(이름·분류·개수만) |
| `bot init <이름> [--template personal\|team\|company] [--dir <폴더>]` | 작업 폴더 `<이름>/`(호스트 설정 예시 `host.json` + 프로필 `profiles/<이름>/`)를 새로 만들고, 만든 직후 `bot validate`를 통과한다. 이름은 소문자·숫자·하이픈이다. 이미 있는 폴더·파일은 덮어쓰지 않고(링크도 따라가지 않는다) 거절한다. 검증은 `$TMPDIR` 아래 우리가 만든 임시 사본에서 프로필 로더로 하고(같은 바이트, 끝나면 치운다), 어긋나면 사용자의 폴더에는 아무것도 만들지 않는다. 통과한 바이트는 `--dir`(준 경로를 한 번만 열고, 경로 속 링크는 따르며, 연 뒤에 이름을 바꿔 쳐도 바깥에 쓰지 않는다) 아래 임시 폴더(`.bot-init-…`)에 쓰고 `<이름>`으로 한 번에 옮긴다 — 이미 있으면 실패한다. 임시 폴더는 처음부터 최종 권한(`0777 & !umask`)으로 만들어 나중에 바꾸지 않으며, 열자마자 소유자·권한·비어 있음을 확인하고, 치울 때는 우리가 만든 이름만 지운다(강제 종료하면 임시 폴더가 남을 수 있다). `BOT_*` 환경 설정은 읽지 않는다. 호스트 설정 예시는 데모 모델과 지금 OS 사용자의 CLI 계정뿐이다. |
| `bot add agent <계보 경로> [--dir <프로필 폴더>]` · `bot add skill <이름> [--agent <계보 경로>] [--dir <프로필 폴더>]` | `AGENT.md`·`SKILL.md` 뼈대를 알맞은 자리에 새로 만든다(예: `main/cs` → `agents/main/agents/cs/`, `--agent`를 생략한 스킬은 프로필 `skills/`). `tools.yaml`은 만들지 않고 `profile.yaml`의 권한 상한은 읽지도 고치지도 않는다(안내만 출력한다). 상위 에이전트가 없거나, 같은 이름이 이미 있거나, 같은 이름의 스킬이 보이는 범위(프로필·조상·자손)에 있으면 거절한다. 프로필 폴더는 한 번만 열어 그 fd 기준으로만 읽고 쓰며, 중간에 실패하면 만든 빈 폴더만 지우고 파일은(이름으로 안전하게 지울 수 없어) 남긴 채 남은 경로를 알린다. |
| `bot doctor [--host <host.json>] [--profiles <폴더>] [--check-db]` | 호스트 설정·모델 제공자·도구 서버·프로필·입구·필요한 자격 **이름**·DB 주소 설정을 한국어로 요약한다. 기본은 네트워크 접속·마이그레이션·모델·도구 서버 시작 없이 설정만 보며, 판정은 운영 경로의 함수를 그대로 쓴다(호스트 해석·모델 별칭(API 직접 호출의 주소·키 규칙 포함)·`state: required` 거절·서버 판본(로컬·원격)·원격 서버의 IP 리터럴 정책·시드·활성화 판본·필요한 자격 목록(수집 헤더·원격 서버 bearer 자격 포함)·주소 구문 분석). 원격 서버의 이름 풀이와 연결은 접속하지 않으므로 확인하지 않는다. `sops` 보관소는 복호화가 필요해 «미검사». `--check-db`를 주면 DB에 짧게(5초) 접속해 연결만 확인한다(쓰기 없음). 자격 값과 DB 주소·접속 오류 글은 출력하지 않고, «미검사»는 «통과»가 아니며, 실패 항목이 하나라도 있으면 종료 코드 1이다. |
| `bot mcp sync <스킬 폴더>` | `BOT_PROFILES` 아래 프로필의 스킬 폴더에서, tools.yaml이 선언한 도구의 정의(이름·설명·입력 스키마·sha256)를 서버(원격 `transport: "http"`)의 `tools/list`에서 받아 `tools.lock.yaml`에 적는다. 선언한 도구만 적고 서버가 돌려준 정의는 믿지 않는다(어휘·자격 값 검사를 통과해야 쓰고, 결과는 결정적이라 바뀐 게 없으면 다시 쓰지 않는다). 실행할 때 브로커가 같은 해시를 비교해 달라졌으면 호출 전에 막는다. [도구 정의 잠금](plugins.md#도구-정의-잠금-toolslockyaml) |
| `bot profile-bundle put <프로필 폴더>` | 옛 판본의 원본 프로필 폴더를 저장소에 보관한다. 이전 버전이 보관하지 않은 판본을 진행 중 작업이 아직 쓰고 있어 `bot serve`가 복원하지 못할 때 쓴다. 폴더를 검증해 만든 묶음은 자기 해시를 판본으로 보관하므로 프로필 ID와 해시가 같은 원본만 복원에 쓰인다(호스트 등록부는 보지 않는다). 보관한 묶음은 불변이라 손상된 묶음은 저장소에서 그 행을 지운 뒤 다시 보관한다. |
| `bot serve --role … [--activate]` | 역할 시작. 순서: 검증 → 활성 판본 결정(읽기만) → 판본 복원 → 역할 점검·리스너 확보 → 활성 판본·시드 확정 → 접수 열기. 판본 복원은 새 집합의 검증한 묶음을 저장소에 먼저 보관하고(같은 바이트면 다시 보관해도 된다), 진행 중 작업(끝나지 않은 실행·재개할 수 있는 dead letter·대기 카드·정적 cron 구간·활성 사용자 예약)이 고정한 옛 판본을 보관한 묶음에서 되살려 등록한다. 되살리지 못하면 판본마다 프로필 ID와 이유를 알리고 시작하지 않으며 활성 판본·실행·카드·시도 횟수는 바뀌지 않는다. 점검(자격·HTTP 키 길이·입구·Slack 설치·시드 상한)이나 리스너 확보(소켓·포트 사용 중)가 실패하면 저장소의 운영 판본은 바뀌지 않는다(시드는 저장소와 같은 검증 `bot_store::check_seed`로 저장소에 접속하기 전에 막고, 게시할 때도 판본과 기억 시드를 한 트랜잭션으로 올려 한 건이라도 거절되면 둘 다 그대로다. 같은 판본에 합류할 때는 아무것도 쓰지 않는다). 판본: 저장소의 활성 판본이 폴더·입구 매핑과 같으면 쓰기 없이 합류, 아직 없으면 첫 게시(bootstrap), 다르면 `--activate`일 때만 예상값 CAS로 교체한다 |

환경: `BOT_PROFILES`·`BOT_HOST_CONFIG`·`BOT_DATABASE_URL`·`BOT_SOCKET`·`BOT_SECRETS`·`BOT_SECRETS_REREAD_SECS`·`BOT_INSTALLS`·`BOT_ADMIN_TOKEN_FILE`·`BOT_LOG`. 접수·회신·승인은 `core::trace` 스팬으로 남고(`bot.private`로 민감 DM 표시) 본문·인자·nonce·영수증은 싣지 않는다. **`BOT_LOG`(기본 `warn`, `error`·`warn`·`info`·`debug`·`trace`)는 이 프로그램의 크레이트에만 그 수준으로 적용한다.** 웹소켓·HTTP·TLS 같은 외부 라이브러리는 `BOT_LOG`가 무엇이든 경고까지만 내고, DB 드라이버(sqlx)는 아예 끈다(접속 주소를 구문 분석하다 모르는 매개변수의 값을 경고에 싣기 때문이다) — 이 라이브러리들은 `trace`에서 오가는 프레임·본문을 그대로 기록하므로(예: 웹소켓 라이브러리의 «Received message …») 민감 DM 원문이나 요청·응답 본문이 stderr에 남기 때문이다(`bot_cli::logging`, 시험은 trace로 실행해 로그에 원문 0을 확인한다). 이 프로그램이 직접 남기는 로그는 식별자·분류 낱말만 싣는다.

## 역할별 운영

`--role all`은 `gateway`, `scheduler`, `worker`, `admin`을 함께 시작한다. 역할을 나눌 때는 같은 PostgreSQL과 활성 집합을 사용한다.

- **게이트웨이**는 사용자 접수와 회신을 맡는다. 민감 DM 트리거가 있으면 지정 도구의 srt 실행기와 프로필별 `private-intake` 지문 키가 필요하다. 본문은 지정 도구에만 전달한다. 처리 여부가 불명확하면 운영자 보류로 남긴다.
- **워커**는 모델과 도구 실행 브로커를 함께 운영한다. 허용한 Claude 모델의 서비스 키가 없으면 활성 판본을 게시하기 전에 멈춘다. 개별 도구 자격이 없으면 이름만 경고하고 시작한다. 해당 호출은 승인 카드를 열기 전에 거절되며 감사에 `secret_missing`이 남는다. `bot validate`는 두 종류의 누락을 모두 보고한다. [CLI 거절과 재개](reliability.md#cli가-승인이나-자식을-기다릴-때)
- **스케줄러**는 발생 시각에 맞춰 실행을 접수한다. 정적 트리거는 어느 역할이 게시하든 활성 판본·기억 시드와 한 커밋에 등록된다. 삭제한 트리거의 구간은 교체 시각에 닫히며 전환 전 발생은 해당 구간에 남는다. [재배포 시 옛 판본의 제약](profile-spec.md#재배포-뒤-판본-복원-계약)
- **관리 역할**은 기본 `127.0.0.1:8787`에서 열린다. 인증된 데이터 요청은 응답 직전에 현재 활성 집합을 다시 확인한다. 교체됐거나 확인할 수 없으면 `503`과 오류 코드만 반환한다. HTTP 접수의 기본 주소는 `127.0.0.1:8788`이다. [관리자 인증과 시작](admin.md#시작-순서)

Slack 버튼과 관리 화면은 같은 브로커에 승인 결정을 맡긴다. 외부 도구의 설치 판본을 확인할 수 없으면 승인 카드를 대기 상태로 두고 일시 오류를 반환한다.

### Slack 예약의 목적지

등록부에 올린 수신자는 그 프로필 멤버 누구나 예약 목적지로 쓸 수 있다. 제한하려면 등록부에서 뺀다. 예약을 만들려면 해당 멤버에게 `schedule.create` 권한이 있어야 한다. Slack 사용자는 같은 프로필에 등록한 채널을 별칭·채널 ID·정규 경로로 지정할 수 있다. CLI 사용자는 원래 대화만 사용할 수 있고, HTTP 주체는 사용자 예약 도구를 호출할 수 없다.

1. 생성할 때 등록부에서 바인딩과 `팀ID/채널ID`를 확정한다. 소유자가 그 경로·스레드·지시문을 확인하고 승인해야 예약이 생긴다. Slack의 실시간 채널 멤버십은 조회하지 않는다.
2. 발화할 때 현재 등록부를 다시 확인한다. 경로가 빠지면 예약을 닫고 원래 대화에 한 번 알린다. 일시 오류면 같은 발생을 다시 확인한다.
3. 발송할 때는 완성된 전송 초안을 별도로 승인받는다. 승인 소비 직전에 등록부를 다시 대조한 뒤 outbox에 넣는다. Slack 어댑터는 전송 직전에도 팀과 채널을 확인하고 API에는 채널 ID만 보낸다. [미발송 결과와 영수증](reliability.md#다른-수신자에게-보낼-예약-답)

**채널에서 시작한 예약의 승인 DM.** v2.0 계약은 원래 채널 바인딩에 연결된 Slack 설치로 예약 소유자 한 명의 1:1 DM을 연다. 원래 대화가 이미 소유자의 DM이면 그 대화를 사용한다. 채널에는 DM 게시를 확인한 뒤 고정 안내 한 줄만 보낸다. 이 경로는 승인 전용이며 일반 회신의 수신자나 DM 메시지 접수 권한을 바꾸지 않는다. [전달 실패·재시작·버튼 검증 규칙](reliability.md#slack-소유자에게-승인-카드를-전달할-때)

### 배포 중인 모델 요청

Claude 워커는 닻 VM이 만든 게이트웨이 주소에 릴레이를 열고 실제 리스닝 포트를 모델 VM에 전달한다. 상류 요청 직전에 현재 활성 집합을 확인한다. 구판이면 403, 저장소 장애나 확인 시간 초과면 503을 반환하며 상류 요청은 보내지 않는다. 열린 스트림도 1초마다 집합을 확인하고, 확인이 지연되면 첫 확인 예정 시각부터 10초 안에 결론이 나야 계속한다. 구판이나 시간 초과가 확인되면 고정 오류 이벤트로 끝난다. [VM 설정](architecture.md#호스트-설정과-역할-조립) · [워커 종료와 리스 인계](reliability.md#워커-종료와-리스-인계)

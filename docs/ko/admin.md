# Admin UI와 관측성

[English](../admin.md) | 한국어

`bot serve --role admin`은 axum Admin API와 내장 UI를 제공합니다. `bot-admin`은 PostgreSQL에 직접 접근하지 않으며, 모든 읽기와 변경은 `bot_core::admin` 포트(`AdminRead`, `AdminAct`)와 `Broker::decide`를 통해서만 수행되고 감사 항목이 상태 변경과 함께 원자적으로 커밋됩니다. UI와 API는 단일 인증 파이프라인을 공유합니다.

## 접근 범위

인증은 단일 시크릿—브라우저의 HTTP Basic(사용자 이름은 무시) 또는 도구의 `Authorization: Bearer`—을 사용하며 `Viewer{principal, operator}`에 바인딩됩니다. 자격 증명이 설정되지 않으면 어떤 경로도 마운트되지 않습니다(기본 닫힘). 비루프백 바인드 주소에는 명시적 `remote: true`가 필요합니다.

- **운영자 (Operator)** (호스트당 1명): 모든 프로필의 구조, 실행, 보류(hold), 사전 승인, 감사 로그, 상태를 조회하고, 보류 해결·실행 재개/취소·사전 승인 일시 중지/재개를 수행합니다.
- **기타 주체**: 자신의 실행과 자신이 요청했거나 승인자로 지정된 승인만 조회할 수 있습니다. 다른 주체의 실행은 `404`를 반환해 존재 여부를 숨깁니다.

## 인증 파이프라인

모든 요청은 호스트, 속도 제한, 자격 증명 검사를 거치며, `POST` 변경 요청은 CSRF 검증도 추가로 통과해야 합니다.

1. **`Host` 검사**: DNS 리바인딩을 막기 위해 허용 목록(바인드 주소, 루프백 이름, 설정된 `hosts`) 밖의 `Host` 헤더를 `421`로 거부합니다.
2. **IP별 실패 제한**: 60초 내 실패 횟수가 `max_failures`(기본 `5`)를 초과하면 다른 클라이언트에 영향을 주지 않고 해당 피어 IP를 60초간 차단합니다(`429`). 소켓 피어 주소만 사용하며 `X-Forwarded-For`나 `Forwarded`는 절대 읽지 않습니다(테스트로 고정됨; 프록시 뒤에서는 프록시 IP가 단일 버킷을 공유). 자격 증명이 없는 요청(초기 브라우저 프롬프트)은 세지 않으며 잘못되거나 형식 오류가 있는 자격 증명만 횟수에 포함합니다.
3. **시크릿 일치 검증**: 시크릿을 해시하여 모든 항목과 상수 시간으로 비교합니다(실패 시 `401`).
4. **CSRF 보호 (`POST`)**: 인증된 `GET /v1/admin/me`가 발급한 뷰어 바인딩 `X-Bot-Csrf`, `Content-Type: application/json`, 동일 출처 헤더(`Origin` / `Sec-Fetch-Site`)를 요구하며 실패 시 `403`을 반환합니다.

오류 본문에는 고정 어휘 단어 하나(`invalid`, `denied`, `stale`, `changed`, `csrf`, `not_found`, `conflict`, `unavailable`, `no_body` 등)만 담기며 입력을 절대 되돌려 보내지 않습니다. 응답 크기는 512 KiB로 제한됩니다. `leak_values` / `leak_redactor`가 설정되어 있으면 출력 전에 디코딩된 JSON 키와 문자열 값을 검사하여 알려진 시크릿이—따옴표·백슬래시·줄바꿈이 포함된 형태라도—담긴 응답을 차단합니다.

## Admin API

| 경로 | 접근 권한 | 동작 |
|---|---|---|
| `GET /v1/admin/me` | 전체 | 현재 `principal`, `operator` 여부, `csrf` 토큰 반환. |
| `GET /v1/admin/runs` | 전체 (범위 제한) | 페이지네이션된 실행 목록(`profile`, `state`, `root`, `limit`, `after`). |
| `GET /v1/admin/runs/{profile}/{run}` | 전체 (범위 제한) | 계보(루트~현재), 하위 실행, 도구 호출(오류 코드: `denied`, `timeout`, `invalid_result`, `outcome_unknown`, `execution_failed`; 원시 페이로드 없음), 이펙트, 시도별 트레이스 ID 및 링크. |
| `POST /v1/admin/runs/{profile}/{run}/resume` | 운영자 | `dead` 상태의 일반 루트 실행을 같은 실행 ID로 재개. |
| `POST /v1/admin/runs/{profile}/{run}/cancel` | 운영자 | 종료되지 않은 일반 루트 실행과 하위 트리 취소 요청. |
| `GET /v1/admin/approvals` | 전체 (범위 제한) | 승인 목록(기본 `?state=pending`, `decided`, `all`), 브로커가 렌더링한 대상/변경 사항, `args_sha256`. |
| `POST /v1/admin/approvals/{nonce}/decide` | 지정 승인자 | 대기 중인 카드 승인 또는 거부. |
| `GET /v1/admin/holds` | 운영자 | 보류된 호출과 단계(보존된 영수증 포함). |
| `POST /v1/admin/holds/{profile}/{run}/{call}/resolve` | 운영자 | 보류 해결(본문에 `hold_id` 필수). |
| `GET /v1/admin/preapprovals` | 운영자 | 활성 사전 승인 목록 `{"items":[{profile, agent, trigger, grant_id, epoch, paused, used_today, max_per_day}]}`(쿼리 파라미터 없음; UI가 프로필별 필터링). |
| `POST /v1/admin/preapprovals/{profile}/{agent}/{trigger}/pause` | 운영자 | 사전 승인 트리거 긴급 일시 중지; 정확한 본문 `{"grant_id","epoch"}`. |
| `POST /v1/admin/preapprovals/{profile}/{agent}/{trigger}/resume` | 운영자 | 일시 중지된 트리거 재개; 정확한 본문 `{"grant_id","epoch"}`. |
| `GET /v1/admin/audit` | 운영자 | 해시 체인 감사 로그(최신순; 다음 페이지 존재 여부를 판별하려 1행을 더 읽어 `next`를 정확히 설정; 저장소 최대 201행). |
| `GET /v1/admin/audit/verify` | 운영자 | 제네시스부터 전체 감사 해시 체인 재계산. |
| `GET /v1/admin/status` | 운영자 | 프로필별 실행 수, 큐 대기 시간, 대기 승인, 보류, 트레이스/로그/이슈 익스포터 상태. |
| `GET /v1/admin/map` | 운영자 | 구조 맵과 접근 요약. |
| `GET /ui` | 전체 | 내장 UI(동일 출처 스크립트/스타일만 허용; 인라인 코드 없음). |

### 실행 재개와 취소

일반 루트 실행에 대한 운영자 실행 상세 응답은 `run.state`, `run.generation`과 함께 `control` 객체를 포함합니다: `claim_epoch`(같은 세대 내 재점유에서도 증가), `held`(하위 트리에 미해결 보류 존재 여부), `cancel_requested`. `cancel_requested`가 `true`이면 활성 실행 취소는 막히지만 `dead` 실행은 여전히 재개할 수 있습니다(커밋 시 `cancel_requested` 초기화).

두 엔드포인트 모두 인증, CSRF, 동일 출처, 활성 세트 펜스를 강제합니다. 관측한 필드와 새 `command_id`를 함께 전송하세요(예: `{"command_id":"<id>","state":"dead","generation":2,"claim_epoch":4}`). 성공 시 `200 {"ok":true}`를 반환합니다. 응답 유실 후 동일한 본문을 재전송하면 이후 세대가 진행되었더라도 `200 {"ok":true}`를 반환합니다. 커밋된 각 동작은 운영자, 대상, 명령 다이제스트, 고정 사유 `operator_requested`로 감사 행 하나를 기록합니다.

- `400 invalid`: 잘못된 본문 또는 식별자.
- `403 denied` / `403 csrf`: 비운영자 또는 CSRF/출처 검증 실패.
- `404 not_found`: 없는 실행 또는 비공개 실행.
- `409 stale`: 관측한 `state`, `generation`, `claim_epoch`가 변경됨.
- `409 conflict`: 다른 페이로드로 `command_id`를 재사용했거나 유효하지 않은 상태 전이.
- `409 held`: 하위 트리에 미해결 보류가 있음(**Approvals & Holds**에서 먼저 해결).
- `503 unavailable`: 저장소 또는 활성 세트 확인 실패.

### 사전 승인 관리

`GET /v1/admin/preapprovals`는 활성 사전 승인당 한 행을 반환합니다(`epoch`, `used_today`, `max_per_day`는 JSON 정수; `paused`는 불리언; `used_today`는 현재 UTC 날짜 기준). `pause`와 `resume`은 인증, 운영자 권한, JSON, `X-Bot-Csrf`, 동일 출처, 활성 세트 펜스, `{grant_id, epoch}` 일치를 강제하며 갱신된 단일 행(래핑 없음)을 `200`으로 반환합니다.

- 같은 `(grant_id, epoch)`에서 이미 해당 상태일 때 동일 명령을 다시 보내거나, 중간 전이 없이 응답 유실 후 재시도하면 새 감사 행이나 `epoch` 증가 없이 `200`을 반환합니다.
- `pause → resume → pause` 이후나 그랜트 교체 뒤에 이전 일시 중지를 재전송하면 `409 stale`을 반환합니다.
- `/`가 포함된 에이전트 ID(예: `main/developer`)는 단일 경로 세그먼트로 퍼센트 인코딩해야 합니다(`main%2Fdeveloper`). 서버는 라우팅 후 한 번만 디코딩합니다.
- 오류: `400 invalid`, `403 denied`, `404 not_found`, `409 stale`, `503 unavailable`.

일시 중지가 커밋되면 새 스케줄 발생, 도구 시작, 쓰기 그랜트 모두 실행되지 않습니다. 재개 전에 예정되었던 발생분은 나중에 따라잡아 실행되지 않고 건너뜁니다(건너뛴 발생마다 감사 기록 작성; 재개 이후 다음 예정 시각부터 실행 재개). 이미 인가된 쓰기의 결과 기록과 보류 복구는 정상적으로 계속됩니다.

## 활성 세트 펜스

`AdminConfig::fence(hook)`가 설정되어 있으면(`bot-cli`가 현재 프로세스의 활성 세트를 저장소의 게시 리비전과 비교) 모든 `/v1/admin/*` 응답은 **반환 직전에** 훅을 실행합니다. 세트가 달라졌거나 검사가 `fence_timeout`(기본 3초)을 초과하면 핸들러 응답을 버리고 `503 {"error":"unavailable"}`로 교체하여 구버전 프로세스가 낡은 맵·카드·실행 정보를 제공하지 못하게 합니다.

- 변경 엔드포인트는 핸들러 **이전에도** 펜스를 실행해 구버전 프로세스가 브로커나 저장소에 닿지 못하게 합니다. 핸들러 이후 펜스가 실패했다면 커밋이 이미 성공했을 수 있으며, 동일 결정을 재시도하면 중복 적용 없이 `repeated: true`를 반환합니다(저장소 커밋도 자체 펜스를 강제).
- 인증(`Host`, 실패 제한, 시크릿, CSRF)이 먼저 실행되므로 거부된 요청은 훅을 호출하지 않고 일반 `401`/`403`/`421`/`429`를 반환합니다. 정적 UI 자산(`/ui`)은 펜스를 건너뜁니다. 훅이 없으면 이 계층은 무동작입니다. 읽기는 훅을 1회, 변경은 2회 호출합니다.

## 승인

승인과 실행 목록은 커서 페이지네이션 전에 필터를 적용하고 행 오프셋이 아닌 정렬 키로 페이지를 나누므로, 페이지 사이에 행이 바뀌어도 중복이나 누락이 생기지 않습니다.

| `state` | 범위 | 정렬 순서 |
|---|---|---|
| `pending` (기본값) | 결정 대기 중 | 만료 임박 오름차순, 다음 `nonce` 오름차순. |
| `decided` | 대기 외 전체(만료/대체 포함) | 결정 시각 내림차순(null 마지막), 다음 `nonce` 오름차순. |
| `all` | 전체 승인 | `decided` 행을 `decided` 순으로 먼저, 그다음 `pending` 행. |

API로 결정하려면 `GET /v1/admin/me`에서 받은 `X-Bot-Csrf`, `Content-Type: application/json`, 본문 `{"allow":true,"seen":"<args_sha256>"}`(거부는 `allow: false`)와 함께 `POST /v1/admin/approvals/{nonce}/decide`를 호출합니다.

- 만료되었거나 대체된 카드는 `409 stale`, `seen` 다이제스트 불일치는 `409 changed`를 반환합니다.
- 지정 승인자만 결정할 수 있습니다(그 외 `403` — 운영자도 타인을 대신해 승인할 수 없으며, 모델과 서비스 주체는 브로커가 거부).
- 같은 승인자가 같은 `seen`으로 같은 판정을 반복하면 브로커를 다시 호출하지 않고 `200`과 `repeated: true`를 반환하며, 판정을 뒤집으면 `409`를 반환합니다.
- 결정이 기록되면 워커가 깨어나고, 브로커가 현재 권한을 재검증한 뒤 저장소가 승인을 원자적으로 소비하고 호출을 시작합니다(또는 예약 전달용 아웃박스 행을 적재).

## 소유자 전용 승인 카드

기본 `bot serve`는 로컬 운영자로 Admin UI에 로그인합니다. v2.0에서 Slack 소유자는 채널 원본 예약을 Slack 1:1 DM으로 승인하며, Admin UI는 Slack 로그인을 지원하지 않습니다([신뢰성](reliability.md)).

메모리 공유/저장 카드(`memory.share`, `memory.save`)에는 비공개 메모리 본문이, 예약 전달 카드(`schedule.deliver`)에는 외부 발송 초안 전문이 담깁니다. 이 본문은 **뷰어 == 요청자 == 지정 승인자**일 때만(운영자 포함) 표시됩니다. 다른 모든 뷰어는 `render: []`와 `render_hidden: true`가 설정된 메타데이터만 받습니다. 실행 상세, 감사, 상태, 맵에는 카드 본문이 절대 포함되지 않습니다.

- **메모리 공유 (`memory.share`)**: 정확히 하나의 `Memory ID` 행과 `Body` 행이 필요합니다(이어지는 줄 결합; 원문 텍스트 또는 JSON 문자열; 제목/범위 등 다른 행은 자유). 승인(`allow: true`)은 본문이 보이고 `memory_share_hash(memory_id, sha256(body))`가 `args_sha256`과 일치할 때만 허용됩니다. 누락·숨김·잘림·불일치 시 `approve_blocked: "no_body"`가 설정되고 UI의 **Approve** 버튼이 비활성화되며 `allow: true` 요청은 브로커나 감사에 닿기 전에 `403 no_body`를 반환합니다. **거부(`allow: false`)는 항상 허용됩니다**(미지정 뷰어는 권한 없음으로 먼저 거부).
- **예약 전달 (`schedule.deliver`, `bot_core::schedule::DELIVERY_CARD_LABEL`, 위험도 `irreversible`)**: 정확히 하나의 `Delivery draft` 행이 필요하며, 결합된 값이 `DeliveryDraft::approval_json()`의 정확한 정규 JSON(JSON 문자열로 감싸지 않은 원시 문서)이고 SHA-256이 `args_sha256`(`DeliveryDraft::sha256()`)과 일치해야 합니다. 잘림·편집·불일치 시 `approve_blocked: "no_body"`와 `allow: true`에 대한 `403 no_body`가 반환됩니다(거부는 허용). UI는 이 행의 텍스트를 변형 없이 그대로 표시합니다.

## 보류와 감사

보류(hold)는 외부 쓰기 결과를 확인할 수 없을 때 발생합니다. 다단계 도구는 확인된 단계의 영수증을 보존하고 미해결 단계만 보류합니다. `0025` 롤링 업그레이드 중에는 구버전 관리자 프로세스가 확인되었거나 미해결 단계가 있는 호출의 전체 호출 `not_executed`를 롤백하므로, 업데이트된 Admin UI에서 단계별 영수증과 최종 호출 영수증을 해결하세요.

- **판정**: `confirmed`(대상 측 `receipt` 필수) 또는 `not_executed`(`evidence` 필수) — 각각 제어 문자가 없는 1–512자 한 줄 문자열.
- **다단계 호출과 최종 결과**: 열린 각 단계를 개별적으로 해결합니다. 모든 단계가 `not_executed`로 해결되면 호출은 현재 권한과 승인을 거쳐 재개됩니다. 최종 호출 응답 없이 `confirmed`된 단계가 하나라도 있으면 마지막 열린 단계를 해결할 때 **Final call outcome** 카드가 열립니다(개별 단계 영수증은 계속 표시). 이전 도구와 하위 프로세스가 종료되었는지 확인한 뒤 대상 측에서 검증된 전체 호출 영수증을 입력하면, 브로커가 다음 점유 시 이를 반환합니다. 확인된 단계가 있는 호출에 `verdict: "not_executed"`를 보내면 `400 invalid`를 반환합니다. 실행을 취소하면 열린 도구 보류가 원자적으로 닫히며(`run.cancelled` 감사 + 종료 시각), 운영자 판정은 `hold.resolve`를 기록합니다. 미실행으로 표시된 단계는 재시작(`null`) 전까지 `Marked not executed`(`resolution: "not_executed"`, 선택 불가)로 표시됩니다.
- **`HoldView.kind`별 `not_executed` 후 동작** (저장소가 원본 행에서 설정하며 API는 도구 이름이나 호출 ID를 검사하지 않음):
  - `tool`: 이전 프로세스 종료와 대상 측 미실행을 **둘 다** 확인해야 합니다. 열린 단계가 남았으면 대기하고, 확인된 단계가 있으면 **Final call outcome**을 열며, 아니면 같은 호출 ID로 재개합니다.
  - `reply`: 응답을 처음부터 다시 보냅니다. 예외적으로 레거시/미설정 `missing_transport` 응답은 `not_executed`를 `409 conflict`로 거부합니다(UI는 재전송 폼을 숨김). DB에 `bot_outbox.transport`를 채운 뒤 `not_executed`로 표시하거나, `bot_outbox.body`를 수동 전달하고 `confirmed`로 닫으세요.
  - `private`: 메시지 본문을 저장하지 않으므로 재시도할 수 없습니다. 미실행으로 닫히며 사용자가 새로 요청해야 합니다(`confirmed`는 완료로 종료).
- **`hold_id` 고정**: 저장소는 보류에 진입할 때마다 새 `hold_id`를 발급합니다(전체 호출/최종/응답/비공개는 `HoldView.hold_id`; 열린 단계는 `EffectView.hold_id`이며 호출 수준 `hold_id`는 비어 있음). 해결 페이로드에 `hold_id`가 필수이며(누락/형식 오류 시 `400`) `(profile, run, call, effect)`와 원자적으로 대조합니다: 미존재 → `404`, 대상 불일치 → `409`, 닫힌 `hold_id`에 대해 같은 운영자가 같은 판정과 영수증/증거로 동일 재전송 → `200 repeated: true`(이후 새 보류가 열렸더라도 추가 감사/스팬 없음), 운영자나 값이 다름 → `409`. `hold_id`가 없는 대상은 해결 버튼(및 Enter 제출)을 비활성화하고 **Reload**를 안내합니다.
- **종류 및 프로필 격리**: UI는 카드와 폼을 `(profile, run, call, kind)`로 키잉하여(`crates/bot-admin/ui/holdpin.js` `keyOf`) 같은 호출 ID에 공존하는 레거시 `tool` + `reply` 보류를 분리합니다. 저장소는 모든 보류·단계·승인·실행을 `(profile, run, call[, effect])`로 키잉합니다(`crates/bot-admin/tests/suite/hold_pairs.rs`, `crates/bot-admin/tests/suite/hold_profiles.rs`, `crates/bot-admin/tests/browser/hold_pair.mjs`). 테스트 더블도 운영 환경과 동일하게 판정과 다이제스트만 저장합니다.
- **2단계 UI 고정**: 되돌릴 수 없는 보류 판정은 두 번 눌러 확정합니다(`crates/bot-admin/ui/holdpin.js`). 첫 클릭에서 대상 단계, `hold_id`, 판정, 입력값을 고정하고, 두 번째 클릭에서 고정된 값만 전송합니다. 동시 목록 갱신, `404`/`409` 응답, 단계 선택 변경 시 확인 상태와 입력이 모두 초기화됩니다(`crates/bot-admin/tests/js/holdpin.test.mjs`, `crates/bot-admin/tests/browser/hold_pin.mjs`, `crates/bot-admin/tests/browser/hold_stale_click.mjs`).
- **감사 체인**: 순번, 이전 해시, 주체, 동작, 대상, 인자 해시, 판정을 연결합니다. `GET /v1/admin/audit/verify`는 제네시스부터 재계산해 처음 깨진 순번을 찾아냅니다(DB 전체 재작성은 방어하지 못함).

## 내장 UI

`/ui`는 `build.rs`를 통해 `crates/bot-admin/ui/`에서 컴파일 시 내장된 빌드리스 정적 HTML/CSS/JS를 제공합니다(매니페스트는 `crates/bot-admin/tests/suite/assets.rs`로 고정). 엄격한 동일 출처 CSP를 적용하며 테스트가 외부 URL, HTML 주입, 인라인 스크립트/스타일을 금지합니다.

| 탭 | 대상 | 내용 |
|---|---|---|
| Structure | 운영자 | 의존성 및 공유 리소스(`needs`) 엣지가 포함된 대화형 카드 맵(이동, 확대/축소, 검색, 종류 필터). |
| Runs | 전체 | 범위 제한된 실행 목록 및 상세 패널(계보, 하위 실행, 도구 호출, 이펙트, 트레이스 링크), 운영자 재개/취소. |
| Approvals & Holds | 전체 (보류: 운영자) | 승인 카드(지정 승인자 전용, `seen` 해시 검증) 및 2단계 보류 해결 폼. |
| Pre-approvals | 운영자 | 상태, UTC 일일 사용량 미터, 긴급 일시 중지/재개 제어([사전 승인 UI](#사전-승인-ui)). |
| Access | 운영자 | 역할, 멤버, 정책 상한, 도구 위험도/승인/스코프/시크릿 이름, 외부 송신 도구. |
| Audit | 운영자 | 해시 체인 로그 및 **Verify chain** 동작. |
| Status | 운영자 | 프로필별 실행 수, 큐 대기 시간, 대기 승인, 보류, 관측성 익스포터 상태. |

라이트/다크 모드와 `prefers-reduced-motion`을 따르며, 입력 중이거나 2단계 확인이 준비된 동안 5초 자동 새로고침을 일시 정지합니다. 비운영자에게는 운영자 탭을 숨깁니다(서버에서도 `403` 차단). 어휘 레이블은 `crates/bot-admin/ui/labels.js`에 정의됩니다(알 수 없는 코드는 `Other` 표시).

**Runs** 상세에서 `dead` 루트 실행의 **Resume**은 관측 상태를 고정한 채 12초 이내 두 번 클릭(600ms 오클릭 방지)이 필요하며, **Cancel run**은 1회 확인 프롬프트를 사용합니다. 하위 트리에 보류가 있으면 **View in Approvals & Holds**가 표시됩니다. 응답이 유실되면 자동 새로고침을 멈추고 **Resend same command** 또는 **Reload latest state**를 제공합니다.

### 구조 맵

활성 `ProfileCatalog`에서 결정적 리비전(`rev`) 아래 8종 카드(profile, agent, skill, tool, trigger, binding, scope, secret)와 6종 엣지(delegates, uses, owns, handles, wakes, needs)로 구성됩니다. 이름, 분류, 개수만 노출하며 지침, 지식 본문, 환경 변수, 시크릿 값은 절대 노출하지 않습니다. 상속된 스킬은 정의된 에이전트에 한 번만 표시됩니다. 에이전트 요약에는 설명 첫 줄(≤ 140자, 없으면 모델명)과 비상속 `builtin` 읽기 전용 도구(스키마는 `read`/`grep`/`glob` 허용; `claude-cli` VM은 `read`만 지원)가 표시됩니다.

### 사전 승인 UI

운영자 전용 **Pre-approvals** 탭은 활성 `preapproved` 정적 트리거마다 상태(`Active`/`Paused`), UTC 일일 쓰기 사용량(`used_today / max_per_day`), 그랜트 ID 접두사(12자), 일시 중지 `epoch`, **Emergency pause** / **Resume** 버튼을 표시합니다(프로필 선택기는 클라이언트 측 필터링).

- **원클릭 일시 중지, 투클릭 재개**: **Emergency pause**는 즉시 전송됩니다. **Resume**은 첫 클릭에서 대상·`grant_id`·`epoch`를 고정하고 12초 내 재확인을 요구하며(600ms 내 클릭 무시), 고정된 값만 전송하고 목록 변동·Esc·Cancel·탭 전환·타임아웃 시 해제됩니다.
- **서버 권위 상태**: 낙관적 변경을 하지 않으며 행당 진행 중 요청을 1건으로 제한하고(15초 타임아웃), 대상·`grant_id`·기대 `paused` 상태가 일치하는 응답만 수락한 뒤 항상 목록을 다시 불러옵니다.
- **형식 오류/중복 행**: 필수 필드 검사(문자열 4개, 정수 3개, 불리언 1개)에 실패하거나 `(profile, agent, trigger)`가 중복된 행은 버튼 없이 건너뛰고 경고 개수에 합산합니다. 목록 응답 자체가 손상되었으면 경고를 표시하고 이전 행을 유지합니다.
- **경계와 테스트**: 서버가 운영자 역할, CSRF, 동일 출처, 활성 세트 펜스, `{grant_id, epoch}` 일치를 강제합니다. `crates/bot-admin/tests/js/preapproval.test.mjs`, `crates/bot-admin/tests/js/preapprovals.view.test.mjs`(`crates/bot-admin/tests/js/support/contract.mjs`), 수동 브라우저 스크립트 `crates/bot-admin/tests/browser/preapproval_stub.mjs` 및 `crates/bot-admin/tests/browser/preapproval_stop.mjs`로 검증됩니다.

## 관측성

### OTLP 트레이싱 (`admin.trace`)

워커, 브로커, 게이트웨이는 완료된 스팬을 차단이나 실패 없이 `bot_core::trace::Tracer`에 동기 제출합니다.

- **결정적 Trace ID**: `(profile, run, generation)`에서 도출되므로 분리된 역할들이 IPC 전파 없이 상관관계를 맺습니다. 새 세대나 다른 프로필은 별개 트레이스가 됩니다.
- **고정 어휘**: `gen_ai.*`, `bot.*`, `user.id`, `session.id`만 내보냅니다(논스, 원시 인자, 영수증, 비공개 DM 텍스트 제외). `worker.model`(`Client`)은 `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, 토큰 수를 기록하고, `worker.run`은 `user.id`, `session.id`, `gen_ai.conversation.id`를 기록합니다(기본 가명화). HTTP 접수 스팬은 호출자가 보낸 경로가 아니라 매칭된 **등록 트리거**의 `http.request.method` / `http.route`(미매칭 시 `_OTHER` / `unmatched`)만 기록합니다.
- **마스킹과 콘텐츠 정책**: 알 수 없는 키는 내보내기 시 버려집니다. 콘텐츠 속성은 프로필별 옵트인(`content: false` 기본값)이며 해당 프로필의 수집기로만 전송되고, 알려진 시크릿/토큰 패턴을 마스킹한 뒤 8 KiB로 제한합니다. 실시간 `Redactor`는 `SecretStore`에서 읽은 모든 시크릿을 사용 전에 등록하고, 선언된 모든 시크릿 이름(호스트 관측성 시크릿 포함)을 `BOT_SECRETS_REREAD_SECS`(기본 60초, `0` 비활성화; 읽기당 5초 타임아웃, 무응답 저장소 시 최대 10배 백오프)마다 갱신합니다. 교체된 시크릿과 다른 역할만 쓰는 시크릿도 한 번 읽히면 트레이스·로그·이슈·Admin API 응답 전반에서 마스킹되며, 이전 값도 프로세스 수명 동안 계속 마스킹되고, 배치 본문은 익스포터 인증 헤더를 읽은 *뒤*에 직렬화됩니다. `bot.private=true`는 `content: true`여도 콘텐츠를 억제합니다(플래그 자체는 내보내지 않음). 식별자는 기본적으로 프로필별 해시(`ids: "hash"`; 원문 유지 시 `ids: "plain"`)를 사용합니다.
- **철회와 활성화 펜스**: `admin.trace`, `admin.logs`, `admin.issues`는 활성화 지문에 포함됩니다(비활성 섹션은 생략되므로 미사용 호스트의 지문은 변하지 않음; 표시용 `name`과 `link`는 제외). 관측성 변경이나 철회에는 `--activate`가 필요합니다. 구버전 프로세스는 대기 항목을 버리고(`dropped`, `last_error: stale`), 저장소 장애나 3초 타임아웃 시 안전 차단(fail-closed)합니다. 펜스는 배치마다 두 번—익스포터 자격 증명 읽기 전과 HTTP 전송 직전—검사합니다(이미 송신된 요청은 회수 불가).
- **격리, 회복력, 샘플링**: 각 프로필은 독립된 유한 큐, 워커, 서킷 브레이커(1초–60초 지수 백오프), 전송 대상을 가집니다. 256 KiB를 초과한 스팬이나 큐 가득 참은 실행에 영향을 주지 않고 깔끔히 폐기합니다. 헤더 시크릿은 전송마다 해당 프로필의 `SecretStore`에서 읽습니다(오류 시 미인증 전송 안 함). 모든 `Exporter` 핸들이 드롭되면 대기 스팬을 플러시하고 저장소 참조를 해제합니다. Trace ID 기반 결정적 샘플링으로 분리 역할 간 일관성을 유지합니다.
- **전송**: OTLP/HTTP protobuf(`application/x-protobuf`).
  - `https://`(Langfuse, LangSmith, 원격 OTLP): `remote: true` 없이 OS 루트 인증서를 사용하며, 리다이렉트·재시도·연결 재사용을 끄고, 사용자 정의 헤더를 민감 표시하며, `localhost`/`*.localhost`는 DNS 조회 없이 루프백에 고정합니다.
  - 평문 `http://`: 루프백(`127.0.0.0/8`, `::1`), 사설(`10/8`, `172.16/12`, `192.168/16`, `fc00::/7`), 링크 로컬(`169.254/16`, `fe80::/10`), `localhost`/`*.localhost`(원래 `Host` 유지한 채 루프백 고정)에 한해 `remote: true` 없이 허용됩니다. 그 외 평문 호스트나 DNS 이름은 명시적 `remote: true`가 필요합니다.

### OTLP 로그 (`admin.logs`)와 이슈 추적 (`admin.issues`)

둘 다 동일한 전송 계층, 서킷 브레이커, 활성화 펜스, `Redactor`를 공유합니다.

- **구조화 stderr 및 OTLP 로그 (`admin.logs`)**: `BOT_LOG_FORMAT=json`은 줄 단위 `{ts_ms, level, target, message, fields}`를 stderr에 기록하며(기본 `text`), `BOT_LOG`로 필터링됩니다(워크스페이스 크레이트 전용; 외부 크레이트는 `warn` 상한; `sqlx` 비활성). `admin.logs`를 켜면 같은 이벤트를 최소 `level`(`debug`, `info`, 기본 `warn`, `error`; `BOT_LOG` 범위 내) 기준으로 OTLP/HTTP(S) protobuf(`ExportLogsServiceRequest`, 기본 경로 `/v1/logs`) 배치 전송합니다. 본문과 필드는 마스킹되고 크기가 제한됩니다(본문 2 KiB, 값 256자, 필드 32개).
- **이슈 트래커 (`admin.issues`, `kind: "sentry"`)**: Sentry, GlitchTip, Bugsink 및 호환 백엔드용 Sentry Envelope v7(`POST <endpoint>`, `Content-Type: application/x-sentry-envelope`, `X-Sentry-Auth`)으로 `bot_core::issue::IssueReporter`를 구현합니다. 자동 보고 대상: (1) `tracing::error!`, (2) 패닉(`fatal`), (3) 비정상 실패 스팬(예상된 거부/취소인 `cancelled`, `denied`, `invalid`, `rate_limited`, `forbidden`, `unregistered`, `no_route`, `too_large`, `parked`, `lost`는 제외). 실패 스팬은 `contexts.trace.trace_id`로 연결하며 `bot.private` 스팬에는 추가 태그를 생략합니다. 루프/폭주 방지: 익스포터 실패는 로그를 남기지 않고 카운터/`last_error`만 갱신하며, 관측성/HTTP 전송 타깃은 건너뛰고 `(kind, target)` 기준으로 5초 쿨다운 중복 제거를 적용합니다.
- **호스트 범위 시크릿 참조**: `admin.logs.headers` 시크릿 객체와 `admin.issues.key`는 `{"secret": "<name>"}`(`bot/host/<name>`에서 해석) 또는 `{"profile": "<profile>", "secret": "<name>"}`을 허용합니다(`bot validate`/`bot doctor`가 검사하고 `bot serve`가 마스킹기에 사전 적재). `admin.issues.key`는 루프백 평문 `http://` 목(mock)에서만 생략할 수 있습니다.
- **상태**: `GET /v1/admin/status`는 `process.logs`와 `process.issues`에 `{enabled, queued, sent, dropped, failed, circuit, last_error, backends}`를 노출합니다(`origin`만 표시하며 경로·헤더·키는 절대 노출하지 않음).

## 호스트 설정 (`admin` 섹션)

`BOT_HOST_CONFIG`의 `admin` 객체는 `AdminSettings::from_json`으로 전달됩니다. 알 수 없는 키와 잘못된 값은 시작 전에 실패하며, `{}`는 로컬 기본값을 사용하고, 시크릿 값은 절대 포함하지 마세요.

| 키 | 기본값 | 계약 |
|---|---|---|
| `bind` | `127.0.0.1:8787` | Admin 리스너 `host:port`. 비루프백에는 `remote: true` 필요. |
| `remote` | `false` | 비루프백 `bind`에 대한 명시적 운영자 옵트인(`trace`/`logs`/`issues` 아래에서는 비사설 평문 `http://` 내보내기 옵트인; `https://`에는 불필요). |
| `hosts` | `[]` | 리버스 프록시 뒤에서 추가로 허용할 `Host` 값(`host:port`). |
| `max_failures` | `5` | 60초 차단이 걸리기 전 피어 IP당 60초 내 허용 인증 실패 횟수(범위 `1..=100`). |
| `trace.<profile>` | 끔 | 프로필별 OTLP 트레이스 익스포터(`null`로 비활성화): 필수 `endpoint`, 선택 `name`(기본 `default`), `link`(`{trace_id}` 포함 `http(s)` 템플릿), `headers`(문자열 또는 해당 프로필 `SecretStore`의 `{"secret": "<name>"}`), `content`(`false`), `ids`(`hash` 또는 `plain`), `sample`(`0.0..=1.0`, 기본 `1.0`), `remote`(`false`). |
| `logs` | 끔 | 호스트 전역 OTLP 로그 익스포터: `endpoint`, `name`, `headers`(문자열 또는 `{"secret": "<name>"}` / `{"profile": "<p>", "secret": "<name>"}`), `level`(`warn`), `remote`(`false`). |
| `issues` | 끔 | 호스트 전역 이슈 트래커: `kind`(`sentry`), `name`, `endpoint`, `key`(`{"secret": "<name>"}` 또는 `profile` 포함; 루프백 `http://`에서만 생략 가능), `environment`, `release`, `sample`(`1.0`), `remote`(`false`). |

로컬 수집기 설정 예(`host.json`의 `admin` 객체):

```json
{
  "bind": "127.0.0.1:8787",
  "remote": false,
  "hosts": [],
  "max_failures": 5,
  "trace": {
    "personal": {
      "endpoint": "http://127.0.0.1:4318/v1/traces",
      "name": "local-collector",
      "headers": {"Authorization": {"secret": "collector-token"}, "X-Env": "dev"},
      "link": "https://traces.example/t/{trace_id}",
      "content": false,
      "ids": "hash",
      "sample": 1.0
    }
  }
}
```

Langfuse, LangSmith, OTLP 로그, Sentry를 함께 구성한 예:

```json
{
  "bind": "127.0.0.1:8787",
  "trace": {
    "personal": {
      "name": "langfuse",
      "endpoint": "https://cloud.langfuse.com/api/public/otel/v1/traces",
      "headers": {
        "Authorization": {"secret": "langfuse-auth"},
        "x-langfuse-ingestion-version": "4"
      },
      "link": "https://cloud.langfuse.com/trace/{trace_id}",
      "ids": "hash",
      "content": false,
      "sample": 1.0
    },
    "work": {
      "name": "langsmith",
      "endpoint": "https://api.smith.langchain.com/otel/v1/traces",
      "headers": {
        "x-api-key": {"secret": "langsmith-key"},
        "Langsmith-Project": "bot-work"
      }
    }
  },
  "logs": {
    "name": "otel-logs",
    "endpoint": "https://otlp.example.com/v1/logs",
    "headers": {"Authorization": {"secret": "otel-logs-token"}},
    "level": "warn"
  },
  "issues": {
    "kind": "sentry",
    "name": "sentry",
    "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/",
    "key": {"secret": "sentry-key"},
    "environment": "production",
    "release": "bot@0.1.0",
    "sample": 1.0
  }
}
```

## 시작 절차

1. `bot serve --role admin`을 시작합니다. 운영자 신원은 현재 OS 사용자에 매핑된 CLI 계정에서 결정됩니다(여러 개면 알파벳순 첫 번째).
2. 시작 시 `Admin UI: http://<addr>/ui`가 출력되고 일회성 운영자 시크릿이 터미널에 한 번 표시됩니다. `/ui`를 열고 HTTP Basic 프롬프트에 임의의 사용자 이름과 해당 시크릿을 입력하세요. 비대화형 실행 시에는 다른 사용자가 쓸 수 없는 디렉터리 아래 절대 경로로 `BOT_ADMIN_TOKEN_FILE`을 설정합니다. 시크릿은 권한 `0600`으로 기록되고 정상 종료 시 삭제됩니다.
3. `GET /v1/admin/me`에서 신원과 역할을 확인합니다. 승인 버튼은 로그인한 주체가 지정 승인자일 때만 동작합니다.

토큰 파일 게시는 경로별 잠금을 획득하고 복구 레코드를 기록한 뒤 시크릿을 게시합니다. 활성 세트 게시에 실패하면 파일은 이전 상태로 롤백됩니다. 비정상 종료 후 다음 시작 시 복구 레코드와 파일 신원을 대조하며, 운영자가 파일을 교체해 이전 상태를 검증할 수 없는 경우 파일을 보존하고 시작을 중단합니다.

Admin UI를 다른 호스트 바이너리에 내장하려면 `bot_admin::bind`, `router`, `serve`를 사용하세요(`serve`는 IP별 로그인 속도 제한에 필요한 피어 `ConnectInfo`를 연결합니다).

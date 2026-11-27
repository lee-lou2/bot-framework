# 프레임워크 확장

[English](../extending.md) | 한국어

수정 위치는 확장 유형에 따라 달라집니다. 각 섹션은 **컴파일러가 안내하는 곳**(`cargo check`가 잡아줌)과 **조용히 누락되는 곳**(문자열, 스키마, SQL 제약, 문서 — 빼먹어도 컴파일됨)을 구분합니다. 조용히 누락되는 곳을 먼저 확인하고, 파일 개수에 의존하지 말고 `scripts/parity_audit.py`로 누락 여부를 검증하세요. 용어 정리는 [용어집](glossary.md)을 참고하세요.

| 확장 유형 | Rust 변경 | 섹션 |
|---|---|---|
| 일반 도구 (MCP 서버) | 없음 — 설정만 변경 | [일반 도구](#일반-도구) |
| OpenAI 호환 서버 (Ollama, vLLM, Gemini 호환) | 없음 — 설정만 변경 | [모델 프로바이더](#모델-프로바이더) |
| 새 모델 API 와이어 프로토콜 | 런타임 와이어 모듈 + `bot-cli` 프로바이더 파일 | [모델 프로바이더](#모델-프로바이더) |
| 채팅 채널 | 여러 크레이트 | [채팅 채널](#채팅-채널) |
| `SecretStore` 백엔드 | `bot-cli`만 변경 | [SecretStore 백엔드](#secretstore-백엔드) |
| 샌드박스 백엔드 | 도구 샌드박스: `bot-broker` + `bot-cli` 연결 | [샌드박스 백엔드](#샌드박스-백엔드) |
| 트리거 유형 | 프로필, 게이트웨이, 관리자 | [트리거 유형](#트리거-유형) |
| 내장 도구 | 중앙 테이블(`bot-core`) + 크레이트별 핸들러 | [내장 도구](#내장-도구) |

## 일반 도구

Rust 변경은 필요 없습니다. MCP 서버를 만들고 설정 4곳을 갱신하세요(`bot validate`가 불일치를 이름별로 알려줌).

1. 서버를 설치하고 `host.json`의 `servers`에 등록합니다([플러그인](plugins.md), `crates/bot-cli/src/install.rs`에서 파싱).
2. 스킬 디렉터리에 `tools.yaml`(`schemas/v1/tools.schema.json`)을 추가하고 `bot mcp sync`로 `tools.lock.yaml`을 생성합니다.
3. `profile.yaml`에서 `roles`, `policy.scopes`와 (승인이 필요하면) `policy.approve`를 부여합니다([프로필 명세](profile-spec.md)).
4. 에이전트 `env.yaml`과 `secrets`를 연결합니다.

## 모델 프로바이더

- **OpenAI 호환 서버:** Rust 변경 없음 — `host.json`에 `kind: "openai-api"`로 `providers.<alias>`를 정의합니다([아키텍처](architecture.md)).
- **새 API 와이어 프로토콜** (예: 네이티브 Gemini):
  - **컴파일러:** `crates/bot-runtime/src/model/api/wire.rs`(모든 `ApiKind` 분기)와 `crates/bot-cli/src/execution/provider.rs`(종류 라벨, 활성화 핑거프린트 태그 바이트, `parse_api` 키).
  - **새 파일:** `crates/bot-runtime/src/model/api/wire/` 아래 요청/응답 코덱.
  - **조용한 누락:** `parse_providers`(`provider.rs`)의 `kind` 문자열 `match`와 `docs/architecture.md`.
  - **주의:** **새** 핑거프린트 태그 바이트를 할당하고 기존 바이트는 절대 바꾸지 마세요(바꾸면 모든 배포에서 재활성화가 강제됨). `crates/bot-cli/src/execution/fingerprint_tests.rs`에 고정 입력 테스트로 못 박아 두세요.
- **새 VM 호스팅 CLI 프로바이더** (새 `Provider` 변형):
  - **컴파일러:** `crates/bot-cli/src/execution/provider.rs`의 `impl Provider`(자격 증명 이름, VM 플래그, 설명, 핑거프린트 입력)와 `crates/bot-cli/src/serve/models.rs`의 `build`.
  - **조용한 누락:** `crates/bot-cli/src/serve/claude.rs`의 프록시 테이블과 CLI 설정은 `Provider::ClaudeCli`만 읽습니다(추가 누락 시 업스트림 부재로 별칭이 거부됨).
  - **제약:** 브로커 프런트(`crates/bot-broker/src/front.rs`)는 Anthropic `/v1/messages`만 중계하므로 다른 CLI 프로토콜은 프런트를 먼저 확장해야 합니다.

## 채팅 채널

### `Adapter` 변형 추가

- **컴파일러:**
  - `crates/bot-profile/src/model.rs`(`entrance`, `adapter_name`)와 `crates/bot-profile/src/activate.rs`(호스트 별칭 일치).
  - `crates/bot-gateway/src/entrance.rs`(`Entrance`, `Real`, `Conflict::fix`).
  - `crates/bot-gateway/src/routes.rs`(`reply_transport` → `PushTransportId`)와 `crates/bot-gateway/src/schedule_routes.rs`(`slack_destination` 명시적 대상 분기).
  - `crates/bot-admin/src/map.rs`.
  - `crates/bot-cli/src/validate.rs`(`entrance_secrets`)와 `crates/bot-cli/src/serve/gateway.rs`(리스너 기동).
- **조용한 누락:**
  - `schemas/v1/profile.schema.json`의 `bindings` `oneOf`, `crates/bot-profile/src/profile.rs`의 `adapter` `match`(누락 시 웹훅으로 폴백하지 않고 `unknown-adapter` 반환).
  - 호스트 설정: `crates/bot-gateway/src/host.rs`(구조체 + 검증), `crates/bot-gateway/src/mapping.rs`(활성화 핑거프린트 — 새 섹션이 없는 설정은 동일한 바이트를 유지해야 함), `crates/bot-profile/src/host.rs`의 `HostCatalog`, `crates/bot-cli/src/host.rs`의 `catalog()`.
  - 어댑터 접수 모듈(수신 → `Inbound` → `Ingress`)과 응답 `Sender`(`crates/bot-gateway/src/delivery.rs`, 여기서 `Sender::scope()`는 고정 `PushTransportId`를 담은 `DeliveryScope` 반환)를 `crates/bot-cli/src/serve/gateway.rs`에 연결.
  - 채널 전용 테이블이 필요하면 마이그레이션 추가(이벤트 중복 제거 예시: `crates/bot-store/migrations/0022_slack_events.sql`).
  - 문서: [게이트웨이 운영](gateway.md) 호스트 설정 표와 [프로필 명세](profile-spec.md) 바인딩 섹션.

### 관련 타입과 아웃박스 라우팅 계약

- **새 `ConversationKind`:** 컴파일러가 `crates/bot-store/src/codec.rs`, `crates/bot-store/src/delivery.rs`, `crates/bot-store/src/user_schedule.rs`, `crates/bot-runtime/src/worker/span.rs`, `crates/bot-gateway/src/inbound.rs`, `crates/bot-gateway/src/schedule_routes.rs`, `crates/bot-broker/src/runner.rs`, `crates/bot-admin/src/json.rs`를 지적합니다. **조용한 누락:** 새 마이그레이션으로 `crates/bot-store/migrations/0001_runs.sql`의 `conv_kind` `CHECK` 제약을 확장하세요.
- **새 `IdentityKind`:** 컴파일러가 **아무것도** 지적하지 않습니다(`==`/`matches!`로 비교하고 `RunOrigin::from_ingress`가 `_ =>`로 끝남). `crates/bot-core/src/identity.rs`, `crates/bot-core/src/state.rs`, `crates/bot-profile/src/approval.rs`, `crates/bot-store/src/api.rs`를 수동으로 점검하세요(`grep -rn IdentityKind crates/*/src`).
- **응답 아웃박스 라우팅 계약** (모든 푸시 어댑터 필수):
  1. **안정적인 `PushTransportId`:** `reply_transport`(`crates/bot-gateway/src/routes.rs`, 타입은 `crates/bot-core/src/intake.rs`)가 접수 시 `bot_runs.reply_transport`를 기록하며 재전달, 위임, 원 대화 묶임 스케줄 실행이 이를 상속합니다.
  2. **클레임 격리 (`Sender::scope`):** `Outbox::claim`(`crates/bot-store/src/outbox.rs`)은 대화 유형이 아니라 `bot_outbox.transport`와 선택적 `(profile, binding)`으로 PostgreSQL에서 필터링하므로, `Direct`/`Channel`을 공유하는 두 푸시 어댑터가 서로의 응답을 가져가거나 거부하지 않습니다.
  3. **시퀀스 순서:** 같은 `run`의 앞선 `pending`/`sending` 행은 `transport`와 무관하게 뒤 시퀀스를 차단합니다.
  4. **명시적 대상(`ScheduleRoutes`) 제한:** `crates/bot-gateway/src/schedule_routes.rs`, 채널 소유자 승인 DM 라우팅(`crates/bot-store/src/approval_open.rs`), 대상 큐 등록(`crates/bot-store/src/delivery.rs`의 `push_to`)은 현재 `PushTransportId::SLACK`에 고정되어 있습니다. 다른 푸시 어댑터에도 명시적 대상을 열려면 `bot_user_schedules`와 `bot_deliveries`에 대상 transport를 저장하고 세 곳의 Slack 고정을 해제해야 합니다.
- **패리티 감사:** `python3 scripts/parity_audit.py slack <new-name> --skip crates/bot-gateway/src/slack/`을 실행해 `slack`은 언급하지만 새 이름은 없는 파일 목록을 확인하고 Slack 전용 기능을 제외한 뒤 점검하세요(`--strict`는 남은 항목이 있으면 실패). 스키마, SQL 제약, 핑거프린트가 정적이므로 채널도 동적 등록 대신 정적 타입을 사용합니다(동적 등록은 체크리스트 대부분을 그대로 둔 채 컴파일러 검사만 잃게 됨).

## SecretStore 백엔드

- **구현:** `crates/bot-cli/src/secrets/` 아래에 `SecretStore` 모듈을 추가하고 `crates/bot-cli/src/secrets/mod.rs`에서 내보내고 설명합니다.
- **컴파일러:** `crates/bot-cli/src/secrets/mod.rs`의 `impl SecretsKind`(보호 경로, 누락 시크릿 힌트, 복호화 플래그)와 `crates/bot-cli/src/load.rs`의 `secret_store`.
- **조용한 누락:** `crates/bot-cli/src/settings.rs`의 `parse_secrets`(`BOT_SECRETS`)와 [시크릿 저장소와 명령](gateway.md#시크릿-저장소와-명령).
- **자동 격리:** `protected_paths()`가 반환하는 파일은 자동으로 도구 샌드박스 `deny_read` 목록(`crates/bot-cli/src/serve/runner.rs`의 `denied`)에 들어갑니다.

## 샌드박스 백엔드

- **도구 서버:** `crates/bot-broker/` 아래에 `bot_broker::sandbox::Sandbox`(`crates/bot-broker/src/sandbox.rs`)를 구현하고(`srt.rs` 참고) `crates/bot-broker/src/lib.rs`에서 `pub mod`로 노출합니다.
  - **조용한 누락:** `local_sandbox`(`crates/bot-cli/src/serve/runner.rs`)가 `SrtSandbox`를 생성하고, `Sandbox`와 `parse_sandbox`(`crates/bot-cli/src/execution.rs`)는 `srt`와 `apple`만 가집니다. 기동 사전 점검 `refuse_required_state`는 호출별 `admit`에 맞춰 `srt::supports_state`를 호출하므로 선택한 백엔드로 분기해야 합니다. `crates/bot-cli/src/doctor/host.rs`, [아키텍처](architecture.md), [보안](security.md), CI의 `tool-sandbox` 작업(`.github/workflows/ci.yml`)도 함께 갱신하세요.
  - 아직 백엔드가 하나뿐이라 호스트 설정 선택 테이블이 없습니다. 두 번째 백엔드를 선정하면 `local_sandbox`를 `Arc<dyn Sandbox>`로 바꾸고 선택기를 추가하세요.
- **모델 VM:** `bot_core::sandbox::Sandbox<ModelSpec>`의 구현은 하나뿐입니다(`crates/bot-runtime/src/model/container.rs`의 `ContainerSandbox`). 연결 코드(`crates/bot-cli/src/serve/claude.rs`)가 해당 구체 타입과 앵커 검증 게이트웨이 IPv4 릴레이 계약([아키텍처](architecture.md))에 의존하므로, 다른 VM 백엔드를 추가하려면 먼저 이 계약을 포트 트레이트로 끌어올려야 합니다.

## 트리거 유형

- **컴파일러 (`TriggerSource` 변형):** `crates/bot-admin/src/map.rs`와 `crates/bot-gateway/src/routes.rs`(두 곳).
- **조용한 누락:**
  - `schemas/v1/triggers.schema.json`.
  - `crates/bot-profile/src/{triggers,model,resolve,convert}.rs`의 파싱·변환과 `crates/bot-core/src/profile.rs`의 정의.
  - **생산자:** 이벤트를 발행하고 접수하는 컴포넌트(`cron`은 `crates/bot-runtime/src/scheduler.rs`, 진입점 이벤트는 `crates/bot-gateway/src/ingress.rs`). 생산자 없이 변형만 추가하면 아무 일도 일어나지 않습니다.
  - 문서: [프로필 명세](profile-spec.md).

## 내장 도구

내장 도구는 승인과 PostgreSQL 트랜잭션에 직접 결합되므로 의도적으로 최소한만 유지합니다. ID, 설명, 스키마, `reserved`, `card_body`는 단일 테이블(`crates/bot-core/src/builtin.rs`)에 모여 있습니다.

- **중앙 테이블과 분류:** `crates/bot-core/src/builtin.rs`(`model_tools`, `reserved`, `card_body`, `delegate_schema`). 프로필 검증(`crates/bot-profile/src/tools.rs`)과 스캐폴딩(`crates/bot-cli/src/scaffold/tool.rs`)은 `builtin::reserved`를 직접 참조합니다.
- **모델 노출과 핸들러:** API 러너 `crates/bot-runtime/src/context.rs`와 `crates/bot-runtime/src/worker/calls.rs`, VM 릴레이 `crates/bot-broker/src/front/{mcp,delegate}.rs`, 브로커 서비스 `crates/bot-broker/src/service/{share,schedule,deliver}.rs`.
- **스토어와 승인 카드:** `crates/bot-store/src/{share,user_schedule,delivery,admin_approvals}.rs`와 `crates/bot-admin/src/json.rs`(둘 다 소유자 전용 카드 본문 보호에 `builtin::card_body` 사용).
- **연결 및 문서:** `crates/bot-cli/src/serve/decide.rs`, `docs/architecture.md`, `docs/plugins.md`, `docs/profile-spec.md`(`.github/scripts/docs_contract.py`가 `crates/bot-core/src/builtin.rs`의 모든 예약 ID가 문서화되었는지 검증).

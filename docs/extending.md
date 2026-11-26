# 확장할 때 손댈 곳은 종류마다 다르고, 컴파일러가 알려 주는 곳과 빠뜨려도 컴파일되는 곳을 나눠 적는다.

새 도구·모델 제공자·채널·보관소·샌드박스를 더하려는 사람과 AI가 코드 전체를 뒤지지 않고 "어디를 고치나"를 찾게 한다. 아래 목록은 새 변종을 실제로 더해 보고 `cargo check`가 가리키는 곳을 센 것(**컴파일러**)과 코드를 읽어 적은 것(**조용한 곳**: 문자열·스키마·SQL·문서처럼 빠뜨려도 컴파일되는 곳)이다. 조용한 곳을 먼저 확인한다. 파일 수는 곧 낡으므로 적지 않는다. 목록이 낡았는지는 같은 실험을 다시 해 보거나 `scripts/parity_audit.py`로 확인한다. 용어는 [용어집](glossary.md)을 본다.

| 확장 | Rust 변경 | 어디 |
|---|---|---|
| 일반 도구(MCP 서버) | 없음, 설정만 | [일반 도구](#일반-도구) |
| OpenAI 호환 모델 서버(Ollama·vLLM·Gemini 호환) | 없음, 설정만 | [모델 제공자](#모델-제공자) |
| 새 모델 API 규약 | 런타임의 규약 파일과 `bot-cli`의 제공자 파일 | [모델 제공자](#모델-제공자) |
| 채팅 채널 | 여러 크레이트 | [채팅 채널](#채팅-채널) |
| SecretStore 백엔드 | `bot-cli`만 | [SecretStore 백엔드](#secretstore-백엔드) |
| 샌드박스 백엔드 | 도구용은 `bot-broker`와 `bot-cli`의 조립 | [샌드박스 백엔드](#샌드박스-백엔드) |
| 트리거 종류 | 프로필·게이트웨이·관리 화면 | [트리거 종류](#트리거-종류) |
| 내장 도구 | 단일 표(`bot-core`)와 각 크레이트의 처리기 | [내장 도구](#내장-도구) |

## 일반 도구

Rust를 고치지 않는다. 도구는 MCP 서버로 만들고 설정 네 곳을 맞춘다. `bot validate`가 어긋난 곳을 이름을 밝혀 알린다.

- 서버 설치와 호스트 설정의 `servers` 등록: [도구 서버 설치](plugins.md#도구-서버-설치). 파싱은 `crates/bot-cli/src/install.rs`다.
- 스킬 폴더의 `tools.yaml`(스키마 `schemas/v1/tools.schema.json`)과 `bot mcp sync`가 적는 `tools.lock.yaml`.
- `profile.yaml`의 `roles`·`policy.scopes`(승인이 필요하면 `policy.approve`). [프로필 명세](profile-spec.md)
- 에이전트의 `env.yaml`과 `secrets`.

## 모델 제공자

- **OpenAI 호환 서버**는 Rust 0줄이다. 호스트 설정에 `providers.<별칭>`을 `kind: "openai-api"`로 적는다([호스트 설정과 역할 조립](architecture.md#호스트-설정과-역할-조립)).
- **새 API 규약**(예: Gemini 고유 규약)
  - 컴파일러: `crates/bot-runtime/src/model/api/wire.rs`(`ApiKind`을 가르는 곳이 모두 이 파일이다)와 `crates/bot-cli/src/execution/provider.rs`(설명에 쓰는 종류 이름·활성화 지문 표지 바이트·`parse_api`가 받는 키).
  - 새 파일: `crates/bot-runtime/src/model/api/wire/` 아래에 그 규약의 요청·응답 변환.
  - 조용한 곳: `parse_providers`의 `kind` 문자열 `match`(`provider.rs`, 빠지면 설정에서 고를 수 없을 뿐이다), 문서(위 architecture.md 절).
  - 함정: 지문 표지 바이트는 **새 값**을 쓰고 기존 값은 바꾸지 않는다. 바뀌면 이미 활성화된 모든 배포가 재활성화를 요구받는다. `crates/bot-cli/src/execution/fingerprint_tests.rs`에 고정 입력 한 줄을 더해 값을 고정한다.
- **VM 안에서 모델 CLI를 돌리는 새 제공자**(새 `Provider` 변종)
  - 컴파일러: `provider.rs`의 `impl Provider`(자격 이름·VM 필요 여부·설명·지문 재료)와 `crates/bot-cli/src/serve/models.rs`의 `build`(실행기 조립).
  - 조용한 곳: `crates/bot-cli/src/serve/claude.rs`의 프록시 표와 CLI 설정은 `Provider::ClaudeCli`만 읽는다. 새 CLI형이면 직접 더한다(빠뜨리면 그 별칭은 상류 항목이 없어 거절된다).
  - 한계: 브로커 프런트는 Anthropic `/v1/messages`만 중계한다(`crates/bot-broker/src/front.rs`). 다른 규약의 CLI는 프런트를 먼저 넓혀야 한다.

## 채팅 채널

새 입구(`Adapter` 변종)를 더하면 컴파일러가 아래를 가리킨다.

- `crates/bot-profile/src/model.rs`(입구의 정규 문자열 `entrance`와 종류 이름 `adapter_name`)와 `crates/bot-profile/src/activate.rs`(호스트 별칭 대조)
- `crates/bot-gateway/src/entrance.rs`(`Entrance`로 바꾸는 곳. 이어서 `Entrance`·`Real` 변종과 `Conflict::fix`의 `match`도 같이 채운다)
- `crates/bot-admin/src/map.rs`
- `crates/bot-cli/src/validate.rs`의 `entrance_secrets`와 `crates/bot-cli/src/serve/gateway.rs`(어느 입구 리스너를 여는지)

컴파일러가 모르는 **조용한 곳**:

- 프로필 스키마 `schemas/v1/profile.schema.json`의 `bindings` `oneOf`에 항목을 더한다. 해석기 `crates/bot-profile/src/profile.rs`의 `adapter` `match`를 빠뜨리면 `unknown-adapter`로 거절된다(조용히 웹훅이 되지 않는다).
- 호스트 설정 절: `crates/bot-gateway/src/host.rs`(구조체와 검증), `crates/bot-gateway/src/mapping.rs`(활성화 지문 — 새 절이 없는 설정의 지문 바이트는 그대로여야 한다), `crates/bot-profile/src/host.rs`의 `HostCatalog` 필드와 그것을 채우는 `crates/bot-cli/src/host.rs`의 `catalog()`.
- 새 입구 어댑터 모듈(수신 → `Inbound` → `Ingress`)과 회신 `Sender`(포트는 `crates/bot-gateway/src/delivery.rs`), 조립은 `crates/bot-cli/src/serve/gateway.rs`.
- 채널 고유 표가 필요하면 마이그레이션(이벤트 중복 제거는 `crates/bot-store/migrations/0022_slack_events.sql`을 본다).
- 문서: [게이트웨이 운영](gateway.md)의 호스트 설정 표, [프로필 명세](profile-spec.md)의 바인딩.

**새 대화 종류(`ConversationKind`)** 가 필요하면 컴파일러는 `crates/bot-store/src/codec.rs`, `crates/bot-runtime/src/worker/span.rs`, `crates/bot-gateway/src/inbound.rs`, `crates/bot-broker/src/runner.rs`, `crates/bot-admin/src/json.rs`를 가리킨다. 조용한 곳은 DB의 `conv_kind` 열 CHECK(`crates/bot-store/migrations/0001_runs.sql`)다. 새 마이그레이션으로 목록을 넓혀야 한다.

**새 신원 종류(`IdentityKind`)** 는 컴파일러가 아무 곳도 가리키지 않는다(모두 `==`·`matches!`로 쓰고 `RunOrigin::from_ingress`는 `_ =>`로 끝난다). 직접 찾아야 한다: `crates/bot-core/src/identity.rs`, `crates/bot-core/src/state.rs`, `crates/bot-profile/src/approval.rs`, `crates/bot-store/src/api.rs`(`grep -rn IdentityKind crates/*/src`).

**함정: 회신 outbox의 교차 점유.** `Outbox::claim`은 대화 종류(`conv_kind = any(종류들)`)로 회신을 점유하고(`crates/bot-store/src/outbox.rs`) Slack 발송자는 `Channel`·`Direct`를 점유한다(`crates/bot-gateway/src/slack/send.rs`). 두 번째 push 어댑터가 같은 대화 종류를 쓰면 한 `Deliverer`가 상대 어댑터의 회신을 점유하고, `Sender::allows`가 거짓이면 그 회신을 **거절로 정산해 잃는다**(`crates/bot-gateway/src/delivery.rs`). 새 `ConversationKind`를 쓰면 위 CHECK가 막는다. 어느 쪽이든 점유 키를 대화 종류에서 어댑터 식별자로 올려야 한다. 그 변경이 들어오면 이 절을 갱신한다.

**빠진 곳 찾기.** `python3 scripts/parity_audit.py slack <새 이름> --skip crates/bot-gateway/src/slack/`은 Slack이 쓴 파일 중 새 이름이 아직 없는 것을 모아 보여 준다. 그 입구에만 있는 기능이면 지워도 되는 점검표다(`--strict`는 빠진 파일이 있으면 실패한다).

채널을 런타임에 등록하는 방식은 쓰지 않는다. 스키마·DB·지문이 정적이라 이 목록의 대부분이 남고, 컴파일러의 확인만 잃는다.

## SecretStore 백엔드

- 구현: `crates/bot-cli/src/secrets/` 아래 새 파일(`SecretStore` 구현. 모듈 선언과 설명은 `secrets/mod.rs`).
- 컴파일러: `crates/bot-cli/src/secrets/mod.rs`의 `impl SecretsKind`(도구가 읽지 못해야 하는 파일·누락 힌트·복호화 여부)와 `crates/bot-cli/src/load.rs`의 `secret_store`.
- 조용한 곳: `BOT_SECRETS` 문자열을 읽는 `crates/bot-cli/src/settings.rs`의 `parse_secrets`(빠지면 고를 수 없을 뿐이다), 문서 [자격 보관소와 명령](gateway.md#자격-보관소와-명령).
- 보호는 따라온다: `protected_paths()`가 알려 준 파일은 도구의 읽기 금지 목록(`crates/bot-cli/src/serve/runner.rs`의 `denied`)에 자동으로 들어간다.

## 샌드박스 백엔드

- **도구 서버용**: `bot_broker::sandbox::Sandbox`(`crates/bot-broker/src/sandbox.rs`)를 `crates/bot-broker/` 아래 새 파일에 구현하고(srt는 `srt.rs`) `crates/bot-broker/src/lib.rs`에서 `pub mod`로 연다.
  - 조용한 곳(컴파일러가 모른다): 조립 `crates/bot-cli/src/serve/runner.rs`의 `local_sandbox`가 구체 `SrtSandbox`를 만든다. 호스트 설정의 `Sandbox` 구조체와 `parse_sandbox`(`crates/bot-cli/src/execution.rs`)는 `srt`·`apple` 두 필드다. 시작 전 점검 `refuse_required_state`는 srt의 `supports_state`(호출마다 거절하는 `admit`과 같은 판정)를 부르므로 srt 전용이다. 둘째 백엔드는 자기 판정을 내놓고 이 점검이 고른 백엔드의 것을 부르게 한다. 그 밖에 `crates/bot-cli/src/doctor/host.rs`, 문서([아키텍처](architecture.md)의 설정 표·[보안](security.md)), CI의 `tool-sandbox` 잡(`.github/workflows/ci.yml`).
  - 백엔드를 호스트 설정에서 고르는 표는 아직 없다. 구현이 하나뿐이라 한 줄짜리 표가 되기 때문이다. 두 번째 백엔드가 정해지면 `local_sandbox`가 `Arc<dyn Sandbox>`를 돌려주게 하고 그때 표를 넣는다.
- **모델 VM용**: 포트 `bot_core::sandbox::Sandbox<ModelSpec>`의 구현은 Apple `container`용 `ContainerSandbox`(`crates/bot-runtime/src/model/container.rs`) 하나다. 조립(`crates/bot-cli/src/serve/claude.rs`)이 그 구체 타입과 릴레이 주소 계약(닻 VM이 확인한 게이트웨이 IPv4, [아키텍처](architecture.md))에 기대고 있어, 다른 VM 백엔드는 그 계약을 포트로 올리는 설계가 먼저다.

## 트리거 종류

새 `TriggerSource` 변종을 더하면 컴파일러는 `crates/bot-admin/src/map.rs`와 `crates/bot-gateway/src/routes.rs`(두 곳)를 가리킨다. 조용한 곳은 다음이다.

- 스키마 `schemas/v1/triggers.schema.json`
- 문자열에서 변종을 만드는 해석: `crates/bot-profile/src/{triggers,model,resolve,convert}.rs`와 정의 `crates/bot-core/src/profile.rs`
- **생산자**: 그 사건을 만들어 접수하는 컴포넌트. cron은 `crates/bot-runtime/src/scheduler.rs`, 입구 이벤트는 게이트웨이의 `crates/bot-gateway/src/ingress.rs`다. 변종만 더하고 생산자를 빠뜨리면 아무 일도 일어나지 않는다.
- 문서 [프로필 명세](profile-spec.md)

## 내장 도구

일반 도구와 달리 승인·DB와 얽혀 있어서 의도적으로 쉽게 만들지 않는다. 내장 도구 ID·설명·스키마·예약 판별(`reserved`)·승인 카드 본문 분류(`card_body`)는 `crates/bot-core/src/builtin.rs` 한 곳에 모여 있다. 새 내장 도구를 더할 때 손댈 곳은 크레이트별로 다음과 같다.

- 단일 표·예약·카드 본문 분류: `crates/bot-core/src/builtin.rs` (`model_tools`·`reserved`·`card_body`·`delegate_schema`). 프로필 검증(`crates/bot-profile/src/tools.rs`)과 스캐폴드(`crates/bot-cli/src/scaffold/tool.rs`)는 `builtin::reserved`를 그대로 쓴다.
- 모델 노출과 분기: API 실행기 `crates/bot-runtime/src/context.rs`·`crates/bot-runtime/src/worker/calls.rs`, VM 릴레이 `crates/bot-broker/src/front/{mcp,delegate}.rs`, 브로커 처리기 `crates/bot-broker/src/service/{share,schedule,deliver}.rs`.
- 저장과 승인 카드: `crates/bot-store/src/{share,user_schedule,delivery,admin_approvals}.rs`, `crates/bot-admin/src/json.rs`(둘 다 `builtin::card_body`로 소유자 전용 본문 대상을 가른다).
- 조립과 문서: `crates/bot-cli/src/serve/decide.rs`, `docs/architecture.md`·`docs/plugins.md`·`docs/profile-spec.md`(`.github/scripts/docs_contract.py`가 `crates/bot-core/src/builtin.rs`의 예약 ID가 문서에 적혔는지 확인한다).

# 용어집

[English](../glossary.md) | 한국어

문서 용어를 코드 식별자에 매핑하고 크레이트 간에 이름이 중복되는 항목을 짚어 줍니다. **주의** 표시가 붙은 항목은 오독하기 쉬운 부분입니다. [AGENTS.md](../../AGENTS.md)의 "Where Things Live" 표와 함께 참고하세요.

## 실행

| 용어 | 코드 | 의미 |
|---|---|---|
| run (실행) | `RunId`, 테이블 `bot_runs`, `RunState` | 수락된 요청 하나에서 생성되는 작업 단위. 루트 실행은 `root = id`이며, 위임된 자식은 `parent`와 `parent_step`을 가집니다. 상태: `Ready`, `Leased`, `Waiting`, `Retry`, `Succeeded`, `Dead`, `Cancelled`. |
| claim (선점) | `RunStore::claim`, `RunContext` | 워커가 준비된 실행을 획득하는 동작(및 그 컨텍스트). 모든 claim은 `claim_epoch`를 증가시킵니다. |
| lease (리스) | `lease_until_ms`, `RunStore::renew` | 실행을 선점한 워커가 보유하는 소유 기한. 하트비트(`renew`)로 연장되며, 만료되면 다른 워커가 인계받습니다. |
| generation (세대) | `Generation`, `bot_runs.generation` | 이전 세대가 열어 둔 승인을 무효화하는 펜싱 토큰. `keep_generation`이 설정되지 않으면 claim 시 증가합니다. 승인·자식·보류(hold) 결정으로 깨어날 때와 우아한 종료 인계 후 첫 번째 claim(이미 증가됨)에서만 유지되며, 최초 claim(`0 -> 1`), 일시적 오류 재시도, 리스 만료 인계, `bot/pass` 이후 다음 후보 인계 시 증가합니다. |
| `claim_epoch` | `ClaimEpoch`, `bot_runs.claim_epoch` | 같은 세대 안에서도 구버전 쓰기 주체를 차단하기 위해 정상 재개를 포함한 **모든** claim마다 단조 증가하는 실행별 카운터. |
| attempt (시도) | `Attempt` | 소유권 검사에 쓰는 단일 claim 식별자 `(profile, run, generation, claim_epoch)`. `claim`이 `None`이면 식별 전용 핸들(승인·UI·트레이스용)이며 워커 쓰기 권한을 부여하지 않습니다. **주의:** `bot_runs.attempts`는 이 구조체가 아니라 소비된 실패·리스 만료 재시도 횟수를 셉니다. |

## 인그레스

| 용어 | 코드 | 의미 |
|---|---|---|
| request (요청) | `RequestId`, `RequestKey` | 호출자가 제공하는 멱등성 키. 중복 전달은 기존 실행으로 수렴합니다. |
| entrance / adapter (입구 / 어댑터) | `Adapter`, `Entrance` | CLI, Slack 채널, Slack DM, HTTP 웹훅, OpenAI 호환 HTTP. `Adapter`(`bot_core::profile`)는 선언된 바인딩 종류이고, `Entrance`(`bot_gateway::entrance`)는 게이트웨이의 별칭 단위 진입점입니다. |
| binding (바인딩) | `BindingId` | 하나의 entrance와 하나의 에이전트를 연결하는 프로필 선언. |
| normalized input (정규화 입력) | `Inbound` | 어댑터 인증 이후 생성되는 전송 무관 페이로드(`bot_gateway::inbound`). |
| ingress (인그레스) | `bot_gateway::ingress` | `verify -> normalize -> durable accept -> ack` 순서로 실행되는 게이트웨이 파이프라인. |
| accept (수락) | `Accept`, `Envelope`, `RunStore::accept` | 검증된 엔벨로프를 PostgreSQL에 커밋해 실행을 생성하는 단계. 수락은 곧 DB 커밋입니다. |
| intake (접수) | `bot_core::intake` | **주의:** 수락 단계 자체가 아니라 접수 상태 조회, 프라이빗 입력 전달, 응답 원장 포트를 정의하는 모듈입니다. |

## 이그레스 (응답)

| 용어 | 코드 | 의미 |
|---|---|---|
| answer (답변) | `Finish.answers` | 실행 완료와 같은 트랜잭션 안에서 커밋되는 응답 페이로드. |
| outbox (아웃박스) | `Outbox`, `Outgoing` | 실행 완료와 원자적으로 기록되는 내구성 응답 발송 원장. |
| reply (응답) | `ReplyBody` | 원래 대화로 전달되는 응답 본문(`bot_gateway::reply`). |
| delivery (전달) | `Deliverer`, `Sender` | 아웃박스 행을 선점하고 활성 리비전을 확인한 뒤 푸시하고 정산하는 과정(`bot_gateway::delivery`). `Sender`는 푸시 어댑터(현재 Slack)이며, CLI와 HTTP는 아웃박스 대신 상태 조회로 응답을 읽습니다. |
| `Reply<'a, T>` | `bot_core::port::Reply` | **주의:** 채팅 응답이 아니라 포트 트레이트가 사용하는 박싱된 비동기 반환 타입 별칭(`Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>`)입니다. |

## 중복 사용되는 이름

| 이름 | 서로 다른 의미 |
|---|---|
| `cli` | `bot_core::cli`는 **Claude CLI 도구 호출 위치**(체크포인트 키)와 실행 전 거부 기록(`CliRefusal`)을 정의하고, `bot_gateway::cli`는 Unix 소켓 CLI 입구이며, `bot-cli`는 `bot` 바이너리 크레이트입니다. |
| `Sandbox` | `bot_core::sandbox::Sandbox<ModelSpec>`은 모델 VM(`ContainerSandbox`)을 격리하고, `bot_broker::sandbox::Sandbox`는 MCP 도구 서버(`SrtSandbox`)를 격리합니다. 둘을 잇는 공통 트레이트는 없습니다. |
| `Delivery` | `bot_gateway::delivery`에서는 아웃박스 발송 결과, `bot_runtime::worker`에서는 예약 응답 포트 묶음, `bot-cli`의 `serve::admin`에서는 운영자 시크릿 출력 대상(터미널 vs 파일)을 뜻합니다. |

## 마이그레이션 주석의 F1–F4

v2.2에서 출시된 네 가지 기능의 내부 식별자입니다. Rust 코드와 테스트에서는 제거되었지만, 배포된 불변 SQL 마이그레이션(`crates/bot-store/migrations/0020_pass_and_threads.sql` ~ `crates/bot-store/migrations/0022_slack_events.sql`)의 주석에만 남아 있습니다.

| 태그 | 의미 | 주요 위치 |
|---|---|---|
| F1 | 정적 트리거에 대해 활성화 시점에 발급되는 사전 승인(pre-approval) 권한 | `crates/bot-core/src/grant.rs`, `crates/bot-store/src/grants.rs` |
| F2 | Slack 채널 이벤트 선택(`events`: 멘션, 스레드 답글, 리액션) 및 봇 스레드 참여 증명 | `crates/bot-core/src/slack.rs`, `crates/bot-gateway/src/slack/events.rs` |
| F3 | 처리되지 않은 직접 도구 이벤트를 다음 후보로 넘기는 패스(`bot/pass`) 체인 및 커서 | `crates/bot-core/src/pass.rs`, `crates/bot-store/src/pass.rs` |
| F4 | 지정 승인자의 메시지나 리액션으로 승인 단계를 충족할 수 있게 하는 증거(evidence) | `crates/bot-store/src/evidence.rs` |

## 기타 혼동하기 쉬운 용어

| 용어 | 코드 | 의미 |
|---|---|---|
| effect (이펙트) | `EffectId`, `EffectState` | 도구가 수행하는 단일 외부 쓰기 단계. 결과가 불확실한 이펙트는 운영자가 보류를 해소(`HoldId`)할 때까지 `Held` 상태로 일시 중지됩니다. |
| active set (활성 세트) | `ActiveRevisions`, `Revision` | 현재 게시된 프로필 세트와 그 리비전. `ActiveRevisions`는 프로필 리비전과 보안 관련 호스트 설정을 함께 해시한 `Revision`을 읽고 원자적으로 교체(CAS)합니다. 변경 시 `bot serve --activate`가 필요하며, 이전 리비전을 실행 중인 프로세스는 구버전(stale) 상태가 됩니다([게이트웨이](gateway.md)). |

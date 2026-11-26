# 문서의 말과 코드의 이름은 이 표로 이어진다.

같은 말이 크레이트마다 다른 뜻으로 쓰이거나 문서의 말과 코드의 이름이 달라 읽는 사람이 오해하는 곳을 모았다. 오해하기 쉬운 곳은 **오독 주의**로 표시했다. 처음 코드를 읽는다면 [AGENTS.md](../AGENTS.md)의 "X는 어디에 있나" 표와 함께 본다.

## 실행 계열

| 문서의 말 | 코드 | 뜻 |
|---|---|---|
| 실행(run) | `RunId`, 표 `bot_runs`, `RunState` | 접수 하나가 만드는 일의 단위. 루트 run은 `root = id`이고 위임한 자식 run은 `parent`·`parent_step`을 가진다. 상태는 `Ready`·`Leased`·`Waiting`·`Retry`·`Succeeded`·`Dead`·`Cancelled`다. |
| 점유(claim) | `RunStore::claim`, `RunContext` | 워커가 준비된 run 하나를 가져가는 일과 그 결과. 점유할 때마다 `claim_epoch`이 오른다. |
| 리스(lease) | `lease_until_ms`, `RunStore::renew` | 점유한 워커의 소유 기한. 하트비트가 `renew`로 늘리고, 만료되면 다른 워커가 인계한다. 문서의 "리스(claim)"는 점유와 그 기한을 함께 가리킨다. |
| 세대(generation) | `Generation`, `bot_runs.generation` | 옛 세대가 연 승인을 무효로 만드는 펜싱 값. 점유할 때마다 오르고, `keep_generation`이 켜진 점유만 그대로 둔다. 그대로 두는 점유: 승인·자식 결과·보류 판정을 기다리다 깨어난 점유, 종료 반납 직후의 첫 점유(반납 때 이미 한 번 올렸다). 그 밖에는 오른다: 첫 점유(0→1)·일시 실패 재시도·리스 만료 인계·`bot/pass`로 다음 후보에 넘긴 뒤의 점유. |
| claim_epoch | `ClaimEpoch`, `bot_runs.claim_epoch` | **모든** 점유(정상 재개 포함)마다 오르는 run별 단조 증가 값. 같은 세대 안에서도 낡은 점유자의 쓰기를 막는다. |
| 시도(attempt) | `Attempt` | 한 점유의 신원(프로필·run·세대·claim_epoch). 소유자 검사의 열쇠다. `claim`이 `None`이면 신원만 담은 값(승인·UI·추적)이라 워커 작업을 인가하지 못한다. **오독 주의**: `bot_runs.attempts` 열은 이 값이 아니라 실패·리스 만료로 소모한 재시도 횟수다. |

## 접수 계열 (들어오는 쪽)

| 문서의 말 | 코드 | 뜻 |
|---|---|---|
| 요청(request) | `RequestId`, `RequestKey` | 호출자가 정하는 멱등 키. 같은 요청이 다시 오면 새 run을 만들지 않고 기존 run으로 돌아간다. |
| 입구 | `Adapter`, `Entrance` | CLI·Slack 채널·Slack DM·HTTP 웹훅·OpenAI 호환 HTTP. `Adapter`는 프로필 바인딩이 선언한 종류(`bot_core::profile`), `Entrance`는 게이트웨이가 별칭 수준에서 가리키는 입구(`bot_gateway::entrance`)다. |
| 바인딩(binding) | `BindingId` | 프로필이 입구 하나를 에이전트 하나에 잇는 선언. |
| 정규화 | `Inbound` | 어댑터가 인증한 입력을 입구와 무관한 한 모양으로 만든 값(`bot_gateway::inbound`). |
| 접수 진입 | `bot_gateway::ingress` | verify → normalize → durable accept → ack 순서로 요청을 받는 게이트웨이 로직. |
| 접수(accept) | `Accept`, `Envelope`, `RunStore::accept` | 인증된 봉투를 DB에 커밋해 run을 만드는 일. 접수 성공은 PostgreSQL 커밋 성공이다. |
| 접수 조회 | `bot_core::intake` | **오독 주의**: 접수 자체가 아니라 접수 상태 조회·민감 입력 전달·회신 원장 포트를 모은 모듈 이름이다. |

## 회신 계열 (나가는 쪽)

| 문서의 말 | 코드 | 뜻 |
|---|---|---|
| 답(answer) | `Finish.answers` | 결과와 같은 커밋에 남기는 회신 본문 목록. |
| outbox | `Outbox`, `Outgoing` | 회신 발송 예약 원장. 결과를 커밋할 때 같은 트랜잭션에 행이 생긴다. |
| 회신(reply) | `ReplyBody` | 원래 대화로 돌려주는 답의 본문(`bot_gateway::reply`). |
| 발송(delivery) | `Deliverer`, `Sender` | outbox 행을 점유하고, 현재 판본을 확인하고, 보내고, 정산하는 일(`bot_gateway::delivery`). `Sender`는 push 어댑터(지금은 Slack) 하나다. CLI·HTTP 회신은 outbox가 아니라 상태 조회로 간다. |
| `Reply<'a, T>` | `bot_core::port::Reply` | **오독 주의**: 회신이 아니다. 포트 메서드의 비동기 반환형 별칭(`Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>`)이다. |

## 이름이 겹치는 것

| 이름 | 서로 다른 뜻 |
|---|---|
| `cli` | `bot_core::cli`는 CLI 입구가 아니라 **Claude CLI가 낸 도구 호출의 위치**(체크포인트 키)와 실행 전 확정 거절(`CliRefusal`) 계약이다. `bot_gateway::cli`는 CLI 입구(유닉스 소켓), `bot-cli`는 `bot` 바이너리 크레이트다. |
| `Sandbox` | `bot_core::sandbox::Sandbox<ModelSpec>`은 모델 VM(`ContainerSandbox`), `bot_broker::sandbox::Sandbox`는 도구 서버(srt, `SrtSandbox`)다. 둘을 묶는 공통 포트는 없다. |
| `Delivery` | 게이트웨이(`bot_gateway::delivery`)에서는 발송 결과, 워커(`bot_runtime::worker`)에서는 예약 회신을 다루는 포트 묶음, `bot-cli`의 관리 화면 기동(`serve::admin`)에서는 운영자 비밀을 내보낼 곳(터미널·파일)이다. |

## 마이그레이션 주석의 F1~F4

v2.2에 함께 들어온 네 기능의 내부 번호다. Rust 소스와 시험 주석에서는 번호를 모두 제거했고, 바이트를 바꿀 수 없는 이미 적용된 마이그레이션 SQL(`crates/bot-store/migrations/0020_pass_and_threads.sql`~`0022_slack_events.sql`)의 주석에만 남아 있다.

| 표지 | 뜻 | 대표 위치 |
|---|---|---|
| F1 | 사전 승인: 활성화가 발급하는 정적 트리거의 쓰기 허가 | `crates/bot-core/src/grant.rs`, `crates/bot-store/src/grants.rs` |
| F2 | Slack 이벤트 선택: 채널 바인딩의 `events`(멘션·스레드 답글·반응)와 봇이 참여한 스레드의 증거 | `crates/bot-core/src/slack.rs`, `crates/bot-gateway/src/slack/events.rs` |
| F3 | 넘김(`bot/pass`): 직접 도구가 처리하지 않은 이벤트를 다음 후보로 넘기는 사슬과 커서 | `crates/bot-core/src/pass.rs`, `crates/bot-store/src/pass.rs` |
| F4 | 지정한 사람의 승인: 지정 승인자의 메시지·반응이 승인 한 단계를 대신하는 증거 | `crates/bot-store/src/evidence.rs` |

## 그 밖에 자주 헷갈리는 말

| 문서의 말 | 코드 | 뜻 |
|---|---|---|
| 효과(effect) | `EffectId`, `EffectState` | 도구가 외부에 쓰는 일의 한 단계. 결과를 알 수 없는 효과는 `Held`로 멈추고 운영자가 판정한다(보류, `HoldId`). |
| 활성 집합 | `ActiveRevisions`(포트), `Revision` | 지금 게시된 프로필 집합과 그 판본. `ActiveRevisions`는 게시된 전체 집합의 판본(`Revision`)을 읽고 한 번에 교체(CAS)하는 포트(트레이트)이고, 판본 값은 프로필 판본과 권한에 영향을 주는 호스트 설정을 함께 묶는다. 바꾸면 `bot serve --activate`로 새 집합을 올려야 하고, 옛 설정으로 남은 프로세스는 구판이다([게이트웨이 운영](gateway.md)). |

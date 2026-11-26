# AGENTS.md

프로필(에이전트·도구·승인 규칙 선언)을 읽어 봇을 실행하는 Rust 워크스페이스다. 왜 이렇게 만들었나 `docs/why.md`, 흐름 `docs/architecture.md`, 용어 `docs/glossary.md`, 확장 `docs/extending.md`, 운영 `docs/gateway.md`.

## 지킬 것 (약화하지 말 것)

1. 모델은 샌드박스에 가둔다. 비밀은 없고(세션 토큰뿐) 나가는 길은 브로커 릴레이 하나다.
2. 위험한 일의 허가·승인·감사는 브로커 한 곳이 한다(`crates/bot-broker/src/service.rs`). 모델이 도구를 직접 돌리는 길을 만들지 않는다.
3. 요청은 DB에 먼저 적는다. 접수 성공은 PostgreSQL 커밋 성공이다.
4. 되돌릴 수 없는 일은 사람이 승인한다. 모델은 승인할 수 없다.

## 규약

- 파일은 600줄 이하다(`.github/scripts/check_contracts.py`가 검사). 분할은 개념 경계로 하고 줄 수만 맞추려고 쪼개지 않는다. 모든 `src` 파일은 `//!`에 책임을 적는다.
- 주석·문서·사용자 문구는 한국어, 식별자는 영어, 커밋 메시지는 제목·본문 모두 영어(conventional commit)다. 오류 사유·감사 결과는 고정 어휘 코드이고 자유 문장·본문·비밀을 싣지 않는다.
- 새 설정 키·스키마 속성·CLI 플래그·내장 도구는 **같은 변경에서** `docs/`를 갱신한다(이름이 빠졌는지 `.github/scripts/docs_contract.py`가 검사한다).
- 새로 쓰는 계약 문단은 수치가 셋 이상이거나 규칙이 둘 이상이면 표나 목록으로 쓴다.
- 의존성은 강한 근거가 있을 때만 더한다(`=` 고정, `deny.toml`). 공개 저장소이므로 비공개 이름·비밀을 넣지 않는다.
- 크레이트 사이에 새 의존을 만들지 않는다. 새 `pub mod`는 다른 크레이트가 쓸 때만 연다(시작은 `pub(crate)`).
- 시험 대역은 다른 크레이트가 쓰면 `testing` 기능 뒤에, 아니면 `cfg(test)`에 둔다. `serve::Overrides`와 게이트웨이의 `Deps.slack_base`는 시험 전용 이음매다.
- 활성화 지문은 바이트가 같아야 한다. 바뀌면 이미 활성화된 모든 배포가 재활성화를 요구받는다. 지문을 건드리는 변경은 고정 입력의 골든 시험을 먼저 쓴다.

## 명령

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked            # PG 시험은 BOT_TEST_DATABASE_URL이 필요하다(CI는 BOT_TEST_REQUIRE_DB=1로 건너뛰기를 실패로 본다)
    python3 .github/scripts/check_contracts.py
    scripts/release-check.sh [--strict]        # 위를 한 번에(docs/release.md)
    cargo test -p bot-broker                   # 한 크레이트만(도구 샌드박스 시험은 CI의 tool-sandbox 잡)

## 크레이트와 의존 방향

`bot-core` ◄ {`bot-profile`, `bot-store`, `bot-runtime`, `bot-broker`, `bot-gateway`, `bot-admin`} ◄ `bot-cli`. 가운데 여섯은 서로 의존하지 않고 `bot-core`의 포트(트레이트)로만 만난다.

| 크레이트 | 책임 | 읽기 시작점 |
|---|---|---|
| `bot-core` | 역할 사이의 데이터·포트·비트가 같아야 하는 알고리즘(cron·정규 JSON·해시) | `crates/bot-core/src/{lib,port,command,state}.rs`(`lib.rs`에 모듈 분류) |
| `bot-profile` | 프로필 폴더 → 검증된 스냅샷·판본 | `crates/bot-profile/src/{lib,registry}.rs`(`lib.rs` 머리말에 처리 순서) |
| `bot-store` | PG 큐·원장·outbox·감사 체인. 공개 표면은 `Store` 하나, 포트로만 접근 | `crates/bot-store/src/{lib,ports}.rs`(`lib.rs` 머리말에 규칙), `crates/bot-store/migrations/` |
| `bot-runtime` | 워커·스케줄러·위임·모델 실행기 | `crates/bot-runtime/src/{worker/mod,model/mod,context}.rs` |
| `bot-broker` | 도구 호출 관문·모델 프록시·MCP 실행(srt) | `crates/bot-broker/src/{lib,service,runner,srt}.rs`(`lib.rs`에 외부 계약) |
| `bot-gateway` | CLI·HTTP·Slack 접수와 회신 | `crates/bot-gateway/src/{ingress,delivery,slack/mod,http/mod}.rs` |
| `bot-admin` | 관리 API·UI(`ui/`)와 OTLP 추적 내보내기(`trace/`, 모든 역할이 쓴다) | `crates/bot-admin/src/{lib,api,trace/mod}.rs` |
| `bot-cli` | `bot` 바이너리·호스트 설정·SecretStore 구현·역할 조립 | `crates/bot-cli/src/{main,host,serve/mod}.rs` |

## X는 어디에 있나

| 무엇 | 어디 |
|---|---|
| 접수(멱등·커밋) | `crates/bot-gateway/src/ingress.rs` → `crates/bot-store/src/accept.rs` |
| 큐·리스·세대·재시도 | `crates/bot-store/src/{queue,finish,handover,shutdown}.rs`, 포트 `crates/bot-core/src/port.rs`의 `RunStore` |
| 워커 루프·위임 | `crates/bot-runtime/src/worker/`, `crates/bot-store/src/delegate.rs`, `crates/bot-broker/src/front/delegate.rs` |
| 도구 호출 관문 | `crates/bot-broker/src/service/call.rs`(호출 순서), `crates/bot-broker/src/{classify,scope,rate,recovery}.rs` |
| 승인(카드·결정) | `crates/bot-broker/src/{approval,service/decide}.rs`, `crates/bot-store/src/approvals.rs`, `crates/bot-admin/ui/decide.js` |
| 효과 원장·보류 | `crates/bot-store/src/{effects,effect_lookup}.rs`, `crates/bot-broker/src/effects.rs` |
| MCP 실행·샌드박스 | `crates/bot-broker/src/{runner,srt,sandbox,egress,proxy}.rs`, `crates/bot-broker/src/mcp/` |
| 모델 실행 | `crates/bot-runtime/src/model/`(`api/` 직접 API · `claude_code.rs`+`container.rs` VM CLI · `demo.rs`), 프록시 `crates/bot-broker/src/model.rs`, 릴레이 `crates/bot-broker/src/front.rs` |
| 프로필 로딩·검증 | `crates/bot-profile/src/lib.rs`, 스키마 `schemas/v1/`, 예시 `examples/profiles/` |
| 호스트 설정(host.json) | `crates/bot-cli/src/host.rs`(최상위), `crates/bot-cli/src/execution.rs`(providers·sandbox), `crates/bot-cli/src/install.rs`(servers), `crates/bot-gateway/src/host.rs`(gateway), `crates/bot-admin/src/settings.rs`(admin) |
| 활성화·복원 | `crates/bot-cli/src/serve/{activation,restore}.rs`, `crates/bot-store/src/{active,profile_bundles}.rs` |
| 회신(outbox) | `crates/bot-store/src/outbox.rs`, `crates/bot-gateway/src/{delivery,slack/send}.rs` |
| Slack 진행 표시·중단 | `crates/bot-gateway/src/slack/{ux,ux/cards,ux/stop}.rs` |
| 예약 | 정적 cron `crates/bot-runtime/src/scheduler.rs`+`crates/bot-core/src/cron.rs`, 대화 예약 `crates/bot-store/src/user_schedule.rs`+`crates/bot-broker/src/service/schedule.rs` |
| 기억 | `crates/bot-store/src/{memory,share}.rs`, `crates/bot-runtime/src/context/memory.rs` |
| 민감 DM | `crates/bot-gateway/src/ingress.rs`(분기), `crates/bot-broker/src/private.rs`, `crates/bot-store/src/private.rs` |
| 사전 승인 | `crates/bot-core/src/grant.rs`, `crates/bot-store/src/grants.rs`, `crates/bot-broker/src/preapproval.rs`, `crates/bot-admin/src/preapproval.rs` |
| 비밀 | 포트 `crates/bot-core/src/port.rs`의 `SecretStore`, 구현 `crates/bot-cli/src/secrets/`, 가림 `crates/bot-cli/src/serve/watched.rs` |
| 관측·감사 | `crates/bot-core/src/trace.rs`, `crates/bot-admin/src/trace/`, `crates/bot-store/src/audit.rs` |
| 확장(도구·모델 제공자·채널·보관소·샌드박스·트리거·내장 도구) | `docs/extending.md`, 빠진 곳 점검 `scripts/parity_audit.py` |

## 불변식 (코드 옆에 적혀 있다)

- store 규칙과 잠금 순서: `crates/bot-store/src/lib.rs` 머리말
- 브로커 호출 순서: `crates/bot-broker/src/service.rs` 머리말
- 접수 순서 verify → normalize → durable accept → ack: `crates/bot-gateway/src/lib.rs` 머리말
- 활성화 지문: `crates/bot-cli/src/host.rs`의 `authority_fingerprint`(전체) · `crates/bot-cli/src/execution.rs`(providers·sandbox) · `crates/bot-gateway/src/mapping.rs`(gateway 절)

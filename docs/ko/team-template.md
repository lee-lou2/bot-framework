# 팀 프로필 템플릿

[English](../team-template.md) | 한국어

[`examples/profiles/team`](../../examples/profiles/team)은 팀 리더(`main`) 한 명이 네 명의 전문가(`developer`, `designer`, `pm`, `ops`)에게 위임하는 복사 가능한 멀티 에이전트 프로필입니다. 규칙, 지식, 스킬, 메모리 시드, 비비밀 환경 변수, cron 트리거, 스코프별 승인 등 프로필의 모든 구성 요소를 보여주며 내장 데모 호스트에서 바로 실행됩니다. 인물, 채널, 회사 정보는 모두 가상의 예시입니다.

프로필 루트에는 허용된 항목(`profile.yaml`, `rules.md`, `agents`, `knowledge`, `skills`)만 둘 수 있으므로 이 가이드는 프로필 디렉터리 밖에 위치합니다.

## 먼저 실행해 보기

저장소 루트에서 실행합니다(README 데모와 동일한 PostgreSQL 설정 사용; 내장 데모 호스트는 `personal`과 `team`을 모두 인식합니다).

```sh
export BOT_PROFILES="$PWD/examples/profiles"
bot validate                                                     # personal과 team 검증
bot map --profile team                                           # 위임 트리, 권한 상한, 스킬, 도구 표시
bot chat --profile team "Show today's agenda"                    # 데모 모델이 agenda.list 호출
bot chat --profile team --request-id team-1 "Delete the walk event"  # Admin UI에 승인 카드 생성
```

`Admin UI: http://...`에서 대기 중인 카드를 승인합니다. 데모 모델은 `delegate`를 호출하지 않으므로 하위 에이전트 위임을 실행해 보려면 `host.json`에 실제 모델 별칭을 연결해야 합니다. 디렉터리 구조, 권한 상한, 승인 게이트 검증은 데모 모드에서도 동일하게 동작합니다.

## 디렉터리 구조

```text
examples/profiles/team/
  profile.yaml                      역할, 멤버, 바인딩, 권한 상한
  rules.md                          모든 에이전트 앞에 붙는 공통 규칙
  knowledge/                        팀 전체 참고 문서 (.md, .txt)
    team-intro.md  glossary.txt
  skills/                           모든 에이전트가 볼 수 있는 공통 스킬
    report-format/SKILL.md
  agents/main/                      팀 리더 (모든 입구가 여기로 바인딩)
    AGENT.md  env.yaml  triggers.yaml
    memory/team-profile.md          초기 공유 메모리 시드
    knowledge/working-agreements.md
    skills/
      delegation/SKILL.md           하위 에이전트 라우팅 가이드
      agenda/{SKILL.md,tools.yaml}  예시 도구 바인딩 (demo-agenda)
    agents/
      developer/                    코드 설명, 리뷰, 버그 분석 (읽기 전용)
        AGENT.md  knowledge/coding-rules.md  skills/code-review/SKILL.md
      designer/                     UI/UX 리뷰, 디자인 원칙, 접근성
        AGENT.md  knowledge/design-principles.md  skills/design-review/SKILL.md
      pm/                           명세, 일정, 우선순위, 리마인더
        AGENT.md  memory/priorities.md  skills/{spec-writing,reminders}/SKILL.md
      ops/                          상태 점검, 장애 런북 (승인 게이트 쓰기)
        AGENT.md  env.yaml  knowledge/escalation.md  skills/incident-runbook/SKILL.md
```

**폴더 계층이 곧 위임과 상속 구조입니다.** `main/ops`는 `rules.md` -> `main/AGENT.md` -> `ops/AGENT.md`를 순서대로 받습니다. 각 에이전트는 자신과 조상의 스킬·지식만 볼 수 있으며 형제 에이전트의 항목은 보지 못합니다. `env.yaml`은 위에서 아래로 상속되고 자식이 덮어쓸 수 있습니다(`ops/env.yaml`이 `TEAM_TIMEZONE`을 `UTC`로 재정의). 메모리 시드는 상속되지 않으며, 위임은 직계 자식에게만 가능합니다.

## 파일별 역할

| 파일 | 역할 |
|---|---|
| `profile.yaml` | 프로필 `id`(폴더명과 일치), `timezone`, `model`, `roles`, `members`, `bindings`, `secrets`(이름만), `policy`(`scopes`, `agents`, `approve`). |
| `rules.md` | 모든 에이전트 프롬프트 앞에 붙는 프로필 공통 규칙. |
| `knowledge/` | 필요 시 출처 인용과 함께 조회(요청당 최대 8개 문서 / 32 KiB); 에이전트 `knowledge/`는 해당 에이전트와 자손에게 공개. |
| `skills/<name>/SKILL.md` | 절차적 지침; 프로필 `skills/`는 전체 에이전트 공유, 에이전트 `skills/`는 해당 에이전트와 자손 공유. |
| `skills/<name>/tools.yaml` | 도구 선언: `server` 별칭, `id`, 위험 등급 `class`(`read`/`write`/`irreversible`), `scopes`, `input`, `env`, `secrets`, `egress`. |
| `agents/*/AGENT.md` | 역할 프롬프트 및 선택적 YAML 프런트매터(`description`, `builtin`, `model`). |
| `agents/*/env.yaml` | 비비밀 키/값 맵; 해당 도구의 `env`에 명시된 키만 도구에 전달. |
| `agents/*/triggers.yaml` | 정적 cron, 이벤트, HTTP 트리거(이 템플릿은 평일 09:00 스탠드업 요약을 정의). |
| `agents/*/memory/*.md` | 최초 1회 부트스트랩되는 메모리 시드; 멤버 `owner` 필수, `visibility` 기본값은 `private`. |

## 권한 한눈에 보기

유효 스코프는 세 집합의 교집합(`role scopes ∩ policy.scopes ∩ policy.agents`)으로 결정됩니다(생략된 에이전트는 부모 상한을 상속).

| 에이전트 | 스코프 상한 | 가능 작업 |
|---|---|---|
| `main` | 호출자 역할 기준(`lead`는 전체) | 일정 조회, 위임, 예약 |
| `main/developer` | `run` | MCP 도구 없음; 내장 `read`로 코드 조회 |
| `main/designer` | `run` | 도구 없음; 텍스트·표 기반 리뷰만 수행 |
| `main/pm` | `run`, `calendar.read`, `schedule.*` | 읽기 전용 일정 조회 + 대화 예약(`create`/`list`/`cancel`) |
| `main/ops` | `run`, `calendar.read`, `calendar.delete` | `agenda.delete` 요청 가능(`irreversible`이므로 매 호출 승인 카드 필요) |

위험한 작업은 세 겹으로 통제됩니다.
1. **도구 계약:** `class: irreversible`은 `policy.approve`가 없어도 실행 전 사람 승인을 강제합니다(`agenda.delete`).
2. **규칙과 프롬프트:** `rules.md`와 `ops/AGENT.md`는 실행 전 대상, 영향 범위, 롤백 계획을 먼저 밝히도록 요구합니다.
3. **런북과 지식:** `incident-runbook`과 `escalation.md`가 승인 기준과 연락 순서를 정의합니다.

평일 다이제스트 트리거는 `service:standup`(`digest` 역할: `run`, `calendar.read`)으로 실행됩니다. 서비스 주체는 사람이 아니므로 승인할 수 없으며, 승인 게이트 도구는 이 방식으로 자동화할 수 없습니다.

## 우리 팀에 맞게 수정하기

1. **복사:** `cp -R examples/profiles/team profiles/<your-team>`(`BOT_PROFILES` 바로 아래에는 프로필 디렉터리만 배치) 또는 `bot init <your-team> --template team`으로 새 작업 공간을 생성합니다.
2. **ID 설정:** `profile.yaml`의 `id`를 폴더명(소문자, 숫자, 하이픈)과 일치시킵니다. `policy.agents` 경로는 그대로 유지합니다.
3. **예시 내용 교체:** `knowledge/`와 `memory/` 아래 샘플 파일을 수정하거나 삭제합니다(모든 메모리 시드의 `owner`는 `members`에 존재해야 함).
4. **멤버와 입구 설정:** `local:team`을 실제 식별자로 바꾸고 호스트에 등록된 입구(CLI 계정 또는 Slack 워크스페이스/채널; `profile.yaml`의 Slack 주석 해제)를 바인딩합니다. 각 입구는 활성 프로필 전체에서 고유해야 합니다.
5. **도구 교체:** `agents/main/skills/agenda`를 실제 MCP 서버로 교체합니다(또는 폴더를 삭제하고 `roles`/`policy`에서 `calendar.*` 스코프를 뺀 뒤 `AGENT.md`/`triggers.yaml`을 수정).
6. **프로필에 비밀 제외:** `profile.yaml` `secrets`와 `tools.yaml` `secrets`에는 비밀 **이름**만 선언하고, 값은 호스트 비밀 저장소에 보관합니다. 누락된 비밀은 `bot validate`에서 거부됩니다.
7. **일정과 시간대 조정:** `timezone`, `triggers.yaml`, `env.yaml`(`TEAM_TIMEZONE`)을 수정하거나 `triggers: []`로 비웁니다.
8. **호스트에 등록:** `host.json`의 `models`에 프로필 ID를 추가하고 입구를 등록합니다([profile-spec.md](profile-spec.md#새-프로필의-호스트-설정)). 데모 호스트는 `personal`과 `team`만 알고 있습니다.
9. **검증:** `bot validate`, `bot doctor`, `bot map --profile <your-team>`을 실행합니다.

## 하위 에이전트 추가하기

`main` 아래에 `support` 에이전트를 추가하는 예입니다.

```sh
bot add agent main/support --dir profiles/<your-team>
bot add skill triage --agent main/support --dir profiles/<your-team>
```

1. `agents/main/agents/support/AGENT.md`에 `description`(<= 200자, 한 줄)과 지침을 작성하고 필요하면 `knowledge/`, `skills/`, `memory/`, `env.yaml`을 추가합니다(스킬 이름은 가시 조상과 충돌할 수 없음).
2. `profile.yaml`의 `policy.agents`에 명시적 상한을 추가합니다(예: `main/support: [run]`). 생략하면 `main`의 전체 상한을 상속받습니다.
3. `main`이 위임 기준을 알 수 있도록 `agents/main/skills/delegation/SKILL.md`에 라우팅 행을 추가합니다.
4. 필요하면 `agents/<name>/` 아래로 더 중첩할 수 있습니다(최대 8단계; 위임은 여전히 직계 자식만 가능).
5. `bot validate`와 `bot map --profile <your-team>`으로 검증합니다.

모든 이름은 `^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$`(<= 63자)와 일치해야 합니다. 오타가 난 `trigger.yaml` 같은 알 수 없는 파일명은 조용히 무시되지 않고 검증에 실패합니다.

## 알아두면 좋은 점

- `main/developer`는 `builtin: [read]`를 선언하므로 읽기 격리가 검증된 `claude-cli` VM이 필요합니다. 직접 API 공급자(`anthropic-api`, `openai-api`, [architecture.md](architecture.md) 참고)에서 `main/developer`를 실행하려면 `builtin`을 제거하세요.
- `main`과 `main/pm`은 `anthropic-api`, `openai-api`, `claude-cli` 모두에서 동일한 대화 예약 도구를 공유합니다. `lead`는 예약을 생성·조회·취소할 수 있고 `member`는 조회할 수 있습니다. 등록된 다른 Slack 채널을 대상으로 지정하려면 생성 시 소유자 승인이 필요하며, 발화 시마다 `Delivery draft` 카드를 승인해야 합니다. 프로필 단위의 반복 작업은 `triggers.yaml`을 사용하며, 서비스 주체는 사용자 예약을 생성할 수 없습니다([plugins.md](plugins.md#예약과-후속-깨우기wake)).
- 모든 예시 이름(`alice`, `bob` 등)은 가상의 값입니다. 실제 식별자와 비밀은 오직 `host.json`, `members`, 비밀 저장소에만 둡니다.

관련 문서: [프로필 명세](profile-spec.md) · [보안](security.md) · [MCP 도구](plugins.md) · [게이트웨이](gateway.md)

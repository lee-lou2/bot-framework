# 팀 프로필 템플릿

`examples/profiles/team`은 **팀 리드 한 명 아래에 개발·디자인·기획·운영 에이전트를 나눠 둔** 프로필 하나다. 프로필 사양이 정한 요소(규칙·지식·스킬·기억·설정·예약·권한)를 한 번씩 보여 주는 복사용 틀이고, 외부 도구 서버 없이 데모 모델로 바로 돈다. 사람·회사·채널은 모두 합성 예시다.

프로필 폴더 안에는 `profile.yaml`·`rules.md`·`agents`·`knowledge`·`skills`만 둘 수 있어서(알려진 이름만 받는다) 이 안내서는 폴더 밖에 있다.

## 먼저 돌려 보기

저장소 루트에서 README의 데모와 같은 준비를 한 뒤 아래를 실행한다. 내장 데모 호스트는 `personal`과 `team`을 모두 알고 있다.

```sh
export BOT_PROFILES="$PWD/examples/profiles"
bot validate                                   # 프로필 2개(personal, team) 검증 통과
bot map --profile team                         # 위임 트리와 에이전트별 권한 상한·스킬·도구
bot chat --profile team "오늘 일정 알려줘"       # 데모 모델이 agenda.list로 합성 일정을 답한다
bot chat --profile team --request-id team-1 "산책 일정을 삭제해줘"   # 승인 카드가 뜬다(관리 화면에서 승인)
```

데모 모델은 위임을 하지 않는다. 팀 리드가 하위 에이전트에게 일을 맡기는 흐름은 위임을 호출하는 실제 모델 별칭을 호스트에 연결했을 때 동작한다. 데모에서도 폴더 구조·권한 상한·승인 규칙은 그대로 검증된다.

## 폴더 구조

```text
examples/profiles/team/
  profile.yaml                      역할·멤버·입구·권한 상한
  rules.md                          모든 에이전트가 따르는 공통 규칙
  knowledge/                        팀 전체가 참고하는 문서(.md·.txt)
    team-intro.md  glossary.txt
  skills/                           모든 에이전트에게 보이는 공용 스킬
    report-format/SKILL.md
  agents/main/                      팀 리드(입구가 연결되는 에이전트)
    AGENT.md  env.yaml  triggers.yaml
    memory/team-profile.md          최초 기동 때 등록하는 기억(팀 공유)
    knowledge/working-agreements.md
    skills/
      delegation/SKILL.md           누구에게 맡길지
      agenda/{SKILL.md,tools.yaml}  도구 선언 예시(예시 서버 demo-agenda)
    agents/
      developer/                    코드 설명·리뷰·버그 분석(읽기 전용)
        AGENT.md  knowledge/coding-rules.md  skills/code-review/SKILL.md
      designer/                     UI/UX 리뷰·디자인 원칙·접근성
        AGENT.md  knowledge/design-principles.md  skills/design-review/SKILL.md
      pm/                           요구사항·일정·우선순위·예약 알림
        AGENT.md  memory/priorities.md  skills/{spec-writing,reminders}/SKILL.md
      ops/                          상태 점검·장애 대응 절차(위험한 작업은 승인)
        AGENT.md  env.yaml  knowledge/escalation.md  skills/incident-runbook/SKILL.md
```

**폴더 계층이 곧 위임 트리이고 규칙 상속이다.** `agents/main/agents/ops`의 지침은 `rules.md` → `main/AGENT.md` → `ops/AGENT.md` 순으로 합쳐진다. 스킬과 지식은 자기와 조상의 것만 보이고(형제의 것은 안 보인다), `env.yaml`은 조상 것을 이어받되 같은 이름은 자식 값이 이긴다. 기억 시드는 상속하지 않는다. 위임은 직계 자식에게만 할 수 있다.

## 각 파일이 하는 일

| 파일 | 하는 일 |
|---|---|
| `profile.yaml` | 프로필 ID(폴더명과 같아야 한다)·시간대·모델 별칭, `roles`(스코프 묶음), `members`(누가 어떤 역할인지), `bindings`(외부 입구 → 에이전트), `secrets`(자격의 이름), `policy`(전체 상한·에이전트별 상한·승인 스코프) |
| `rules.md` | 모든 에이전트의 지침 맨 앞에 붙는 공통 규칙 |
| `knowledge/` | 질문에 맞는 문서를 찾아 근거로 쓴다(한 요청에 최대 8개·32 KiB, 출처 표시). 에이전트 폴더의 `knowledge/`는 그 에이전트와 자손만 본다 |
| `skills/<이름>/SKILL.md` | 일하는 요령을 글로 적은 스킬. 프로필 `skills/`는 모두에게, 에이전트의 `skills/`는 그 에이전트와 자손에게 보인다 |
| `skills/<이름>/tools.yaml` | 스킬이 쓰는 도구의 선언: 서버 별칭, 도구 ID, 위험 등급(`read`·`write`·`irreversible`), 필요한 스코프, 입력 스키마, 환경·비밀·접속 허용 목록 |
| `agents/*/AGENT.md` | 에이전트의 지침. 맨 위 front matter의 `description`(한 줄 설명)·`builtin`(읽기 전용 내장 도구)·`model`은 선택이다 |
| `agents/*/env.yaml` | 비밀이 아닌 설정. 도구의 `env: [이름]`에 적은 것만 그 도구에 전달된다 |
| `agents/*/triggers.yaml` | 예약(cron)·이벤트·HTTP로 도구나 지시문을 부르는 선언. 이 예시는 평일 09:00 팀 요약 하나다 |
| `agents/*/memory/*.md` | 최초 기동 때 한 번 등록하는 기억. front matter에 `owner`(멤버 신원)가 필수이고 `visibility`를 생략하면 private이다 |

## 권한 한눈에 보기

권한은 세 겹의 교집합이다: 사람의 역할 스코프 ∩ `policy.scopes` ∩ 에이전트 상한(`policy.agents`, 적지 않으면 부모 것).

| 에이전트 | 스코프 상한 | 할 수 있는 일 |
|---|---|---|
| `main` | 역할에 따름(리드는 전부) | 일정 조회, 위임, 예약 |
| `main/developer` | `run` | 도구 없음. 읽기 전용 내장 도구 `read`로 코드를 읽는다 |
| `main/designer` | `run` | 도구 없음. 글로만 리뷰한다 |
| `main/pm` | `run`, `calendar.read`, `schedule.*` | 일정 조회, 대화에서 예약 만들기·조회·취소 |
| `main/ops` | `run`, `calendar.read`, `calendar.delete` | 일정 삭제를 시도할 수 있다. 삭제는 `irreversible`이라 **호출마다 승인 카드**가 뜬다 |

위험한 작업이 승인을 받는 방식은 세 곳에 나뉘어 있다. 모두 합쳐서 읽어야 한다.
1. **도구 선언**: `class: irreversible`인 도구는 `policy.approve`에 없어도 항상 사람 승인이 필요하다. 승인 전에는 실행되지 않는다(`agenda.delete`).
2. **규칙**: `rules.md`와 ops의 `AGENT.md`가 대상·영향·되돌리는 방법을 먼저 적게 한다.
3. **문서**: ops의 `incident-runbook` 스킬과 `escalation` 지식이 어떤 작업이 승인 대상이고 무엇을 적어 요청할지 정한다.

정기 요약 트리거의 서비스 주체(`service:standup`)는 읽기 역할(`digest`)만 가져서 승인 대상 도구를 부를 수 없다. 사람이 아니라 승인도 못 하므로, 승인이 필요한 일은 이 방식으로 자동화하지 않는다.

## 복사해서 내 팀으로 바꾸는 순서

1. **복사**: `cp -R examples/profiles/team profiles/<내팀>`. `BOT_PROFILES`가 가리키는 폴더 바로 아래에는 프로필 폴더만 둔다.
2. **ID 바꾸기**: 폴더 이름을 정하고 `profile.yaml`의 `id`를 같게 맞춘다. 이름은 소문자·숫자·하이픈이다. `policy.agents`의 경로는 에이전트 폴더 이름이라 그대로 둔다.
3. **합성 내용 지우기**: `knowledge/team-intro.md`·`glossary.txt`, `agents/main/knowledge/working-agreements.md`, 각 에이전트의 `knowledge`와 `memory`는 예시다. 내 팀의 내용으로 바꾸거나 파일을 지운다(폴더가 비어도 된다). 기억 파일의 `owner`는 `members`에 있는 신원이어야 한다.
4. **사람과 입구 바꾸기**: `members`의 `local:team`을 내 신원으로 바꾸고, `bindings`는 호스트에 등록한 입구 별칭(CLI 계정·Slack 워크스페이스와 채널)으로 바꾼다. 주석으로 둔 Slack 예시를 풀고 `<사용자 ID>` 자리에 Slack 프로필에서 복사한 ID를 넣으면 된다. 입구 별칭은 활성 프로필 전체에서 하나뿐이다.
5. **도구 바꾸기**: `agents/main/skills/agenda`는 예시 서버를 쓰는 선언이다. 실제 도구 서버를 호스트에 등록하고 `tools.yaml`의 `server`와 도구를 바꾼다. 쓰지 않으면 스킬 폴더를 지우고, 그 도구의 스코프(`calendar.*`)를 `roles`·`policy`에서 빼고, 이를 언급하는 지침(`AGENT.md`·`SKILL.md`)과 `triggers.yaml`의 지시문도 고친다.
6. **비밀은 이름만**: 도구가 자격이 필요하면 `profile.yaml`의 `secrets`에 이름(예: `tracker-reader`)을 적고 `tools.yaml`의 `secrets`에서 환경 변수에 연결한다. 값은 호스트의 비밀 보관소에 둔다. 값은 어떤 프로필 파일에도 쓰지 않는다. `bot validate`는 선언한 비밀이 보관소에 없으면 통과하지 않는다.
7. **시간대와 예약**: `timezone`, `triggers.yaml`의 cron, `env.yaml`의 `TEAM_TIMEZONE`을 맞춘다. 정기 요약이 필요 없으면 `triggers.yaml`을 `triggers: []`로 두거나 지운다.
8. **호스트 등록**: 호스트 파일의 `models`에 이 프로필 ID와 모델 별칭을, `gateway`에 입구를 등록한다([호스트 파일 예시](profile-spec.md#새-프로필의-호스트-설정)). 내장 데모 호스트는 `team`이라는 이름의 이 예시만 알고 있어서 이름을 바꾸면 호스트 파일이 필요하다.
9. **검증**: `bot validate`가 통과하는지 보고 `bot map --profile <내팀>`으로 트리와 권한을 확인한다.

## 에이전트를 더 추가하는 방법

예: 고객 지원 담당 `support`를 팀 리드 아래에 더한다.

1. `agents/main/agents/support/AGENT.md`를 만든다. 맨 위에 `description`(한 줄, 200자 이하)을 적으면 팀 리드가 무엇을 기대할지 알 수 있다.
2. 필요하면 같은 폴더에 `knowledge/`, `skills/<이름>/SKILL.md`, `memory/`, `env.yaml`을 둔다. 스킬 이름은 보이는 범위(조상과 자기) 안에서 겹치면 안 된다.
3. `profile.yaml`의 `policy.agents`에 `main/support: [run]`처럼 상한을 적는다. 적지 않으면 부모의 상한을 그대로 받으니, **안전한 쪽은 필요한 스코프만 명시하는 것**이다.
4. 팀 리드가 알아보도록 `agents/main/skills/delegation/SKILL.md`의 표에 한 줄을 더한다.
5. 더 깊게 나누고 싶으면 그 에이전트 폴더 안에 `agents/<이름>/`을 만든다(최대 8단계, 위임은 직계 자식에게만).
6. `bot validate`와 `bot map --profile <프로필>`로 확인한다.

이름은 `[a-z][a-z0-9]*(-[a-z0-9]+)*` 모양의 63자 이하다. 알려진 이름이 아닌 파일(오타 난 `trigger.yaml` 등)은 조용히 무시되지 않고 검증 오류가 된다.

## 알아 둘 것

- `main/developer`는 `builtin: [read]`를 선언한다. 이 파일 도구를 쓰려면 읽기 격리를 보증하는 Claude CLI VM에 개발자의 모델 별칭을 연결한다. API 직접 호출 실행기로 개발자를 실행하려면 파일 도구 선언을 지운다. [모델 실행기 선택](architecture.md#모델-실행기-선택)
- 팀 리드와 `main/pm`은 `anthropic-api`, `openai-api`, `claude-cli`에서 같은 예약 도구를 쓴다. 이 예시의 `lead`는 자기 예약을 생성·조회·취소하고, `member`는 자기 예약을 조회한다. 기본 수신자는 원래 대화다. Slack에서 등록된 다른 수신자를 지정하면 소유자가 생성할 때 수신자와 지시를 승인하고, 예약이 발화할 때마다 전송할 답을 승인한다. 프로필에 미리 정해 두는 반복 실행은 `triggers.yaml`에 적는다. 서비스 주체는 사용자 예약을 만들 수 없다. [예약과 후속 확인](plugins.md#예약과-후속-확인)
- 이 폴더의 모든 사람 이름(alice·bob 등)과 신원·채널 표기는 합성이다. 실제 신원은 호스트 설정과 `members`에서만 정하고, 비밀 값은 프로필에 쓰지 않는다.

관련 문서: [프로필 작성](profile-spec.md) · [보안](security.md) · [MCP 도구](plugins.md) · [게이트웨이](gateway.md)

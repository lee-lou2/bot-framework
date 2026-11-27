//! `bot init`·`bot add`가 쓰는 시작 파일. 모두 합성 내용이다(실제 이름·계정·주소·비밀 없음). `__ID__`는 프로필 ID(YAML에는 항상 따옴표 글로 쓴다: `null`·`true`도 이름 규칙에 맞지만 따옴표 없이는 글이 아니다),
//! `__UID__`는 이 OS 사용자 번호로 바꿔 쓴다. 도구·서버·예약은 넣지 않는다(권한을 넓히지 않는 가장 작은 시작점).
use crate::cli::Template;

/// (프로필 폴더 안의 상대 경로, 내용)
pub type Entry = (&'static str, &'static str);

pub fn files(template: Template) -> &'static [Entry] {
    match template {
        Template::Personal => PERSONAL,
        Template::Team => TEAM,
        Template::Company => COMPANY,
    }
}

/// 템플릿이 만드는 에이전트 수(안내 출력용)
pub fn agent_count(template: Template) -> usize {
    files(template)
        .iter()
        .filter(|(path, _)| path.ends_with("/AGENT.md"))
        .count()
}

/// 호스트 설정 예시: 데모 모델과 이 OS 사용자에 연결한 CLI 계정 `owner`뿐이다(외부 서비스·서버·키 없음).
pub const HOST_JSON: &str = r##"{
  "version": 1,
  "gateway": {"cli": {"accounts": {"owner": {"uid": __UID__}}}},
  "models": {"__ID__": ["assistant"]},
  "providers": {"assistant": {"kind": "demo"}}
}
"##;

pub fn agent_md(leaf: &str) -> String {
    format!(
        "---\ndescription: {leaf} — 이 에이전트가 맡을 일을 한 줄로 적어 주세요\n---\n\
         너는 이 팀의 {leaf} 에이전트다. 맡은 일을 아래에 적는다.\n\n\
         ## 하는 일\n- 맡은 영역의 요청에 답한다.\n\n\
         ## 하지 않는 일\n- 맡은 영역 밖의 일은 직접 답하지 않고 부모 에이전트에게 돌려보낸다.\n- 권한이나 도구가 없는 일을 한 것처럼 말하지 않는다.\n"
    )
}

pub fn skill_md(name: &str) -> String {
    if name == "skill-creator" {
        return SKILL_CREATOR.to_owned();
    }
    format!(
        "---\nname: \"{name}\"\ndescription: 이 스킬을 언제 쓰는지 한 줄로 적어 주세요\n---\n\
         # {name}\n\n이 스킬이 하는 일과 쓰는 때를 두세 문장으로 적는다.\n\n\
         ## 순서\n1. 먼저 확인할 것\n2. 그다음 할 일\n3. 답에 넣을 것\n"
    )
}

/// 슈퍼 관리자 전용 스킬·MCP 생성·수정·삭제 스킬(`bot add skill skill-creator`).
/// Claude·Codex skill-creator의 핵심 원칙(간결·명확·판단을 바꾸는 규칙 중심)과 `bot-framework`의
/// 64 KiB 한도·SOPS 암호화 비밀·즉시 활성화 구조를 함께 담는다.
pub const SKILL_CREATOR: &str = r##"---
name: "skill-creator"
description: "슈퍼 관리자의 자연어 요청으로 에이전트 스킬을 생성·수정·삭제하거나 MCP 서버를 추가·제거하고 즉시 활성화한다. '스킬 만들어줘', '스킬 수정/삭제해줘', 'MCP 추가/제거해줘' 요청에 사용한다."
---
# skill-creator

슈퍼 관리자의 자연어 요청을 받아 프로필의 스킬과 MCP 서버를 설계·적용하고 즉시 활성화한다.

## 1. 권한 확인
- 이 작업은 프로필의 `super_admins`에 등록된 슈퍼 관리자만 요청할 수 있다.
- 관리 도구(`skill.inspect`·`skill.create`·`skill.update`·`skill.delete`·`mcp.add`·`mcp.remove`)가 보이지 않거나 거절되면 권한이 없음을 분명히 알리고 작업을 진행하지 않는다.

## 2. 현황 먼저 조회
- 스킬을 만들거나 고치기 전에 항상 `skill.inspect({})`를 호출해 에이전트 목록(`id`·`description`), 기존 스킬과 이름 충돌 여부, 에이전트별 스킬 본문 용량(64 KiB 한도), 등록된 MCP 서버, 비밀 보관소 종류(`sops`·`keychain`·`env`)와 비밀 **이름**을 확인한다.
- 기존 스킬을 수정할 때는 `skill.inspect({"skill": "<이름>"})`으로 현재 본문을 먼저 읽는다.

## 3. 대상 에이전트 고르기
- 사용자가 에이전트를 명시하면 그 에이전트에 둔다.
- 명시하지 않으면 `skill.inspect`로 읽은 각 에이전트의 `description`과 역할을 비교해 가장 알맞은 에이전트 하나를 고른다.
- 모든 에이전트가 공통으로 지켜야 하는 규칙·형식만 프로필 공용(`agent: null`)으로 둔다.

## 4. 심플하고 명확한 스킬 작성 원칙
- **이름(`name`)**: 소문자 kebab-case(예: `pr-review`, `weekly-summary`).
- **설명(`description`)**: 언제 발동하고 무엇을 하는지 1~2문장으로 구체적으로 적는다(포괄적 설명 금지).
- **본문(`body`)**:
  - 모델이 이미 아는 일반 설명·군더더기는 빼고, 실제 판단과 출력을 바꾸는 핵심 절차·출력 형식·제약·예외 처리만 명령형으로 짧게 적는다.
  - YAML front matter(`---`)는 본문에 넣지 않는다(`skill.create`·`skill.update`가 자동 생성한다).
  - `allowed-tools`는 쓸 수 없으며, 대상 에이전트의 전체 스킬 본문 합계가 64 KiB를 넘지 않게 유지한다.
- 생성·수정·삭제 후에는 자동으로 검증과 즉시 활성화가 끝나 다음 요청부터 바로 동작한다.

## 5. MCP 추가·제거와 암호화 파일(SOPS) 처리
- **원격 Streamable HTTP MCP**: `mcp.add(transport: "http", alias, url, ...)`를 호출하면 서버의 `tools/list`를 검증해 `tools.lock.yaml`·`tools.yaml`·`SKILL.md`·`host.json`·`profile.yaml`을 갱신하고 즉시 활성화한다.
- **로컬 stdio MCP**: `mcp.add(transport: "stdio", alias, root, command, stdio_tools, ...)`로 등록한다.
- **위험도(`tool_classes` · `default_class`)**: 읽기 전용 도구는 `"read"`, 삭제·발송 등 되돌릴 수 없는 도구는 `"irreversible"`, 그 밖의 변경 도구는 `"write"`로 지정한다.
- **비밀·암호화 파일 규칙**:
  - 토큰·API 키 **값**은 절대 `SKILL.md`·`tools.yaml`·`tools.lock.yaml`·`profile.yaml`·`host.json`·답변에 적지 않는다.
  - SOPS 암호화 파일(`BOT_SECRETS=sops:<파일>`)이나 키체인에 이미 저장된 비밀이 있으면 `auth_secret: "<비밀 이름>"`처럼 이름만 연결한다(SOPS 캐시는 자동으로 갱신된다).
  - 슈퍼 관리자가 새 비밀 값을 함께 전달했을 때만 `secret_value`(또는 `secret_values`)로 넘겨 암호화 보관소(`bot.<profile>.<name>`)에 바로 저장하고, 답변에는 값을 되풀이하지 않는다.
- **MCP 제거**: `mcp.remove(alias, ...)`로 서버와 스킬 바인딩을 제거한다. 미사용 비밀까지 암호화 보관소에서 지우려면 사용자가 명시적으로 요청했을 때만 `remove_secrets: true`를 준다.

## 6. 완료 보고
- 적용 범위(에이전트 ID 또는 프로필 공용), 생성·수정·삭제한 스킬·도구 이름, 새 판본(앞 12자), 바로 사용 가능 여부를 짧게 보고한다.
"##;

const RULES: &str = r##"- 답은 한국어로 짧게 쓴다. 결론을 먼저 말하고 근거와 다음 행동을 뒤에 붙인다.
- 모르는 것은 모른다고 답한다. 확인하지 못한 내용을 사실처럼 쓰지 않는다.
- 사람의 이름·연락처·계정 같은 개인정보와 비밀 값(토큰·비밀번호)은 답에 옮기지 않는다.
- 되돌릴 수 없는 일(삭제·배포·계정 변경)은 대상과 영향을 먼저 적고, 사람이 승인한 뒤에만 한다.
"##;

const REPORT_FORMAT: &str = r##"---
name: report-format
description: 보고할 때 결론·근거·다음 행동·확인하지 못한 것 순서로 쓴다
---
# 보고 형식

프로필 `skills/`에 둔 스킬은 모든 에이전트에게 보인다. 모두가 같은 모양으로 보고하게 하는 규칙을 둔다.

1. **결론**: 한두 문장.
2. **근거**: 파일·문서 같은 출처를 목록으로.
3. **다음 행동**: 누가, 무엇을, 언제까지. 없으면 "없음".
4. **확인하지 못한 것**: 추측과 사실을 가른다.

하위 에이전트의 보고를 합칠 때도 이 순서를 유지한다.
"##;

// ── personal ──────────────────────────────────────────────────────────────────────────────────

const PERSONAL: &[Entry] = &[
    ("profile.yaml", PERSONAL_PROFILE),
    ("rules.md", RULES),
    ("agents/main/AGENT.md", PERSONAL_MAIN),
];

const PERSONAL_PROFILE: &str = r##"# 개인 비서 프로필: 가장 작은 모양. 항목별 설명은 docs/profile-spec.md에 있다.
version: 1
id: "__ID__"                  # 폴더 이름과 같아야 한다(소문자·숫자·하이픈)
timezone: UTC                 # IANA 시간대. 예약(cron)을 쓰게 되면 이 시간대로 읽는다
model: assistant              # 호스트(host.json)가 이 프로필에 허용한 모델 별칭
roles:
  owner: [run]                # 역할 = 스코프 묶음. run은 에이전트와 대화할 권한이다
members:
  - {identity: "local:owner", roles: [owner]}   # CLI 계정 owner(host.json이 이 OS 사용자에 연결한다)
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}   # 터미널 입구 → main 에이전트
secrets: []                   # 도구가 쓰는 자격의 '이름'만 적는다. 값은 호스트 보관소에 둔다
# 권한 상한은 policy로 줄일 수 있다. 적지 않으면 역할 스코프의 합집합이 상한이고, 하위 에이전트는 부모의 상한을 이어받는다:
# policy:
#   scopes: [run]
#   agents:
#     main/research: [run]
#   approve: []
"##;

const PERSONAL_MAIN: &str = r##"---
description: 사용자의 일을 돕는 개인 비서
---
너는 사용자의 개인 비서다. 요청을 받으면 무엇을 원하는지 한 문장으로 정리하고, 직접 답할 수 있는 것은 짧게 답한다.

## 하는 일
- 할 일·메모·계획에 대한 질문에 간단히 답한다.
- 확인이 필요한 것은 추측하지 말고 되묻는다.

## 하지 않는 일
- 도구나 권한이 없는 일을 한 것처럼 말하지 않는다.
"##;

// ── team ──────────────────────────────────────────────────────────────────────────────────────

const TEAM: &[Entry] = &[
    ("profile.yaml", TEAM_PROFILE),
    ("rules.md", RULES),
    ("knowledge/team-intro.md", TEAM_INTRO),
    ("skills/report-format/SKILL.md", REPORT_FORMAT),
    ("agents/main/AGENT.md", TEAM_MAIN),
    ("agents/main/skills/delegation/SKILL.md", TEAM_DELEGATION),
    ("agents/main/agents/developer/AGENT.md", TEAM_DEVELOPER),
    ("agents/main/agents/designer/AGENT.md", TEAM_DESIGNER),
    ("agents/main/agents/pm/AGENT.md", TEAM_PM),
];

const TEAM_PROFILE: &str = r##"# 팀 프로필: 팀 리드(main) 아래에 개발·디자인·기획 에이전트. examples/profiles/team을 줄인 시작점이다.
version: 1
id: "__ID__"                  # 폴더 이름과 같아야 한다(소문자·숫자·하이픈)
timezone: Asia/Seoul          # IANA 시간대. 예약(cron)을 쓰게 되면 이 시간대로 읽는다
model: assistant              # 호스트(host.json)가 이 프로필에 허용한 모델 별칭
roles:
  lead: [run]                 # 역할 = 스코프 묶음. run은 에이전트와 대화할 권한이다
members:
  - {identity: "local:owner", roles: [lead]}   # CLI 계정 owner(host.json이 이 OS 사용자에 연결한다)
  # 팀원은 Slack 신원으로 더한다. 워크스페이스 별칭은 호스트에 등록하고 사용자 ID는 Slack 프로필에서 복사해 쓴다:
  # - {identity: "slack:<워크스페이스 별칭>/<사용자 ID>", roles: [lead]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}   # 사람은 main(팀 리드)에게만 말을 건다
secrets: []                   # 도구가 쓰는 자격의 '이름'만 적는다. 값은 호스트 보관소에 둔다
policy:
  scopes: [run]               # 전체 상한
  agents:                     # 에이전트별 추가 상한. 적지 않은 에이전트는 부모의 상한을 이어받는다
    main/developer: [run]
    main/designer: [run]
    main/pm: [run]
  approve: []                 # 승인이 필요한 write 스코프. irreversible 도구는 여기에 없어도 항상 승인받는다
"##;

const TEAM_INTRO: &str = r##"# 팀 소개 (예시)

이 파일을 우리 팀 소개로 바꿔 쓴다. 에이전트는 팀에 대한 질문에 `knowledge/`를 근거로 답하고 출처를 붙인다.

- 팀 이름: 예시 팀
- 하는 일: 제품을 만들고 운영한다.
- 일하는 방식: 요청은 팀 리드가 받아 알맞은 에이전트에게 맡긴다.
"##;

const TEAM_MAIN: &str = r##"---
description: 요청을 받아 알맞은 하위 에이전트에게 맡기고 결과를 모아 답하는 팀 리드
---
너는 이 팀의 리드다. 사람의 요청을 받아, 직접 답할 수 있는 것은 짧게 답하고 전문 영역이면 알맞은 하위 에이전트에게 맡긴 뒤 결과를 모아 답한다.

## 하는 일
- 요청을 한 문장으로 다시 정리하고 무엇을 누구에게 맡길지 정한다(`delegation` 스킬).
- 하위 에이전트의 결과를 그대로 붙이지 않는다. `report-format` 순서(결론·근거·다음 행동·확인하지 못한 것)로 합치고 어느 에이전트의 말인지 밝힌다.
- 팀 소개는 knowledge를 근거로 답하고 출처를 붙인다.

## 하지 않는 일
- 코드·디자인·기획을 추측으로 단정하지 않는다. 모르면 해당 하위 에이전트에게 맡기거나 모른다고 답한다.
- 하위 에이전트의 권한을 넘는 일을 시키지 않는다.
"##;

const TEAM_DELEGATION: &str = r##"---
name: delegation
description: 요청의 성격에 따라 어떤 하위 에이전트에게 맡길지 정한다
---
# 위임: 누구에게 맡길까

요청 한 건은 한 에이전트에게 맡기고, 여러 영역에 걸치면 영역별로 나눠 각각 맡긴다. 맡길 수 있는 것은 직계 자식뿐이다.

| 요청의 성격 | 맡길 에이전트 |
|---|---|
| 코드 설명, 코드 리뷰, 버그 원인 분석 | `main/developer` |
| 화면·흐름·문구 리뷰, 디자인 원칙 | `main/designer` |
| 요구사항 정리, 일정·우선순위 | `main/pm` |

## 맡길 때
- 지시 한 번에 목표·배경·원하는 결과의 모양(표, 목록, 한 문단)을 적는다. 하위 에이전트는 이 대화 전체를 모른다.
- 권한 상한은 profile.yaml의 `policy.agents`에 있다. 위험한 작업을 시키기 전에 그 에이전트에게 권한이 있는지 본다.

## 받은 뒤
- 결과를 그대로 붙이지 않는다. 결론, 근거, 다음 행동 순으로 합치고 어느 에이전트의 말인지 밝힌다.
- 서로 다른 결론이 나오면 감추지 말고 차이를 그대로 보여 준다.
"##;

const TEAM_DEVELOPER: &str = r##"---
description: 코드 설명·리뷰·버그 원인 분석을 맡는 개발자
---
너는 이 팀의 개발자다. 코드와 설계에 대한 질문에 근거를 들어 답한다.

## 하는 일
- 코드 설명, 코드 리뷰, 버그 원인 분석을 맡는다.
- 확인하지 못한 가정은 가정이라고 밝힌다.

## 하지 않는 일
- 코드를 실행하거나 배포하지 않는다(이 에이전트에게는 그런 도구와 권한이 없다).
"##;

const TEAM_DESIGNER: &str = r##"---
description: 화면·흐름·문구 리뷰와 디자인 원칙을 맡는 디자이너
---
너는 이 팀의 디자이너다. 화면과 흐름, 문구를 사용자 입장에서 살펴본다.

## 하는 일
- 화면·흐름·문구를 리뷰하고 바꿀 점을 이유와 함께 적는다.
- 접근성(대비·글자 크기·키보드 사용)을 함께 본다.

## 하지 않는 일
- 근거 없이 취향만으로 단정하지 않는다.
"##;

const TEAM_PM: &str = r##"---
description: 요구사항 정리와 일정·우선순위를 맡는 기획자
---
너는 이 팀의 기획자다. 요구사항을 정리하고 우선순위를 제안한다.

## 하는 일
- 요청을 목표·범위·완료 기준으로 정리한다.
- 우선순위는 이유와 함께 제안하고 결정은 사람에게 맡긴다.

## 하지 않는 일
- 일정을 약속하지 않는다. 근거가 있는 추정만 말한다.
"##;

// ── company ───────────────────────────────────────────────────────────────────────────────────

const COMPANY: &[Entry] = &[
    ("profile.yaml", COMPANY_PROFILE),
    ("rules.md", RULES),
    ("knowledge/company-intro.md", COMPANY_INTRO),
    ("skills/report-format/SKILL.md", REPORT_FORMAT),
    ("agents/main/AGENT.md", COMPANY_MAIN),
    ("agents/main/skills/delegation/SKILL.md", COMPANY_DELEGATION),
    ("agents/main/agents/product/AGENT.md", COMPANY_PRODUCT),
    (
        "agents/main/agents/product/agents/engineer/AGENT.md",
        COMPANY_ENGINEER,
    ),
    (
        "agents/main/agents/product/agents/designer/AGENT.md",
        COMPANY_DESIGNER,
    ),
    ("agents/main/agents/support/AGENT.md", COMPANY_SUPPORT),
    (
        "agents/main/agents/support/agents/triage/AGENT.md",
        COMPANY_TRIAGE,
    ),
];

const COMPANY_PROFILE: &str = r##"# 회사 프로필: 매니저(main) 아래에 팀 리드(product·support)와 팀원. 팀 구조는 폴더 계층이 곧 위임 트리다.
version: 1
id: "__ID__"                  # 폴더 이름과 같아야 한다(소문자·숫자·하이픈)
timezone: Asia/Seoul          # IANA 시간대. 예약(cron)을 쓰게 되면 이 시간대로 읽는다
model: assistant              # 호스트(host.json)가 이 프로필에 허용한 모델 별칭
roles:
  owner: [run]                # 역할 = 스코프 묶음. run은 에이전트와 대화할 권한이다
members:
  - {identity: "local:owner", roles: [owner]}   # CLI 계정 owner(host.json이 이 OS 사용자에 연결한다)
  # 직원은 Slack 신원으로 더한다. 워크스페이스 별칭은 호스트에 등록하고 사용자 ID는 Slack 프로필에서 복사해 쓴다:
  # - {identity: "slack:<워크스페이스 별칭>/<사용자 ID>", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}   # 사람은 main(매니저)에게만 말을 건다
secrets: []                   # 도구가 쓰는 자격의 '이름'만 적는다. 값은 호스트 보관소에 둔다
policy:
  scopes: [run]               # 전체 상한
  agents:                     # 에이전트별 추가 상한. 적지 않은 에이전트는 부모의 상한을 이어받는다
    main/product: [run]
    main/support: [run]
  approve: []                 # 승인이 필요한 write 스코프. irreversible 도구는 여기에 없어도 항상 승인받는다
"##;

const COMPANY_INTRO: &str = r##"# 회사 소개 (예시)

이 파일을 우리 회사 소개로 바꿔 쓴다. 에이전트는 회사에 대한 질문에 `knowledge/`를 근거로 답하고 출처를 붙인다.

- 회사 이름: 예시 회사
- 팀: 제품팀(`product`), 고객지원팀(`support`)
- 일하는 방식: 요청은 매니저가 받아 알맞은 팀 리드에게 맡기고, 팀 리드가 팀원에게 나눈다.
"##;

const COMPANY_MAIN: &str = r##"---
description: 요청을 받아 알맞은 팀에 맡기고 결과를 모아 답하는 매니저
---
너는 이 회사의 매니저다. 사람의 요청을 받아, 직접 답할 수 있는 것은 짧게 답하고 팀의 일이면 알맞은 팀 리드에게 맡긴 뒤 결과를 모아 답한다.

## 하는 일
- 요청을 한 문장으로 다시 정리하고 어느 팀에 맡길지 정한다(`delegation` 스킬).
- 팀 리드의 결과를 `report-format` 순서(결론·근거·다음 행동·확인하지 못한 것)로 합치고 어느 팀의 말인지 밝힌다.
- 회사 소개는 knowledge를 근거로 답하고 출처를 붙인다.

## 하지 않는 일
- 팀의 전문 영역을 추측으로 단정하지 않는다. 모르면 해당 팀에 맡기거나 모른다고 답한다.
- 팀원에게 직접 지시하지 않는다. 팀원은 팀 리드를 통해 맡긴다.
"##;

const COMPANY_DELEGATION: &str = r##"---
name: delegation
description: 요청의 성격에 따라 어느 팀 리드에게 맡길지 정한다
---
# 위임: 어느 팀에 맡길까

요청 한 건은 한 팀에 맡기고, 여러 팀에 걸치면 팀별로 나눠 각각 맡긴다. 맡길 수 있는 것은 직계 자식(팀 리드)뿐이다.

| 요청의 성격 | 맡길 팀 |
|---|---|
| 제품 기능, 코드, 디자인 | `main/product` |
| 고객 문의, 불만 접수, 응대 문구 | `main/support` |

## 맡길 때
- 지시 한 번에 목표·배경·원하는 결과의 모양(표, 목록, 한 문단)을 적는다. 팀은 이 대화 전체를 모른다.
- 권한 상한은 profile.yaml의 `policy.agents`에 있다. 위험한 작업을 시키기 전에 그 팀에 권한이 있는지 본다.

## 받은 뒤
- 결과를 그대로 붙이지 않는다. 결론, 근거, 다음 행동 순으로 합치고 어느 팀의 말인지 밝힌다.
"##;

const COMPANY_PRODUCT: &str = r##"---
description: 제품 기능·코드·디자인 요청을 팀원에게 나눠 맡는 제품팀 리드
---
너는 제품팀 리드다. 매니저가 맡긴 요청을 팀원(`engineer`, `designer`)에게 나눠 맡기고 결과를 합쳐 돌려준다.

## 하는 일
- 코드와 설계 질문은 `engineer`에게, 화면과 문구 질문은 `designer`에게 맡긴다.
- 팀원의 결과를 합쳐 결론·근거·다음 행동으로 정리한다.

## 하지 않는 일
- 팀 밖의 일(고객 응대 등)은 직접 답하지 않고 매니저에게 돌려보낸다.
"##;

const COMPANY_ENGINEER: &str = r##"---
description: 코드 설명·리뷰·버그 원인 분석을 맡는 엔지니어
---
너는 제품팀의 엔지니어다. 코드와 설계에 대한 질문에 근거를 들어 답한다.

## 하는 일
- 코드 설명, 코드 리뷰, 버그 원인 분석을 맡는다.
- 확인하지 못한 가정은 가정이라고 밝힌다.

## 하지 않는 일
- 코드를 실행하거나 배포하지 않는다(이 에이전트에게는 그런 도구와 권한이 없다).
"##;

const COMPANY_DESIGNER: &str = r##"---
description: 화면·흐름·문구 리뷰를 맡는 디자이너
---
너는 제품팀의 디자이너다. 화면과 흐름, 문구를 사용자 입장에서 살펴본다.

## 하는 일
- 화면·흐름·문구를 리뷰하고 바꿀 점을 이유와 함께 적는다.

## 하지 않는 일
- 근거 없이 취향만으로 단정하지 않는다.
"##;

const COMPANY_SUPPORT: &str = r##"---
description: 고객 문의·불만 요청을 분류하고 팀원에게 맡기는 고객지원팀 리드
---
너는 고객지원팀 리드다. 매니저가 맡긴 문의를 `triage`에게 분류시키고 응대 방향을 정해 돌려준다.

## 하는 일
- 문의를 `triage`에게 맡겨 유형과 긴급도를 받는다.
- 응대 문구 초안은 공손하고 짧게 쓴다.

## 하지 않는 일
- 환불·계정 변경처럼 되돌릴 수 없는 일은 약속하지 않는다. 사람의 승인이 필요하다고 안내한다.
"##;

const COMPANY_TRIAGE: &str = r##"---
description: 고객 문의의 유형과 긴급도를 분류하는 분류 담당
---
너는 고객지원팀의 분류 담당이다. 문의를 읽고 유형과 긴급도를 정한다.

## 하는 일
- 문의를 한 줄로 요약하고 유형(문의·불만·장애)과 긴급도(낮음·보통·높음)를 붙인다.

## 하지 않는 일
- 고객 개인정보를 답에 옮기지 않는다.
"##;

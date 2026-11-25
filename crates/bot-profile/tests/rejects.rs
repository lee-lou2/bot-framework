//! 거절 사례 표: 기본 프로필을 한 군데만 고쳐 어떤 오류 코드가 나와야 하는지 확인한다.
mod support;

use support::{DELETE, Files, base, check, edit, host};

type Edit<'a> = (&'a str, &'a str, &'a str);

/// 호스트가 내장 도구로 예약한 정확한 ID `memory.share`를 선언하는 tools.yaml
const MEMORY_SHARE: &str = "version: 1\nserver: demo-notes\ntools:\n  - id: share\n    class: write\n    scopes: [calendar.read]\n    input: {type: object, properties: {}, additionalProperties: false}\n";

const TOOLS: &str = "agents/main/skills/agenda/tools.yaml";
const AGENT: &str = "agents/main/AGENT.md";

fn apply(edits: &[Edit]) -> Files {
    edits.iter().fold(base(), |files, (path, from, to)| {
        edit(files, path, from, to)
    })
}

fn expect(name: &str, files: &Files, code: &str) {
    match check(files) {
        Ok(_) => panic!("{name}: 거절해야 하는데 통과했어요(기대 {code})"),
        Err(report) => assert!(report.has(code), "{name}: 기대 {code}\n{report}"),
    }
}

#[test]
fn rejects_files_yaml_and_schema_mistakes() {
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "지원하지 않는 버전",
            &[("profile.yaml", "version: 1", "version: 2")],
            "schema",
        ),
        (
            "알 수 없는 필드",
            &[(
                "profile.yaml",
                "secrets: [calendar-reader]",
                "secrets: [calendar-reader]\nextra: 1",
            )],
            "schema",
        ),
        (
            "필수 필드 model 없음",
            &[("profile.yaml", "model: assistant\n", "")],
            "schema",
        ),
        (
            "대문자 모델",
            &[("profile.yaml", "model: assistant", "model: Assistant")],
            "schema",
        ),
        (
            "연속 하이픈 이름",
            &[("profile.yaml", "{id: desk,", "{id: de--sk,")],
            "schema",
        ),
        (
            "중복 키",
            &[(
                "profile.yaml",
                "model: assistant",
                "model: assistant\nmodel: fast",
            )],
            "yaml-duplicate-key",
        ),
        (
            "앵커",
            &[("profile.yaml", "model: assistant", "model: &m assistant")],
            "yaml-anchor",
        ),
        (
            "별칭",
            &[(
                "profile.yaml",
                "secrets: [calendar-reader]",
                "secrets: &s [calendar-reader]\nextra: *s",
            )],
            "yaml-anchor",
        ),
        (
            "태그",
            &[("profile.yaml", "model: assistant", "model: !!str assistant")],
            "yaml-tag",
        ),
        (
            "다중 문서",
            &[("profile.yaml", "version: 1", "version: 1\n---\nversion: 1")],
            "yaml-multi-doc",
        ),
        (
            "문법 오류",
            &[("profile.yaml", "roles:", "roles: [")],
            "yaml-syntax",
        ),
        (
            "이스케이프 제어 문자",
            &[(
                "profile.yaml",
                "model: assistant",
                "model: \"assi\\u0000stant\"",
            )],
            "yaml-control",
        ),
        (
            "BOM",
            &[("rules.md", "- 답은", "\u{feff}- 답은")],
            "text-bom",
        ),
        (
            "방향 전환 문자",
            &[(AGENT, "오늘", "오\u{202e}늘")],
            "text-control",
        ),
        ("AGENT.md 없음", &[(AGENT, DELETE, "")], "missing-file"),
        (
            "오타 난 파일",
            &[("agents/main/trigger.yaml", "", "version: 1\n")],
            "unknown-file",
        ),
        (
            "모르는 폴더",
            &[("extra/readme.md", "", "x")],
            "unknown-dir",
        ),
        (
            "루트 에이전트 둘",
            &[("agents/other/AGENT.md", "", "다른 루트")],
            "multiple-root-agents",
        ),
        (
            "SKILL.md 없음",
            &[("agents/main/skills/agenda/SKILL.md", DELETE, "")],
            "missing-file",
        ),
        (
            "내장 도구 이름 memory.share",
            &[
                ("agents/main/skills/memory/SKILL.md", "", "메모리 스킬"),
                ("agents/main/skills/memory/tools.yaml", "", MEMORY_SHARE),
            ],
            "reserved-tool",
        ),
        (
            "스킬 이름 충돌",
            &[("skills/agenda/SKILL.md", "", "공용 일정 스킬")],
            "skill-conflict",
        ),
        (
            "빈 AGENT.md",
            &[(
                AGENT,
                "오늘 일정을 조회하고 변경 전에는 정확한 일정을 확인한다.\n",
                "",
            )],
            "empty-file",
        ),
        (
            "front matter 알 수 없는 필드",
            &[(AGENT, "오늘", "---\ntools: x\n---\n오늘")],
            "schema",
        ),
        (
            "front matter 안 닫힘",
            &[(AGENT, "오늘", "---\nmodel: fast\n오늘")],
            "front-matter-unclosed",
        ),
        (
            "env 값이 문자열이 아님",
            &[("agents/main/env.yaml", "today", "[a]")],
            "schema",
        ),
        (
            "env 예약 이름",
            &[("agents/main/env.yaml", "AGENDA_VIEW: today", "BOT_X: one")],
            "reserved-env",
        ),
        (
            "지식 확장자",
            &[("knowledge/data.csv", "", "a,b")],
            "bad-document",
        ),
        (
            "기억 시드에 owner 없음",
            &[(
                "agents/main/memory/routine.md",
                "---\nowner: \"local:owner\"\n---\n",
                "",
            )],
            "memory-owner-missing",
        ),
        (
            "기억 시드 owner가 멤버 아님",
            &[(
                "agents/main/memory/routine.md",
                "local:owner",
                "local:stranger",
            )],
            "memory-owner-unknown",
        ),
        (
            "기억 시드 visibility 오류",
            &[(
                "agents/main/memory/routine.md",
                "---\n",
                "---\nvisibility: everyone\n",
            )],
            "schema",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
}

#[test]
fn rejects_profile_yaml_references() {
    let owner = "{identity: \"local:owner\", roles: [owner]}";
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "id가 폴더와 다름",
            &[("profile.yaml", "id: personal", "id: other")],
            "profile-id-mismatch",
        ),
        (
            "없는 시간대",
            &[("profile.yaml", "Asia/Seoul", "Mars/Base")],
            "bad-timezone",
        ),
        (
            "시간대는 대소문자까지 정확히",
            &[("profile.yaml", "Asia/Seoul", "asia/seoul")],
            "bad-timezone",
        ),
        (
            "선언하지 않은 역할",
            &[(
                "profile.yaml",
                owner,
                "{identity: \"local:owner\", roles: [boss]}",
            )],
            "unknown-role",
        ),
        (
            "같은 멤버 두 번",
            &[("profile.yaml", owner, &format!("{owner}\n  - {owner}"))],
            "duplicate-member",
        ),
        (
            "신원 모양",
            &[(
                "profile.yaml",
                "identity: \"local:owner\"",
                "identity: \"owner\"",
            )],
            "bad-identity",
        ),
        (
            "바인딩이 없는 에이전트",
            &[(
                "profile.yaml",
                "account: owner, agent: main",
                "account: owner, agent: ghost",
            )],
            "unknown-agent",
        ),
        (
            "정책의 없는 에이전트",
            &[("profile.yaml", "main/research:", "main/ghost:")],
            "unknown-agent",
        ),
        (
            "approve 오타",
            &[("profile.yaml", "approve: []", "approve: [calendar.delet]")],
            "unknown-scope",
        ),
        (
            "정책 상한의 모르는 스코프",
            &[(
                "profile.yaml",
                "  scopes: [run, calendar.read, calendar.delete]",
                "  scopes: [run, calendar.read, calendar.delete, nope.scope]",
            )],
            "unknown-scope",
        ),
        (
            "slack 채널 바인딩에 channel 없음",
            &[(
                "profile.yaml",
                "workspace: home, channel: planning,",
                "workspace: home,",
            )],
            "schema",
        ),
        (
            "같은 id 두 바인딩",
            &[("profile.yaml", "{id: inbox,", "{id: desk,")],
            "duplicate-binding-id",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
}

#[test]
fn rejects_tool_declarations() {
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "미등록 서버",
            &[(TOOLS, "server: demo-agenda", "server: gone")],
            "missing-server",
        ),
        (
            "잘못된 class",
            &[(TOOLS, "class: irreversible", "class: dangerous")],
            "schema",
        ),
        (
            "알 수 없는 도구 필드",
            &[(
                TOOLS,
                "egress: [\"calendar.example:443\"]",
                "egress: [\"calendar.example:443\"]\n    bogus: true",
            )],
            "schema",
        ),
        (
            "허용하지 않은 비밀",
            &[("profile.yaml", "secrets: [calendar-reader]", "secrets: []")],
            "unknown-secret",
        ),
        (
            "포트 범위",
            &[(TOOLS, "calendar.example:443", "calendar.example:99999")],
            "bad-egress",
        ),
        (
            "destination 경로가 input에 없음",
            &[(
                TOOLS,
                "scopes: [calendar.delete]\n",
                "scopes: [calendar.delete]\n    destination: {recipient: args.to, content: args.event}\n",
            )],
            "bad-destination",
        ),
        (
            "지원하지 않는 input 키워드",
            &[(
                TOOLS,
                "{type: string, minLength: 1}",
                "{type: string, uniqueItems: true}",
            )],
            "schema",
        ),
        (
            "input에 type: object 없음",
            &[(
                TOOLS,
                "input: {type: object, properties: {}, additionalProperties: false}",
                "input: {properties: {}}",
            )],
            "bad-input-schema",
        ),
        (
            "도구 id 중복",
            &[(TOOLS, "  - id: find", "  - id: list")],
            "duplicate-tool",
        ),
        (
            "scopes 비어 있음",
            &[(
                TOOLS,
                "scopes: [calendar.read]\n    secrets",
                "scopes: []\n    secrets",
            )],
            "schema",
        ),
        (
            "read 도구에 destination",
            &[(
                TOOLS,
                "class: read\n    scopes: [calendar.read]\n    secrets",
                "class: read\n    destination: reply\n    scopes: [calendar.read]\n    secrets",
            )],
            "destination-on-read",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
}

#[test]
fn rejects_aliases_the_host_did_not_register() {
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "허용하지 않은 모델",
            &[("profile.yaml", "model: assistant", "model: huge")],
            "model-not-allowed",
        ),
        (
            "에이전트 모델이 허용 밖",
            &[(AGENT, "오늘", "---\nmodel: huge\n---\n오늘")],
            "model-not-allowed",
        ),
        (
            "없는 CLI 계정",
            &[("profile.yaml", "account: owner,", "account: ghost,")],
            "unknown-account",
        ),
        (
            "없는 Slack 워크스페이스",
            &[(
                "profile.yaml",
                "workspace: home, channel",
                "workspace: away, channel",
            )],
            "unknown-workspace",
        ),
        (
            "없는 Slack 채널",
            &[("profile.yaml", "channel: planning", "channel: nowhere")],
            "unknown-channel",
        ),
        (
            "없는 HTTP 입구",
            &[("profile.yaml", "endpoint: reminders", "endpoint: ghost")],
            "unknown-endpoint",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
    let mut closed = host();
    closed.slack.get_mut("home").unwrap().dm = false;
    let dir = support::Temp::new("dm");
    support::write_profile(dir.path(), "personal", &base());
    let report =
        bot_profile::load(dir.path(), &closed, &bot_profile::Limits::default()).unwrap_err();
    assert!(report.has("dm-not-allowed"), "{report}");
}

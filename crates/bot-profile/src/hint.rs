//! 오류 문구를 도우미로: 비슷한 이름 찾기(did-you-mean), 번들 스키마의 정규식을 사람 말로, YAML 파서의 영어 설명을 고칠 방법으로.
//! 사용자가 «무엇을 어디서 어떻게 고치는지» 알 수 있어야 하므로 예시를 함께 보인다. 파일 내용·값은 싣지 않는다(이름과 허용 목록만).

/// 두 글자열의 편집 거리(글자 단위: 삽입·삭제·바꿈·이웃 글자 맞바꿈).
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

/// `input`과 가장 비슷한 후보(오타일 만큼만: 짧은 글은 한 글자, 길면 두세 글자까지). 같은 글이거나 너무 다르면 `None`.
pub fn nearest<'a>(input: &str, options: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let allowed = match input.chars().count() {
        0..=3 => 1,
        4..=8 => 2,
        _ => 3,
    };
    options
        .into_iter()
        .filter(|option| *option != input)
        .map(|option| (distance(input, option), option))
        // 바뀐 글자가 긴 쪽의 절반보다 적어야 오타로 본다(한 글자짜리 이름끼리는 제안하지 않는다)
        .filter(|(d, option)| {
            *d <= allowed && d * 2 < input.chars().count().max(option.chars().count())
        })
        .min()
        .map(|(_, option)| option)
}

/// 번들 스키마가 쓰는 정규식을 «무엇을 쓸 수 있는지»로. 모르는 정규식(도구 입력 스키마의 `pattern`)은 `None`이다.
pub fn pattern_hint(pattern: &str) -> Option<&'static str> {
    let table: &[(&str, &str)] = &[
        (
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "소문자로 시작하고 소문자·숫자·하이픈(-)만 쓸 수 있어요(하이픈을 연달아 쓰거나 끝에 둘 수 없어요). 예: `daily-report`",
        ),
        (
            "^[a-z0-9]+(?:-[a-z0-9]+)*$",
            "소문자·숫자·하이픈(-)만 쓸 수 있어요(앞뒤 하이픈과 연속 하이픈 `--`은 안 돼요). 예: `pdf-processing`",
        ),
        (
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*(?:/[a-z][a-z0-9]*(?:-[a-z0-9]+)*)*$",
            "에이전트 이름은 소문자 이름(소문자·숫자·하이픈)을 `/`로 이은 경로예요. 예: `main/research`",
        ),
        (
            "^[a-z0-9-]{1,63}(?:/[a-z0-9-]{1,63})*$",
            "에이전트 이름은 소문자 이름(소문자·숫자·하이픈)을 `/`로 이은 경로예요. 예: `main/research`",
        ),
        (
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*\\.[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "`스킬.도구` 형식이어야 해요(둘 다 소문자·숫자·하이픈). 예: `agenda.list`",
        ),
        (
            "^[a-z0-9-]{1,63}\\.[a-z0-9-]{1,63}$",
            "`스킬.도구` 형식이어야 해요(둘 다 소문자·숫자·하이픈). 예: `agenda.list`",
        ),
        (
            "^[A-Za-z0-9_.-]+$",
            "서버가 내세우는 도구 이름이에요: 영문·숫자·밑줄(_)·하이픈(-)·점(.)만 쓸 수 있어요(128자 이하). 예: `get_forecast`",
        ),
        (
            "^[A-Z_][A-Z0-9_]*$",
            "대문자·숫자·밑줄(_)만 쓸 수 있고 숫자로 시작할 수 없어요. 예: `CALENDAR_TOKEN`",
        ),
        (
            "^[A-Za-z_][A-Za-z0-9_]*$",
            "글자·숫자·밑줄(_)만 쓸 수 있고 숫자로 시작할 수 없어요. 예: `AGENDA_VIEW`",
        ),
        (
            "^[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*$",
            "글자·숫자·밑줄로 된 이름이고 `.`으로 하위 항목을 이을 수 있어요(숫자로 시작할 수 없어요). 예: `event.channel`",
        ),
        (
            "^args\\.[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*$",
            "`args.`로 시작하는 인자 경로여야 해요. 예: `args.to`",
        ),
        (
            "^[a-z0-9.-]+:[0-9]{1,5}$",
            "`호스트:포트` 형식이어야 해요(호스트는 소문자·숫자·`.`·`-`, 포트는 숫자). 예: `api.example.com:443`",
        ),
        (
            "^[^\\n\\r]+$",
            "줄바꿈 없이 한 줄이어야 하고 비어 있으면 안 돼요",
        ),
        (
            "^/[^?#]*$",
            "`/`로 시작하는 경로여야 하고 `?`·`#`은 쓸 수 없어요. 예: `/notify`",
        ),
        (
            "^http:[a-z][a-z0-9]*(?:-[a-z0-9]+)*/[A-Za-z0-9._@+\\-]{1,128}$",
            "`http:<입구 별칭>/<주체>` 형식의 HTTP 주체여야 해요(주체는 글자·숫자와 `.`·`_`·`@`·`+`·`-`, 128자까지). 예: `http:openai/build`",
        ),
        (
            "^local:[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "`local:<CLI 계정 별칭>` 형식의 로컬 사람 신원이어야 해요. 예: `local:owner`",
        ),
        (
            "^slack:[a-z][a-z0-9]*(?:-[a-z0-9]+)*/[UW][A-Z0-9]{1,31}$",
            "`slack:<워크스페이스 별칭>/<사용자 ID>` 형식이어야 해요(사용자 ID는 Slack 프로필에서 복사한 `U`나 `W`로 시작하는 대문자·숫자). 예: `slack:home/<사용자 ID>`",
        ),
        (
            "^[ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]*(?:@(?:hourly|daily|midnight|weekly|monthly|yearly|annually)|[^@ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}][^ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]*(?:[ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]+[^ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]+){4})[ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]*$",
            "cron은 '분 시 일 월 요일' 다섯 칸(초 칸 없음) 또는 별칭(@hourly·@daily·@midnight·@weekly·@monthly·@yearly·@annually)이어야 해요. 예: `0 9 * * *`",
        ),
        (
            "^(?:\\.|\\.?[A-Za-z0-9_:#@+\\-]+(?:\\.[A-Za-z0-9_:#@+\\-]+)*\\.?)$",
            "영문·숫자와 `-`·`_`·`.`·`:`·`#`·`@`·`+`만 쓴 정확한 이름이나 ID여야 해요(와일드카드·URL·공백·`..`·자격 값은 쓸 수 없어요). 예: `channel:reports`",
        ),
    ];
    table
        .iter()
        .find(|(known, _)| *known == pattern)
        .map(|(_, text)| *text)
}

/// 한 HTTP 별칭에 웹훅 바인딩과 OpenAI 모델 바인딩이 섞였을 때 고치는 방법.
pub const MIXED_ENDPOINT: &str = "한 별칭은 한 가지 방식만 써요. `model`이 있는 바인딩(OpenAI 호환)과 `model`이 없는 바인딩(웹훅)은 서로 다른 `endpoint` 별칭으로 나누고, 호스트 설정의 별칭 종류(`kind`)도 맞춰 주세요";

/// OpenAI 입구 별칭을 다른 프로필이 이미 쓸 때 고치는 방법.
pub const OPENAI_OWNER: &str = "OpenAI 호환 입구 별칭은 활성 프로필 하나만 소유하고 그 안에서 모델을 여러 개 둘 수 있어요. 다른 프로필은 호스트 설정에 별칭을 따로 만들어 쓰거나, 모델을 한 프로필로 모아 주세요";

/// 같은 입구를 두 바인딩이 쓸 때 고치는 방법. `key`는 `model::entrance`의 정규 문자열(`cli:owner`·`slack:home:channel:planning`·`slack:home:dm`·`http:reminders`),
/// `across`가 참이면 서로 다른 프로필이 겹친 것이다(입구는 활성 프로필 전체에서 한 번만 쓸 수 있다).
pub fn entrance_fix(key: &str, across: bool) -> String {
    if key.starts_with("http:") && key.contains('/') {
        // OpenAI 모델 바인딩(`http:<별칭>/<모델>`): 같은 별칭에서 모델 이름이 겹쳤다
        return "같은 입구 별칭에서 공개 모델 이름은 바인딩 하나에만 연결할 수 있어요. 한쪽 바인딩을 지우거나 `model`을 다른 이름으로 바꿔 주세요".to_owned();
    }
    let (what, field) = if key.starts_with("cli:") {
        ("CLI 계정 별칭", "`account`")
    } else if key.ends_with(":dm") {
        ("Slack DM 입구(워크스페이스마다 하나예요)", "`workspace`")
    } else if key.starts_with("slack:") {
        ("Slack 채널 별칭", "`channel`(또는 `workspace`)")
    } else {
        ("HTTP 입구 별칭", "`endpoint`")
    };
    let mut text = if across {
        format!(
            "{what}은 활성 프로필 전체에서 바인딩 하나에만 연결할 수 있어요. 이 바인딩의 {field}를 다른 별칭으로 바꾸거나 바인딩을 지워 주세요"
        )
    } else {
        format!(
            "{what}은 바인딩 하나에만 연결할 수 있어요. 한쪽 바인딩을 지우거나 그 {field}를 다른 별칭으로 바꿔 주세요"
        )
    };
    if key.starts_with("cli:") && across {
        text.push_str(
            ". 새 별칭은 호스트 설정의 `gateway.cli.accounts`에도 더해야 하고, 같은 OS 사용자에 연결해도 돼요(그때는 `bot chat --profile`로 프로필을 골라요)",
        );
    }
    text
}

/// YAML 파서의 영어 설명을 사용자가 고칠 수 있는 한국어로(대표적인 실수만; 나머지는 원문을 괄호로 덧붙인다).
pub fn yaml_hint(parser: &str) -> String {
    let known: &[(&str, &str)] = &[
        (
            "mapping values are not allowed",
            "여기에는 `키: 값`을 쓸 수 없어요. 앞 줄과 들여쓰기가 맞는지, 한 줄에 `:`가 두 번 있지 않은지 확인하세요(값에 `:`가 들어가면 따옴표로 감싸세요)",
        ),
        (
            "did not find expected key",
            "이 줄의 들여쓰기가 앞 줄과 맞지 않아요. 같은 목록 안의 키는 같은 칸에서 시작해야 해요",
        ),
        (
            "did not find expected '-' indicator",
            "목록의 항목은 모두 `- `로 시작해야 해요(들여쓰기도 맞춰 주세요)",
        ),
        (
            "':' must be followed by",
            "`:` 뒤에는 공백 한 칸이 있어야 해요(`키: 값`). 탭 문자는 쓸 수 없어요",
        ),
        (
            "'-' must be followed by",
            "목록 항목은 `- 값`처럼 `-` 뒤에 공백 한 칸이 있어야 해요",
        ),
        (
            "tab",
            "들여쓰기나 `:` 뒤에 탭 문자를 쓸 수 없어요. 공백으로 바꿔 주세요",
        ),
        (
            "illegal placement of ':'",
            "`:`가 잘못된 자리에 있어요. 닫지 않은 `[`나 `{`가 없는지 확인하고, 값에 `:`가 들어가면 따옴표로 감싸세요",
        ),
        (
            "flow sequence, expected ',' or ']'",
            "`[ … ]` 목록을 `]`로 닫지 않았거나 항목 사이에 `,`가 없어요",
        ),
        (
            "flow mapping, did not find expected ',' or '}'",
            "`{ … }`를 `}`로 닫지 않았거나 항목 사이에 `,`가 없어요",
        ),
        (
            "quoted scalar, found unexpected end of stream",
            "여는 따옴표(`\"` 또는 `'`)를 닫지 않았어요",
        ),
        (
            "unexpected eof",
            "파일이 중간에 끝났어요. 닫지 않은 따옴표나 괄호가 있는지 확인하세요",
        ),
        (
            "simple key expect",
            "`키: 값` 형식이어야 해요. `:`가 빠졌는지 확인하세요",
        ),
        (
            "block sequence entries are not allowed",
            "여기에는 `- 항목` 목록을 쓸 수 없어요. 앞 줄의 `키:` 아래에 들여써서 적어 주세요",
        ),
        (
            "mapping keys are not allowed",
            "여기에는 키를 쓸 수 없어요. 앞 줄과 들여쓰기가 맞는지 확인하세요",
        ),
        (
            "invalid indentation",
            "들여쓰기가 맞지 않아요. 같은 목록·같은 키 아래는 같은 칸 수를 써 주세요",
        ),
        (
            "wrongly indented",
            "들여쓰기가 맞지 않아요. 같은 목록·같은 키 아래는 같은 칸 수를 써 주세요",
        ),
        (
            "misplaced bracket",
            "괄호(`[` `]` `{` `}`)가 잘못된 자리에 있어요",
        ),
        (
            "unknown escape character",
            "큰따옴표 안의 `\\` 뒤 글자가 잘못됐어요. 역슬래시가 필요하면 `\\\\`로 쓰거나 작은따옴표로 감싸세요",
        ),
    ];
    match known
        .iter()
        .find(|(needle, _)| parser.contains(needle))
        .map(|(_, text)| *text)
    {
        Some(text) => text.to_owned(),
        None => format!("YAML 문법이 맞지 않아요(파서 설명: {parser})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typos_get_the_one_close_option_and_unrelated_words_get_none() {
        assert_eq!(nearest("cl", ["cli", "slack", "http"]), Some("cli"));
        assert_eq!(nearest("slak", ["cli", "slack", "http"]), Some("slack"));
        assert_eq!(nearest("htp", ["cli", "slack", "http"]), Some("http"));
        assert_eq!(
            nearest("bindngs", ["bindings", "secrets"]),
            Some("bindings")
        );
        assert_eq!(nearest("secrts", ["bindings", "secrets"]), Some("secrets"));
        assert_eq!(
            nearest("assistent", ["assistant", "fast"]),
            Some("assistant")
        );
        // 이웃한 두 글자를 맞바꾼 오타도 한 번으로 센다
        assert_eq!(nearest("modle", ["model", "roles"]), Some("model"));
        // 너무 다르거나 같은 글이면 제안하지 않는다
        assert_eq!(nearest("x", ["cli", "slack", "http"]), None);
        assert_eq!(
            nearest("z", ["y"]),
            None,
            "한 글자짜리 이름끼리는 오타로 보지 않는다"
        );
        assert_eq!(nearest("ab", ["ac"]), None);
        assert_eq!(nearest("totally-different", ["cli", "slack"]), None);
        assert_eq!(nearest("cli", ["cli"]), None);
        assert_eq!(nearest("", ["cli"]), None);
    }

    #[test]
    fn every_bundled_pattern_has_a_plain_description_with_an_example() {
        let mut seen = std::collections::BTreeSet::new();
        for schema in [
            include_str!("../../../schemas/v1/profile.schema.json"),
            include_str!("../../../schemas/v1/agent.schema.json"),
            include_str!("../../../schemas/v1/skill.schema.json"),
            include_str!("../../../schemas/v1/env.schema.json"),
            include_str!("../../../schemas/v1/tools.schema.json"),
            include_str!("../../../schemas/v1/triggers.schema.json"),
        ] {
            let doc: serde_json::Value = serde_json::from_str(schema).unwrap();
            collect(&doc, &mut seen);
        }
        assert!(seen.len() >= 10, "{seen:?}");
        for pattern in seen {
            let text = pattern_hint(&pattern)
                .unwrap_or_else(|| panic!("패턴에 사람 말 설명이 없어요: {pattern}"));
            assert!(!text.contains('^') && !text.contains("(?:"), "{text}");
            assert!(
                text.contains("예:") || text.contains("한 줄"),
                "예시가 있어야 해요: {text}"
            );
        }
        assert_eq!(
            pattern_hint("^[x-z]+$"),
            None,
            "모르는 정규식은 설명하지 않는다"
        );
    }

    fn collect(value: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(p)) = map.get("pattern") {
                    out.insert(p.clone());
                }
                map.values().for_each(|v| collect(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
            _ => {}
        }
    }

    #[test]
    fn yaml_parser_messages_become_fixes_and_unknown_ones_keep_the_original() {
        let text = yaml_hint("mapping values are not allowed in this context");
        assert!(
            text.contains("키: 값") && text.contains("들여쓰기"),
            "{text}"
        );
        assert!(yaml_hint("':' must be followed by a valid YAML whitespace").contains("공백"));
        assert!(yaml_hint("while parsing a flow sequence, expected ',' or ']'").contains("`]`"));
        assert!(yaml_hint("tab cannot be used as indentation").contains("탭"));
        let unknown = yaml_hint("some brand new parser failure");
        assert!(
            unknown.contains("some brand new parser failure"),
            "{unknown}"
        );
        assert!(unknown.starts_with("YAML 문법이 맞지 않아요"));
        // 영어만 남지 않는다: 알려진 설명은 모두 한국어 문장이다
        for sample in [
            "did not find expected key",
            "illegal placement of ':' indicator",
            "invalid indentation",
            "simple key expect ':'",
            "unexpected eof",
        ] {
            assert!(
                yaml_hint(sample)
                    .chars()
                    .any(|c| ('가'..='힣').contains(&c)),
                "{sample}"
            );
        }
    }
}

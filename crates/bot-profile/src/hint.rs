//! Actionable validation hints: edit-distance suggestions, pattern explanations, entrance conflict fixes, and YAML syntax guidance.

/// Damerau-Levenshtein edit distance in Unicode scalar values.
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

/// Closest candidate within typo distance (1 for len <= 3, 2 for <= 8, 3 otherwise), or `None`.
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
        // Require edits to be strictly less than half the longer string (avoids 1-char false positives).
        .filter(|(d, option)| {
            *d <= allowed && d * 2 < input.chars().count().max(option.chars().count())
        })
        .min()
        .map(|(_, option)| option)
}

/// Plain description for known bundled schema regexes; `None` for custom tool patterns.
pub fn pattern_hint(pattern: &str) -> Option<&'static str> {
    let table: &[(&str, &str)] = &[
        (
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "must start with a lowercase letter and use only lowercase letters, digits, and single hyphens (`-`); e.g. `daily-report`",
        ),
        (
            "^[a-z0-9]+(?:-[a-z0-9]+)*$",
            "must use only lowercase letters, digits, and single hyphens (`-`) (no leading, trailing, or `--`); e.g. `pdf-processing`",
        ),
        (
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*(?:/[a-z][a-z0-9]*(?:-[a-z0-9]+)*)*$",
            "agent path must be lowercase segments (letters, digits, hyphens) separated by `/`; e.g. `main/research`",
        ),
        (
            "^[a-z0-9-]{1,63}(?:/[a-z0-9-]{1,63})*$",
            "agent path must be lowercase segments (letters, digits, hyphens) separated by `/`; e.g. `main/research`",
        ),
        (
            "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*\\.[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "must be `<skill>.<tool>` (both lowercase letters, digits, hyphens); e.g. `agenda.list`",
        ),
        (
            "^[a-z0-9-]{1,63}\\.[a-z0-9-]{1,63}$",
            "must be `<skill>.<tool>` (both lowercase letters, digits, hyphens); e.g. `agenda.list`",
        ),
        (
            "^[A-Za-z0-9_.-]+$",
            "server tool name may only use ASCII letters, digits, `_`, `-`, and `.` (at most 128 chars); e.g. `get_forecast`",
        ),
        (
            "^[A-Z_][A-Z0-9_]*$",
            "must use only uppercase letters, digits, and `_`, and cannot start with a digit; e.g. `CALENDAR_TOKEN`",
        ),
        (
            "^[A-Za-z_][A-Za-z0-9_]*$",
            "must use only letters, digits, and `_`, and cannot start with a digit; e.g. `AGENDA_VIEW`",
        ),
        (
            "^[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*$",
            "dot-separated segments of letters, digits, and `_` (cannot start with a digit); e.g. `event.channel`",
        ),
        (
            "^args\\.[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*$",
            "must be an argument path starting with `args.`; e.g. `args.to`",
        ),
        (
            "^[a-z0-9.-]+:[0-9]{1,5}$",
            "must be `<host>:<port>` (lowercase host with letters, digits, `.`, `-`, and numeric port); e.g. `api.example.com:443`",
        ),
        (
            "^[^\\n\\r]+$",
            "must be a non-empty single line without line breaks",
        ),
        (
            "^/[^?#]*$",
            "must be a path starting with `/` without `?` or `#`; e.g. `/notify`",
        ),
        (
            "^http:[a-z][a-z0-9]*(?:-[a-z0-9]+)*/[A-Za-z0-9._@+\\-]{1,128}$",
            "must be an HTTP principal `http:<endpoint-alias>/<subject>` (subject: letters, digits, `.`, `_`, `@`, `+`, `-`, up to 128 chars); e.g. `http:openai/build`",
        ),
        (
            "^local:[a-z][a-z0-9]*(?:-[a-z0-9]+)*$",
            "must be a local human identity `local:<cli-account-alias>`; e.g. `local:owner`",
        ),
        (
            "^slack:[a-z][a-z0-9]*(?:-[a-z0-9]+)*/[UW][A-Z0-9]{1,31}$",
            "must be `slack:<workspace-alias>/<user-id>` (Slack user ID starting with `U` or `W`); e.g. `slack:home/U01234567`",
        ),
        (
            "^[ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]*(?:@(?:hourly|daily|midnight|weekly|monthly|yearly|annually)|[^@ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}][^ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]*(?:[ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]+[^ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]+){4})[ \\t\\r\\n\u{000b}\u{000c}\u{0085}\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}]*$",
            "cron must have 5 fields ('min hour dom month dow', no seconds) or a macro (@hourly, @daily, @midnight, @weekly, @monthly, @yearly, @annually); e.g. `0 9 * * *`",
        ),
        (
            "^(?:\\.|\\.?[A-Za-z0-9_:#@+\\-]+(?:\\.[A-Za-z0-9_:#@+\\-]+)*\\.?)$",
            "must be an exact name or ID using ASCII letters, digits, `-`, `_`, `.`, `:`, `#`, `@`, `+` (no wildcards, URLs, whitespace, `..`, or credentials); e.g. `channel:reports`",
        ),
    ];
    table
        .iter()
        .find(|(known, _)| *known == pattern)
        .map(|(_, text)| *text)
}

pub const MIXED_ENDPOINT: &str = "An HTTP alias can only serve one mode. Split bindings with `model` (OpenAI-compatible) and bindings without `model` (webhook) onto separate `endpoint` aliases, and match each alias `kind` in the host config.";

pub const OPENAI_OWNER: &str = "An OpenAI-compatible endpoint alias can only be owned by one active profile, which may expose multiple models on it. Create a separate alias in the host config for another profile, or consolidate models into one profile.";

/// Remediation guidance when two bindings share an entrance (`across` indicates cross-profile conflict).
pub fn entrance_fix(key: &str, across: bool) -> String {
    if key.starts_with("http:") && key.contains('/') {
        return "A public model name on the same endpoint alias can only be bound once. Remove one binding or change its `model` name.".to_owned();
    }
    let (what, field) = if key.starts_with("cli:") {
        ("CLI account alias", "`account`")
    } else if key.ends_with(":dm") {
        ("Slack DM entrance (one per workspace)", "`workspace`")
    } else if key.starts_with("slack:") {
        ("Slack channel alias", "`channel` (or `workspace`)")
    } else {
        ("HTTP endpoint alias", "`endpoint`")
    };
    let mut text = if across {
        format!(
            "{what} can only be bound once across all active profiles. Change {field} to another alias or remove this binding."
        )
    } else {
        format!(
            "{what} can only be bound once. Remove one binding or change its {field} to another alias."
        )
    };
    if key.starts_with("cli:") && across {
        text.push_str(
            " Also add the new alias under `gateway.cli.accounts` in the host config (it may map to the same OS user; select profiles with `bot chat --profile`).",
        );
    }
    text
}

/// Maps common YAML parser diagnostics to actionable fix hints, preserving unknown errors verbatim.
pub fn yaml_hint(parser: &str) -> String {
    let known: &[(&str, &str)] = &[
        (
            "mapping values are not allowed",
            "`key: value` is not allowed here; check indentation against the previous line and quote values containing `:`",
        ),
        (
            "did not find expected key",
            "indentation does not match the previous line; sibling keys must start in the same column",
        ),
        (
            "did not find expected '-' indicator",
            "every list item must start with `- ` at the same indentation",
        ),
        (
            "':' must be followed by",
            "put a space after `:` (`key: value`); tab characters are not allowed",
        ),
        (
            "'-' must be followed by",
            "put a space after `-` in list items (`- value`)",
        ),
        (
            "tab",
            "tab characters cannot be used for indentation or after `:`; replace tabs with spaces",
        ),
        (
            "illegal placement of ':'",
            "misplaced `:`; check for unclosed `[` or `{` and quote values containing `:`",
        ),
        (
            "flow sequence, expected ',' or ']'",
            "missing `,` between items or unclosed `]` in `[ ... ]` list",
        ),
        (
            "flow mapping, did not find expected ',' or '}'",
            "missing `,` between entries or unclosed `}` in `{ ... }` mapping",
        ),
        (
            "quoted scalar, found unexpected end of stream",
            "unclosed quote (`\"` or `'`)",
        ),
        (
            "unexpected eof",
            "unexpected end of file; check for unclosed quotes or brackets",
        ),
        (
            "simple key expect",
            "expected `key: value`; check for a missing `:`",
        ),
        (
            "block sequence entries are not allowed",
            "`- item` list is not allowed here; indent it under the parent `key:`",
        ),
        (
            "mapping keys are not allowed",
            "mapping key is not allowed here; check indentation against the previous line",
        ),
        (
            "invalid indentation",
            "invalid indentation; align entries under the same list or key to the same column",
        ),
        (
            "wrongly indented",
            "invalid indentation; align entries under the same list or key to the same column",
        ),
        (
            "misplaced bracket",
            "misplaced bracket (`[`, `]`, `{`, or `}`)",
        ),
        (
            "unknown escape character",
            "invalid `\\` escape in double quotes; use `\\\\` for a literal backslash or use single quotes",
        ),
    ];
    match known
        .iter()
        .find(|(needle, _)| parser.contains(needle))
        .map(|(_, text)| *text)
    {
        Some(text) => text.to_owned(),
        None => format!("Invalid YAML syntax (parser: {parser})"),
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
        assert_eq!(nearest("modle", ["model", "roles"]), Some("model"));
        assert_eq!(nearest("x", ["cli", "slack", "http"]), None);
        assert_eq!(
            nearest("z", ["y"]),
            None,
            "single-character names must not match each other as typos"
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
                .unwrap_or_else(|| panic!("missing plain description for pattern: {pattern}"));
            assert!(!text.contains('^') && !text.contains("(?:"), "{text}");
            assert!(
                text.contains("e.g.") || text.contains("single line"),
                "description must include an example: {text}"
            );
        }
        assert_eq!(
            pattern_hint("^[x-z]+$"),
            None,
            "unknown custom regex must not return a hint"
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
            text.contains("key: value") && text.contains("indentation"),
            "{text}"
        );
        assert!(yaml_hint("':' must be followed by a valid YAML whitespace").contains("space"));
        assert!(yaml_hint("while parsing a flow sequence, expected ',' or ']'").contains("`]`"));
        assert!(yaml_hint("tab cannot be used as indentation").contains("tab"));
        let unknown = yaml_hint("some brand new parser failure");
        assert!(
            unknown.contains("some brand new parser failure"),
            "{unknown}"
        );
        assert!(unknown.starts_with("Invalid YAML syntax"));
        for sample in [
            "did not find expected key",
            "illegal placement of ':' indicator",
            "invalid indentation",
            "simple key expect ':'",
            "unexpected eof",
        ] {
            assert!(
                !yaml_hint(sample).starts_with("Invalid YAML syntax"),
                "{sample}"
            );
        }
    }
}

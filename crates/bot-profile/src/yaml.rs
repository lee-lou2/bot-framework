//! 안전한 YAML → JSON 값. 파서 이벤트를 직접 받아 만들면서 위험한 구성을 그 자리에서 거절한다.
//!
//! 거절: 앵커·별칭·태그·다중 문서·중복 키·복합 키·한도 초과(깊이·노드·문자열)·무한대/NaN.
//! 평문 스칼라는 YAML 1.2 코어 스키마(`true/false/null`, 10진·16진·8진 정수, 실수)로 풀고,
//! `yes/no/on/off`는 문자열로 둔다. 매핑 키는 스타일과 무관하게 글자 그대로 문자열이다.
use crate::{hint, issue::Issue, limits::Limits, text};
use saphyr_parser::{Event, Parser, ScalarStyle, Span};
use serde_json::{Map, Number, Value};

enum Frame {
    Seq(Vec<Value>),
    Map {
        map: Map<String, Value>,
        pending: Option<String>,
    },
}

struct Builder<'a> {
    path: &'a str,
    limits: &'a Limits,
    stack: Vec<Frame>,
    root: Option<Value>,
    docs: usize,
    nodes: usize,
}

pub fn parse(path: &str, text: &str, limits: &Limits) -> Result<Value, Issue> {
    let mut builder = Builder {
        path,
        limits,
        stack: Vec::new(),
        root: None,
        docs: 0,
        nodes: 0,
    };
    for item in Parser::new_from_str(text) {
        match item {
            Ok((event, span)) => builder.event(event, span)?,
            Err(e) => {
                // 파서의 영어 설명은 고칠 방법으로 바꾸고, 줄과 칸을 함께 알린다(파일 내용은 싣지 않는다)
                let marker = e.marker();
                return Err(Issue::error("yaml-syntax", path, hint::yaml_hint(e.info()))
                    .at(format!("줄 {}, 칸 {}", marker.line(), marker.col() + 1)));
            }
        }
    }
    builder
        .root
        .ok_or_else(|| Issue::error("yaml-empty", path, "내용이 없어요"))
}

impl Builder<'_> {
    fn fail(&self, code: &'static str, span: Span, message: &str) -> Issue {
        Issue::error(code, self.path, message).at(format!("줄 {}", span.start.line()))
    }

    fn event(&mut self, event: Event<'_>, span: Span) -> Result<(), Issue> {
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
            Event::DocumentStart(_) => {
                self.docs += 1;
                if self.docs > 1 {
                    return Err(self.fail("yaml-multi-doc", span, "문서가 둘 이상이에요(`---`)"));
                }
            }
            Event::Alias(_) => {
                return Err(self.fail("yaml-alias", span, "별칭(`*이름`)은 쓸 수 없어요"));
            }
            Event::Scalar(text, style, anchor, tag) => {
                self.guard(anchor, tag.is_some(), span)?;
                self.node(span)?;
                if text.len() > self.limits.max_yaml_scalar {
                    return Err(self.fail("yaml-too-large", span, "값 하나가 너무 길어요"));
                }
                if text::has_forbidden(&text) {
                    return Err(self.fail(
                        "yaml-control",
                        span,
                        "이스케이프로 만든 제어·보이지 않는 문자는 쓸 수 없어요",
                    ));
                }
                self.scalar(&text, style, span)?;
            }
            Event::SequenceStart(anchor, tag) => {
                self.guard(anchor, tag.is_some(), span)?;
                self.open(Frame::Seq(Vec::new()), span)?;
            }
            Event::MappingStart(anchor, tag) => {
                self.guard(anchor, tag.is_some(), span)?;
                let frame = Frame::Map {
                    map: Map::new(),
                    pending: None,
                };
                self.open(frame, span)?;
            }
            Event::SequenceEnd => {
                if let Some(Frame::Seq(items)) = self.stack.pop() {
                    self.place(Value::Array(items));
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map { map, .. }) = self.stack.pop() {
                    self.place(Value::Object(map));
                }
            }
        }
        Ok(())
    }

    fn guard(&self, anchor: usize, tagged: bool, span: Span) -> Result<(), Issue> {
        if anchor != 0 {
            return Err(self.fail("yaml-anchor", span, "앵커(`&이름`)는 쓸 수 없어요"));
        }
        if tagged {
            return Err(self.fail("yaml-tag", span, "태그(`!`)는 쓸 수 없어요"));
        }
        Ok(())
    }

    fn node(&mut self, span: Span) -> Result<(), Issue> {
        self.nodes += 1;
        if self.nodes > self.limits.max_yaml_nodes {
            return Err(self.fail("yaml-too-large", span, "항목이 너무 많아요"));
        }
        Ok(())
    }

    fn open(&mut self, frame: Frame, span: Span) -> Result<(), Issue> {
        self.node(span)?;
        if let Some(Frame::Map { pending: None, .. }) = self.stack.last() {
            return Err(self.fail("yaml-complex-key", span, "매핑 키는 문자열이어야 해요"));
        }
        if self.stack.len() >= self.limits.max_yaml_depth {
            return Err(self.fail("yaml-too-deep", span, "중첩이 너무 깊어요"));
        }
        self.stack.push(frame);
        Ok(())
    }

    fn scalar(&mut self, text: &str, style: ScalarStyle, span: Span) -> Result<(), Issue> {
        if let Some(Frame::Map { map, pending }) = self.stack.last_mut()
            && pending.is_none()
        {
            if map.contains_key(text) {
                let message = format!("같은 키가 두 번 나와요: {text}");
                return Err(self.fail("yaml-duplicate-key", span, &message));
            }
            *pending = Some(text.to_owned());
            return Ok(());
        }
        let value = if style == ScalarStyle::Plain {
            resolve_plain(text).map_err(|why| self.fail("yaml-number", span, why))?
        } else {
            Value::String(text.to_owned())
        };
        self.place(value);
        Ok(())
    }

    fn place(&mut self, value: Value) {
        match self.stack.last_mut() {
            None => self.root = Some(value),
            Some(Frame::Seq(items)) => items.push(value),
            Some(Frame::Map { map, pending }) => {
                if let Some(key) = pending.take() {
                    map.insert(key, value);
                }
            }
        }
    }
}

/// YAML 1.2 코어 스키마의 평문 스칼라.
fn resolve_plain(s: &str) -> Result<Value, &'static str> {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => return Err("무한대·NaN은 쓸 수 없어요"),
        _ => {}
    }
    if let Some(hex) = s.strip_prefix("0x")
        && !hex.is_empty()
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return int(i128::from_str_radix(hex, 16));
    }
    if let Some(oct) = s.strip_prefix("0o")
        && !oct.is_empty()
        && oct.bytes().all(|b| (b'0'..=b'7').contains(&b))
    {
        return int(i128::from_str_radix(oct, 8));
    }
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if !unsigned.is_empty() && unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return int(s.strip_prefix('+').unwrap_or(s).parse::<i128>());
    }
    if is_float(unsigned) {
        let n: f64 = s.parse().map_err(|_| "실수를 읽을 수 없어요")?;
        return Number::from_f64(n)
            .map(Value::Number)
            .ok_or("무한대·NaN은 쓸 수 없어요");
    }
    Ok(Value::String(s.to_owned()))
}

fn int(parsed: Result<i128, std::num::ParseIntError>) -> Result<Value, &'static str> {
    let n = parsed.map_err(|_| "정수 범위를 넘어요")?;
    if let Ok(v) = i64::try_from(n) {
        Ok(Value::Number(v.into()))
    } else if let Ok(v) = u64::try_from(n) {
        Ok(Value::Number(v.into()))
    } else {
        Err("정수 범위를 넘어요")
    }
}

/// `(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?` 이고 `.`이나 지수가 있는 것
fn is_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    let int_part = digits(&mut i);
    let mut seen_mark = false;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        seen_mark = true;
        let frac = digits(&mut i);
        if !int_part && !frac {
            return false;
        }
    } else if !int_part {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        seen_mark = true;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    seen_mark && i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(text: &str) -> Value {
        parse("t.yaml", text, &Limits::default()).unwrap()
    }

    fn code(text: &str) -> &'static str {
        parse("t.yaml", text, &Limits::default()).unwrap_err().code
    }

    #[test]
    fn builds_json_with_core_schema_scalars() {
        let v = ok(
            "a: 1\nb: [true, null, ~, 'x', \"y\", no, on, yes]\nc: {d: 1.5, e: -3, f: 0x1F, g: 1e3}\nh:\n",
        );
        assert_eq!(
            v,
            json!({"a":1,"b":[true,null,null,"x","y","no","on","yes"],
                   "c":{"d":1.5,"e":-3,"f":31,"g":1000.0},"h":null})
        );
        assert_eq!(ok("\"on\": 1\nnull: 2\n"), json!({"on":1,"null":2}));
        assert_eq!(ok("t: |\n  a\n  b\n")["t"], "a\nb\n");
        assert_eq!(
            ok("v: 007\nw: 1.2.3\nx: 1_000\n"),
            json!({"v":7,"w":"1.2.3","x":"1_000"})
        );
    }

    #[test]
    fn rejects_anchors_aliases_tags_and_merge_expansion() {
        assert_eq!(code("a: &x 1\n"), "yaml-anchor");
        assert_eq!(code("a: &x 1\nb: *x\n"), "yaml-anchor");
        assert_eq!(code("a: *x\n"), "yaml-syntax");
        assert_eq!(code("a: !!str 1\n"), "yaml-tag");
        assert_eq!(code("a: !custom {b: 1}\n"), "yaml-tag");
        assert_eq!(code("a: &m {b: 1}\nc:\n  <<: *m\n"), "yaml-anchor");
        // 병합 키 자체는 평범한 키일 뿐이다
        assert_eq!(ok("<<: 1\n"), json!({"<<": 1}));
    }

    #[test]
    fn rejects_multi_document_duplicates_and_complex_keys() {
        assert_eq!(code("a: 1\n---\nb: 2\n"), "yaml-multi-doc");
        assert_eq!(code("a: 1\na: 2\n"), "yaml-duplicate-key");
        assert_eq!(code("{a: 1, a: 2}\n"), "yaml-duplicate-key");
        assert_eq!(code("? [1, 2]\n: x\n"), "yaml-complex-key");
        assert_eq!(code("? {a: 1}\n: x\n"), "yaml-complex-key");
        assert_eq!(code(""), "yaml-empty");
        assert_eq!(code("# only a comment\n"), "yaml-empty");
        let one = parse("t.yaml", "---\na: 1\n", &Limits::default()).unwrap();
        assert_eq!(one, json!({"a": 1}));
    }

    #[test]
    fn enforces_limits() {
        let deep = Limits {
            max_yaml_depth: 3,
            ..Limits::default()
        };
        let nested = "[[[[1]]]]";
        assert_eq!(parse("t", nested, &deep).unwrap_err().code, "yaml-too-deep");
        let nodes = Limits {
            max_yaml_nodes: 5,
            ..Limits::default()
        };
        assert_eq!(
            parse("t", "[1,2,3,4,5,6]", &nodes).unwrap_err().code,
            "yaml-too-large"
        );
        let scalar = Limits {
            max_yaml_scalar: 4,
            ..Limits::default()
        };
        assert_eq!(
            parse("t", "a: abcdef", &scalar).unwrap_err().code,
            "yaml-too-large"
        );
        let bomb = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_ne!(parse("t", &bomb, &Limits::default()).unwrap_err().code, "");
    }

    #[test]
    fn rejects_control_characters_made_by_escapes() {
        assert_eq!(code("a: \"x\\u0000y\"\n"), "yaml-control");
        assert_eq!(code("a: \"x\\u202ey\"\n"), "yaml-control");
        assert_eq!(ok("a: \"줄\\n바꿈\\t탭\"\n"), json!({"a": "줄\n바꿈\t탭"}));
    }

    #[test]
    fn rejects_unrepresentable_numbers() {
        assert_eq!(code("a: .inf\n"), "yaml-number");
        assert_eq!(code("a: .NaN\n"), "yaml-number");
        assert_eq!(
            code("a: 99999999999999999999999999999999999999999\n"),
            "yaml-number"
        );
        assert_eq!(
            ok("a: 18446744073709551615\n"),
            json!({"a": 18446744073709551615u64})
        );
    }

    #[test]
    fn reports_syntax_errors_with_line() {
        let e = parse("t.yaml", "a: 1\nb: [1, 2\nc: 3\n", &Limits::default()).unwrap_err();
        assert_eq!(e.code, "yaml-syntax");
        assert!(e.at.starts_with("줄 "), "{e}");
    }

    #[test]
    fn random_input_never_panics_and_always_ends() {
        use crate::testrand::{Rng, yaml_soup};
        let limits = Limits::default();
        let mut rng = Rng::new(0x5eed_1234);
        let (mut ok, mut err) = (0, 0);
        for _ in 0..30_000 {
            let text = yaml_soup(&mut rng);
            let outcome = std::panic::catch_unwind(|| parse("fuzz.yaml", &text, &limits));
            match outcome {
                Ok(Ok(_)) => ok += 1,
                Ok(Err(_)) => err += 1,
                Err(_) => panic!("패닉: {text:?}"),
            }
        }
        assert!(
            ok > 100 && err > 100,
            "다양한 입력이 섞여야 한다: ok {ok}, err {err}"
        );
    }
}

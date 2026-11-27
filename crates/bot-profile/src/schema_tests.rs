use super::*;
use serde_json::json;

fn schema(doc: Value) -> Schema {
    Schema::compile_bundled(doc).unwrap()
}

fn messages(s: &Schema, v: Value) -> Vec<String> {
    s.validate(&v)
        .into_iter()
        .map(|x| format!("{} {}", x.pointer, x.message))
        .collect()
}

#[test]
fn objects_required_additional_and_names() {
    let s = schema(json!({
        "type": "object",
        "properties": {"a": {"type": "string", "pattern": "^[a-z]+$"}, "n": {"type": "integer", "minimum": 1}},
        "required": ["a"], "additionalProperties": false
    }));
    assert!(messages(&s, json!({"a": "x", "n": 2})).is_empty());
    let m = messages(&s, json!({"n": 0, "z": 1}));
    assert!(m.iter().any(|x| x.contains("필수 필드 `a`")), "{m:?}");
    assert!(
        m.iter()
            .any(|x| x.starts_with("/z ") && x.contains("알 수 없는 필드")),
        "{m:?}"
    );
    assert!(
        m.iter().any(|x| x.starts_with("/n ") && x.contains("이상")),
        "{m:?}"
    );
    assert!(!messages(&s, json!({"a": "UP"})).is_empty());
    assert!(messages(&s, json!({"a": "x", "n": 1.0})).is_empty());
    assert!(!messages(&s, json!({"a": "x", "n": 1.5})).is_empty());
}

#[test]
fn arrays_unique_and_property_names() {
    let s = schema(
        json!({"type": "object", "propertyNames": {"pattern": "^[A-Z_]+$"}, "additionalProperties": {"type": "string"}}),
    );
    assert!(messages(&s, json!({"A_B": "x"})).is_empty());
    assert!(!messages(&s, json!({"a": "x"})).is_empty());
    assert!(!messages(&s, json!({"A": 1})).is_empty());
    let u = schema(
        json!({"type": "array", "items": {"type": "string"}, "uniqueItems": true, "maxItems": 2}),
    );
    assert!(messages(&u, json!(["a", "b"])).is_empty());
    assert_eq!(messages(&u, json!(["a", "a", "c"])).len(), 2);
}

#[test]
fn combinators_pick_the_closest_branch() {
    let s = schema(json!({"oneOf": [
        {"type": "object", "properties": {"k": {"const": "a"}, "x": {"type": "string"}}, "required": ["k", "x"], "additionalProperties": false},
        {"type": "object", "properties": {"k": {"const": "b"}, "y": {"type": "string"}}, "required": ["k", "y"], "additionalProperties": false}
    ]}));
    assert!(messages(&s, json!({"k": "b", "y": "1"})).is_empty());
    let m = messages(&s, json!({"k": "b", "z": "1"}));
    assert!(m.iter().any(|x| x.contains("필수 필드 `y`")), "{m:?}");
    assert!(m.iter().all(|x| !x.contains("필수 필드 `x`")), "{m:?}");
    let both = schema(json!({"oneOf": [{"type": "string"}, {"type": "string", "minLength": 1}]}));
    assert!(
        messages(&both, json!("x"))
            .iter()
            .any(|x| x.contains("동시에"))
    );
}

#[test]
fn if_then_else_and_not() {
    let s = schema(json!({
        "if": {"properties": {"on": {"required": ["cron"]}}},
        "then": {"required": ["principal"]},
        "else": {"not": {"anyOf": [{"required": ["principal"]}, {"required": ["approver"]}]}}
    }));
    assert!(messages(&s, json!({"on": {"cron": "x"}, "principal": "a"})).is_empty());
    assert!(!messages(&s, json!({"on": {"cron": "x"}})).is_empty());
    assert!(messages(&s, json!({"on": {"event": "m"}})).is_empty());
    assert!(!messages(&s, json!({"on": {"event": "m"}, "approver": "a"})).is_empty());
}

#[test]
fn refs_resolve_inside_the_document_and_cycles_stop() {
    let s = schema(
        json!({"$defs": {"n": {"type": "string"}}, "properties": {"a": {"$ref": "#/$defs/n"}}}),
    );
    assert!(messages(&s, json!({"a": "x"})).is_empty());
    assert!(!messages(&s, json!({"a": 1})).is_empty());
    let cyc = schema(json!({"$ref": "#"}));
    assert!(
        messages(&cyc, json!(1))
            .iter()
            .any(|m| m.contains("너무 깊어요"))
    );
}

#[test]
fn input_dialect_rejects_refs_and_unknown_keywords() {
    for doc in [
        json!({"$ref": "https://example.test/x.json"}),
        json!({"type": "object", "$ref": "#"}),
        json!({"patternProperties": {}}),
    ] {
        assert!(Schema::compile_input(doc.clone()).is_err(), "{doc}");
    }
    assert!(Schema::compile_bundled(json!({"$ref": "https://example.test/x.json"})).is_err());
    assert!(Schema::compile_bundled(json!({"$ref": META_REF})).is_ok());
}

#[test]
fn meta_ref_checks_the_instance_is_a_supported_input_schema() {
    let s = Schema::compile_bundled(json!({"type": "object", "$ref": META_REF})).unwrap();
    assert!(messages(&s, json!({"type": "object"})).is_empty());
    assert!(!messages(&s, json!({"type": "nope"})).is_empty());
    assert!(!messages(&s, json!({"type": "object", "$ref": "#"})).is_empty());
    assert!(!messages(&s, json!({"type": "object", "uniqueItems": true})).is_empty());
    assert!(!messages(&s, json!("text")).is_empty());
}

#[test]
fn exponential_schemas_are_cut_off_with_a_violation() {
    // 문서는 작지만 같은 정의를 둘씩 가리키는 anyOf를 30겹 쌓아 평가 경로가 2^30개다
    let mut defs = serde_json::Map::new();
    defs.insert("a0".into(), json!({"type": "string"}));
    for i in 1..=30 {
        let prev = json!({"$ref": format!("#/$defs/a{}", i - 1)});
        defs.insert(format!("a{i}"), json!({"anyOf": [prev.clone(), prev]}));
    }
    let s = Schema::compile_bundled(json!({"$defs": defs, "$ref": "#/$defs/a30"})).unwrap();
    let found = s.validate(&json!(1));
    assert!(
        found.iter().any(|v| v.message.contains("너무 복잡해요")),
        "{:?}",
        found.len()
    );
}

#[test]
fn pointer_escaping() {
    let s = schema(json!({"properties": {"a/b": {"type": "string"}}}));
    assert_eq!(
        messages(&s, json!({"a/b": 1}))[0]
            .split(' ')
            .next()
            .unwrap(),
        "/a~1b"
    );
}

fn union() -> Schema {
    schema(json!({"oneOf": [
        {"type": "object", "properties": {"id": {"type": "string"}, "adapter": {"const": "cli"}, "account": {"type": "string"}}, "required": ["id", "adapter", "account"], "additionalProperties": false},
        {"type": "object", "properties": {"id": {"type": "string"}, "adapter": {"const": "slack"}, "channel": {"type": "string"}}, "required": ["id", "adapter", "channel"], "additionalProperties": false},
        {"type": "object", "properties": {"id": {"type": "string"}, "adapter": {"const": "slack"}, "dm": {"type": "boolean"}}, "required": ["id", "adapter", "dm"], "additionalProperties": false},
        {"type": "object", "properties": {"id": {"type": "string"}, "adapter": {"const": "http"}, "endpoint": {"type": "string"}}, "required": ["id", "adapter", "endpoint"], "additionalProperties": false}
    ]}))
}

#[test]
fn a_tag_value_that_matches_no_variant_lists_every_allowed_value_with_the_closest_one() {
    let s = union();
    let found = s.validate(&json!({"id": "a", "adapter": "cl", "account": "x"}));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].pointer, "/adapter");
    let text = &found[0].message;
    // 허용 값 전부(겹치는 slack은 한 번), 받은 값, 비슷한 값
    assert!(text.contains(r#""cli", "slack", "http""#), "{text}");
    assert!(text.contains(r#"받은 값: "cl""#), "{text}");
    assert!(text.contains(r#"혹시 "cli""#), "{text}");
    // 비슷한 값이 없으면 제안하지 않는다
    let text = &s.validate(&json!({"id": "a", "adapter": "carrier-pigeon"}))[0].message;
    assert!(
        text.contains(r#""cli", "slack", "http""#) && !text.contains("혹시"),
        "{text}"
    );
    // 문자열이 아닌 값도 허용 값을 알려 준다
    let found = s.validate(&json!({"id": "a", "adapter": 7}));
    assert!(found[0].message.contains("받은 값: 7"), "{found:?}");
    // 정해 둔 가지의 값이면 그 가지의 위반만 보인다(다른 가지의 필수 필드를 늘어놓지 않는다)
    let m = messages(&s, json!({"id": "a", "adapter": "http"}));
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0].contains("필수 필드 `endpoint`"), "{m:?}");
    // slack은 두 가지 중 가까운 쪽(필드가 하나 모자란 쪽)의 위반을 보인다
    let m = messages(&s, json!({"id": "a", "adapter": "slack"}));
    assert!(
        m.iter()
            .all(|x| !x.contains("account") && !x.contains("endpoint")),
        "{m:?}"
    );
    // 값이 맞는 가지가 아닌 다른 가지가 더 가까워 보여도(필드가 우연히 맞는 경우) 값이 맞는 가지의 위반을 보인다
    let m = messages(&s, json!({"id": "a", "adapter": "slack", "account": "x"}));
    assert!(m.iter().all(|x| !x.contains(r#""cli""#)), "{m:?}");
    assert!(
        m.iter()
            .any(|x| x.contains("필수 필드 `channel`") || x.contains("필수 필드 `dm`")),
        "{m:?}"
    );
    // 태그 필드가 없으면 예전처럼 가장 가까운 가지의 위반이다
    let m = messages(&s, json!({"id": "a", "account": "x"}));
    assert!(m.iter().any(|x| x.contains("필수 필드 `adapter`")), "{m:?}");
    // 맞는 값은 그대로 통과한다
    assert!(messages(&s, json!({"id": "a", "adapter": "cli", "account": "x"})).is_empty());
    assert!(messages(&s, json!({"id": "a", "adapter": "slack", "dm": true})).is_empty());
}

fn closed() -> Schema {
    schema(json!({
        "type": "object",
        "properties": {"bindings": {"type": "array"}, "secrets": {"type": "array"}, "model": {"type": "string"}},
        "required": ["bindings"], "additionalProperties": false
    }))
}

#[test]
fn a_misspelled_field_is_one_line_that_names_the_field_it_was_meant_to_be() {
    let s = closed();
    // 필수 필드의 오타: «없어요»와 «알 수 없어요» 두 줄이 아니라 한 줄
    let found = s.validate(&json!({"bindngs": []}));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].pointer, "/bindngs");
    assert!(
        found[0].message.contains("`bindngs`") && found[0].message.contains("`bindings`를 잘못 쓴"),
        "{found:?}"
    );
    // 선택 필드의 오타도 같다
    let found = s.validate(&json!({"bindings": [], "secrts": []}));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].message.contains("`secrets`를 잘못 쓴"),
        "{found:?}"
    );
    // 맞는 필드가 이미 있으면 오타로 보지 않는다(제안하지 않는다)
    let found = s.validate(&json!({"bindings": [], "modle": "x", "model": "y"}));
    assert_eq!(found.len(), 1);
    assert!(!found[0].message.contains("잘못 쓴"), "{found:?}");
    // 오타가 아닌 알 수 없는 필드는 쓸 수 있는 필드를 알려 주고, 필수 필드 누락은 따로 말한다
    let m = messages(&s, json!({"extra": 1}));
    assert!(
        m.iter().any(|x| x.contains("필수 필드 `bindings`")),
        "{m:?}"
    );
    let unknown = m.iter().find(|x| x.starts_with("/extra ")).unwrap();
    assert!(
        unknown.contains("쓸 수 있는 필드: bindings, model, secrets"),
        "{unknown}"
    );
    // 필드가 많은 객체는 목록을 싣지 않는다
    let wide: serde_json::Map<String, Value> = (0..20)
        .map(|i| (format!("field{i}"), json!({"type": "string"})))
        .collect();
    let s = schema(json!({"type": "object", "properties": wide, "additionalProperties": false}));
    let found = s.validate(&json!({"zzz": 1}));
    assert!(!found[0].message.contains("쓸 수 있는 필드"), "{found:?}");
}

#[test]
fn bundled_patterns_are_explained_in_plain_words_and_other_patterns_keep_their_regex() {
    let env = schema(
        json!({"type": "object", "properties": {"token": {"type": "string", "pattern": "^[A-Z_][A-Z0-9_]*$"}}}),
    );
    let found = env.validate(&json!({"token": "calendar_token"}));
    assert_eq!(found.len(), 1);
    let text = &found[0].message;
    assert!(
        text.contains("대문자·숫자·밑줄") && text.contains("예: `CALENDAR_TOKEN`"),
        "{text}"
    );
    assert!(
        !text.contains("^[A-Z_]") && !text.contains("패턴"),
        "정규식 원문이 없다: {text}"
    );
    // 도구가 스스로 정한 패턴은 사람 말이 없으니 원문을 그대로 보인다
    let custom = schema(json!({"type": "string", "pattern": "^[x-z]+$"}));
    assert!(
        custom.validate(&json!("abc"))[0]
            .message
            .contains("(패턴 ^[x-z]+$)")
    );
    // 같은 자리의 같은 문구는 한 번만 보인다(두 패턴이 같은 규칙을 말하는 경우)
    let twice = schema(json!({
        "type": "string",
        "pattern": "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*(?:/[a-z][a-z0-9]*(?:-[a-z0-9]+)*)*$",
        "allOf": [{"pattern": "^[a-z0-9-]{1,63}(?:/[a-z0-9-]{1,63})*$"}]
    }));
    let found = twice.validate(&json!("Main"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].message.contains("예: `main/research`"),
        "{found:?}"
    );
}

#[test]
fn json_schema_equality_treats_integer_and_float_values_and_object_keys_semantically() {
    // const·enum: 1 == 1.0, 객체 키 순서와 숫자 표기를 의미로 비교한다
    let c = schema(json!({"const": 1}));
    assert!(messages(&c, json!(1)).is_empty());
    assert!(messages(&c, json!(1.0)).is_empty());
    assert!(!messages(&c, json!(1.5)).is_empty());

    let e = schema(json!({"enum": [1, {"a": 1, "b": [2]}]}));
    assert!(messages(&e, json!(1.0)).is_empty());
    assert!(messages(&e, json!({"b": [2.0], "a": 1.0})).is_empty());
    assert!(!messages(&e, json!({"a": 2, "b": [2]})).is_empty());

    // uniqueItems: 1과 1.0, 그리고 내부 숫자 표기가 다른 같은 객체도 중복으로 잡는다
    let u = schema(json!({"type": "array", "uniqueItems": true}));
    assert!(!messages(&u, json!([1, 1.0])).is_empty());
    assert!(!messages(&u, json!([{"a": 1, "b": 2}, {"b": 2.0, "a": 1.0}])).is_empty());
    assert!(messages(&u, json!([1, 2, {"a": 1}, {"a": 2}])).is_empty());
}

#[test]
fn exact_integer_bounds_do_not_lose_precision_above_two_to_the_53() {
    // 2^53 + 1(9007199254740993)은 f64로 반올림하면 2^53(9007199254740992)과 같아진다
    let min = schema(json!({"type": "integer", "minimum": 9007199254740993_i64}));
    assert!(!messages(&min, json!(9007199254740992_i64)).is_empty());
    assert!(messages(&min, json!(9007199254740993_i64)).is_empty());

    let max = schema(json!({"type": "integer", "maximum": 9007199254740992_i64}));
    assert!(!messages(&max, json!(9007199254740993_i64)).is_empty());
    assert!(messages(&max, json!(9007199254740992_i64)).is_empty());
    // 정수 상한과 실수 인스턴스 비교(2^53+2.0은 초과, 10.5는 소수 초과)
    assert!(!messages(&max, json!(9007199254740994.0_f64)).is_empty());
    let num_max = schema(json!({"type": "number", "maximum": 10}));
    assert!(!messages(&num_max, json!(10.5_f64)).is_empty());
    assert!(messages(&num_max, json!(10.0_f64)).is_empty());
}

#[test]
fn exact_integer_extractors_accept_float_integers_and_reject_fractions_or_overflow() {
    assert_eq!(as_exact_u64(&json!(300)), Some(300));
    assert_eq!(as_exact_u64(&json!(300.0)), Some(300));
    assert_eq!(as_exact_u64(&json!(0.0)), Some(0));
    assert_eq!(as_exact_u64(&json!(300.5)), None);
    assert_eq!(as_exact_u64(&json!(-1.0)), None);
    assert_eq!(as_exact_u64(&json!(1e20)), None);

    assert_eq!(as_exact_i64(&json!(5.0)), Some(5));
    assert_eq!(as_exact_i64(&json!(-5.0)), Some(-5));
    assert_eq!(as_exact_i64(&json!(5.5)), None);
    assert_eq!(as_exact_i64(&json!(1e20)), None);
}

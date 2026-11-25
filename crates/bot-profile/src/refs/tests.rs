use super::*;
use serde_json::json;

fn err(doc: Value) -> String {
    let found = expand(doc).expect_err("거절해야 해요");
    format!("[{}] {}", found.code, found.message)
}

#[test]
fn local_refs_are_inlined_and_the_definitions_disappear() {
    let doc = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "from": {"$ref": "#/$defs/Place"},
            "to": {"$ref": "#/definitions/Place"},
            "stops": {"type": "array", "items": {"$ref": "#/$defs/Place"}}
        },
        "$defs": {"Place": {"type": "object", "properties": {"city": {"type": "string"}}}},
        "definitions": {"Place": {"type": "string"}}
    });
    let out = expand(doc).unwrap();
    assert_eq!(
        out,
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "from": {"type": "object", "properties": {"city": {"type": "string"}}},
                "to": {"type": "string"},
                "stops": {"type": "array", "items": {"type": "object", "properties": {"city": {"type": "string"}}}}
            }
        })
    );
}

#[test]
fn a_chain_of_references_and_the_combinators_are_followed() {
    let doc = json!({
        "type": "object",
        "properties": {"a": {"anyOf": [{"$ref": "#/$defs/A"}, {"type": "null"}]}},
        "$defs": {"A": {"$ref": "#/$defs/B"}, "B": {"allOf": [{"$ref": "#/$defs/C"}]}, "C": {"type": "integer"}}
    });
    let out = expand(doc).unwrap();
    assert_eq!(
        out["properties"]["a"],
        json!({"anyOf": [{"allOf": [{"type": "integer"}]}, {"type": "null"}]})
    );
}

fn beside_a_ref(sibling: Value) -> Value {
    let mut node = json!({"$ref": "#/$defs/Text"});
    node.as_object_mut()
        .unwrap()
        .extend(sibling.as_object().unwrap().clone());
    json!({"type": "object", "properties": {"a": node},
           "$defs": {"Text": {"type": "string", "description": "글"}}})
}

#[test]
fn a_condition_beside_a_ref_is_refused_never_dropped_or_weakened() {
    // `allOf`를 객체·숫자·빈 배열로 적어도(잘못된 모양) 조건을 조용히 버리고 통과시키지 않는다. 조건이 되는 키워드는 어떤 모양이든 거절한다
    for sibling in [
        json!({"allOf": {"minLength": 2}}),
        json!({"allOf": 5}),
        json!({"allOf": []}),
        json!({"allOf": [{"minLength": 2}]}),
        json!({"minLength": 2}),
        json!({"type": "string"}),
        json!({"enum": ["a"]}),
        json!({"const": "a"}),
        json!({"properties": {}}),
        json!({"required": ["a"]}),
        json!({"additionalProperties": false}),
        json!({"pattern": "^a$"}),
        json!({"anyOf": [{"type": "null"}]}),
        json!({"oneOf": [{"type": "null"}]}),
        json!({"x-mcp-header": "A"}),
        json!({"unknown-keyword": 1}),
        json!({"description": "메모", "minLength": 2}),
    ] {
        let found = expand(beside_a_ref(sibling.clone())).expect_err(&sibling.to_string());
        assert_eq!(found.code, "ref-sibling", "{sibling}: {}", found.message);
        assert_eq!(found.pointer, "/properties/a", "{sibling}");
    }
    // 최상위의 `$ref`도 같은 규칙이다
    assert!(
        expand(json!({"$ref": "#/$defs/X", "type": "object", "$defs": {"X": {"type": "object"}}}))
            .is_err()
    );
    // `Schema::compile_input`도 같다(검증 단계로 조건 없는 스키마가 넘어가지 않는다)
    assert!(
        crate::schema::Schema::compile_input(beside_a_ref(json!({"allOf": {"minLength": 2}})))
            .is_err()
    );
}

#[test]
fn annotation_keywords_beside_a_ref_ride_on_the_definition() {
    let out = expand(beside_a_ref(json!({"description": "메모", "title": "제목", "default": "x",
                                         "examples": ["y"], "$comment": "c", "deprecated": true,
                                         "readOnly": false, "writeOnly": false, "$schema": "s", "format": "uri"})))
    .unwrap();
    // 같은 자리의 값이 이긴다
    assert_eq!(
        out["properties"]["a"],
        json!({"type": "string", "description": "메모", "title": "제목", "default": "x",
               "examples": ["y"], "$comment": "c", "deprecated": true, "readOnly": false,
               "writeOnly": false, "$schema": "s", "format": "uri"})
    );
    // 정의가 불리언 스키마이면 주석은 검증에 영향이 없으니 정의 그대로다
    let out = expand(
        json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/Any", "description": "d"}},
                            "$defs": {"Any": true}}),
    )
    .unwrap();
    assert_eq!(out["properties"]["a"], json!(true));
    // 최상위의 `$ref`에 `$schema`만 곁들인 모양(이름 있는 모델을 내놓는 생성기가 흔히 쓴다)은 받는다
    assert!(
        expand(json!({"$ref": "#/$defs/X", "$schema": "s", "$defs": {"X": {"type": "object"}}}))
            .is_ok()
    );
}

#[test]
fn ref_fragments_are_percent_decoded_before_the_pointer_is_read() {
    let defs = json!({
        "Item Size": {"type": "integer"},
        "Item%20Size": {"type": "string"},
        "a/b": {"type": "boolean"},
        "a~b": {"type": "null"},
        "도시": {"type": "number"}
    });
    let pick = |reference: &str| {
        let doc =
            json!({"type": "object", "properties": {"x": {"$ref": reference}}, "$defs": defs});
        expand(doc).map(|out| out["properties"]["x"].clone())
    };
    // `%20`은 공백이다: 글자 그대로의 `Item%20Size` 정의가 아니라 `Item Size`를 고른다
    assert_eq!(
        pick("#/$defs/Item%20Size").unwrap(),
        json!({"type": "integer"})
    );
    // 퍼센트 부호화한 `%25`는 `%` 한 글자이고 그 뒤에 `20`이 이어진다 → 글자 그대로의 이름
    assert_eq!(
        pick("#/$defs/Item%2520Size").unwrap(),
        json!({"type": "string"})
    );
    // 퍼센트 부호화가 없는 글자는 그대로, 소문자 16진도 풀고, UTF-8 한글도 풀고, JSON 포인터 이스케이프는 그다음에 푼다(`~1`→`/`, `~0`→`~`)
    assert_eq!(pick("#/$defs/도시").unwrap(), json!({"type": "number"}));
    assert_eq!(
        pick("#/$defs/%EB%8F%84%EC%8B%9C").unwrap(),
        json!({"type": "number"})
    );
    assert_eq!(
        pick("#/$defs/Item%20size".replace("size", "Size").as_str()).unwrap(),
        json!({"type": "integer"})
    );
    assert_eq!(pick("#/$defs/a~1b").unwrap(), json!({"type": "boolean"}));
    assert_eq!(pick("#/$defs/a%7E1b").unwrap(), json!({"type": "boolean"}));
    assert_eq!(pick("#/$defs/a~0b").unwrap(), json!({"type": "null"}));
    assert_eq!(pick("#/$defs/a%7e0b").unwrap(), json!({"type": "null"}));
    // 풀고 나온 `/`는 포인터의 구분자다: 이름 하나만 가리킬 수 있다
    assert_eq!(pick("#/$defs/a%2Fb").unwrap_err().code, "ref-unsupported");
    // 잘못된 퍼센트 표기·UTF-8이 아닌 결과·잘못된 `~` 이스케이프는 거절한다
    for bad in [
        "#/$defs/100%",
        "#/$defs/a%2",
        "#/$defs/a%zz",
        "#/$defs/a%2g",
        "#/$defs/%C3%28",
        "#/$defs/a~2b",
        "#/$defs/a~",
    ] {
        let found = pick(bad).expect_err(bad);
        assert_eq!(found.code, "ref-unsupported", "{bad}: {}", found.message);
    }
    // 정의가 없는 이름은 여전히 없다고 알린다
    assert_eq!(pick("#/$defs/Nope%20Nope").unwrap_err().code, "ref-missing");
}

#[test]
fn names_and_values_that_look_like_keywords_are_data() {
    let doc = json!({
        "type": "object",
        "properties": {
            "$ref": {"type": "string"},
            "definitions": {"type": "string"},
            "$defs": {"type": "string"},
            "mode": {"enum": [{"$ref": "x"}], "default": {"$ref": "#"}, "examples": [{"$ref": "#"}]}
        },
        "required": ["$ref"]
    });
    let out = expand(doc.clone()).unwrap();
    assert_eq!(out, doc);
}

#[test]
fn recursion_is_refused_with_the_chain() {
    let own = json!({"type": "object", "properties": {"child": {"$ref": "#/$defs/Node"}},
                     "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}}});
    let message = err(own);
    assert!(
        message.contains("재귀") && message.contains("$defs/Node → $defs/Node"),
        "{message}"
    );
    let mutual = json!({"properties": {"a": {"$ref": "#/$defs/A"}},
                        "$defs": {"A": {"items": {"$ref": "#/$defs/B"}}, "B": {"anyOf": [{"$ref": "#/$defs/A"}]}}});
    let message = err(mutual);
    assert!(message.contains("$defs/A → $defs/B → $defs/A"), "{message}");
    let root = err(json!({"type": "object", "properties": {"x": {"$ref": "#"}}}));
    assert!(root.contains("재귀"), "{root}");
}

#[test]
fn external_missing_and_other_pointers_are_refused_clearly() {
    for (doc, want) in [
        (
            json!({"properties": {"a": {"$ref": "https://example.test/x.json"}}}),
            "외부",
        ),
        (
            json!({"properties": {"a": {"$ref": "other.json#/$defs/A"}}}),
            "외부",
        ),
        (
            json!({"properties": {"a": {"$ref": "#/$defs/Nope"}}, "$defs": {}}),
            "정의가 없어요",
        ),
        (
            json!({"properties": {"a": {"$ref": "#/properties/b"}, "b": {"type": "string"}}}),
            "지원하지 않는",
        ),
        (
            json!({"properties": {"a": {"$ref": "#/$defs/A/properties/x"}}, "$defs": {"A": {}}}),
            "정의 이름 하나",
        ),
        (json!({"properties": {"a": {"$ref": 5}}}), "문자열"),
        (json!({"$defs": []}), "객체"),
        (
            json!({"properties": {"a": {"type": "object", "$defs": {"X": {}}}}}),
            "최상위",
        ),
    ] {
        let message = err(doc.clone());
        assert!(message.contains(want), "{doc}: {message}");
    }
}

#[test]
fn an_exponential_expansion_stops_at_the_size_limit() {
    // 각 정의가 다음 정의를 두 번 부른다: 25단계면 2^25개
    let mut defs = Map::new();
    for i in 0..25 {
        defs.insert(
            format!("D{i}"),
            json!({"allOf": [{"$ref": format!("#/$defs/D{}", i + 1)}, {"$ref": format!("#/$defs/D{}", i + 1)}]}),
        );
    }
    defs.insert("D25".into(), json!({"type": "string"}));
    let doc = json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}}, "$defs": defs});
    let message = err(doc);
    assert!(
        message.contains("너무 커요") || message.contains("너무 깊어요"),
        "{message}"
    );
}

#[test]
fn a_schema_without_references_is_returned_as_it_is() {
    let doc = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
    assert_eq!(expand(doc.clone()).unwrap(), doc);
    assert_eq!(expand(json!(true)).unwrap(), json!(true));
}

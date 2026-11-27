use super::*;
use serde_json::json;

fn err(doc: Value) -> String {
    let found = expand(doc).expect_err("expected rejection");
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
           "$defs": {"Text": {"type": "string", "description": "text"}}})
}

#[test]
fn a_condition_beside_a_ref_is_refused_never_dropped_or_weakened() {
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
        json!({"description": "note", "minLength": 2}),
    ] {
        let found = expand(beside_a_ref(sibling.clone())).expect_err(&sibling.to_string());
        assert_eq!(found.code, "ref-sibling", "{sibling}: {}", found.message);
        assert_eq!(found.pointer, "/properties/a", "{sibling}");
    }
    assert!(
        expand(json!({"$ref": "#/$defs/X", "type": "object", "$defs": {"X": {"type": "object"}}}))
            .is_err()
    );
    assert!(
        crate::schema::Schema::compile_input(beside_a_ref(json!({"allOf": {"minLength": 2}})))
            .is_err()
    );
}

#[test]
fn annotation_keywords_beside_a_ref_ride_on_the_definition() {
    let out = expand(beside_a_ref(json!({"description": "note", "title": "title", "default": "x",
                                         "examples": ["y"], "$comment": "c", "deprecated": true,
                                         "readOnly": false, "writeOnly": false, "$schema": "s", "format": "uri"})))
    .unwrap();
    assert_eq!(
        out["properties"]["a"],
        json!({"type": "string", "description": "note", "title": "title", "default": "x",
               "examples": ["y"], "$comment": "c", "deprecated": true, "readOnly": false,
               "writeOnly": false, "$schema": "s", "format": "uri"})
    );
    // Annotations on a boolean schema leave the boolean definition unchanged.
    let out = expand(
        json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/Any", "description": "d"}},
                            "$defs": {"Any": true}}),
    )
    .unwrap();
    assert_eq!(out["properties"]["a"], json!(true));
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
        "★": {"type": "number"}
    });
    let pick = |reference: &str| {
        let doc =
            json!({"type": "object", "properties": {"x": {"$ref": reference}}, "$defs": defs});
        expand(doc).map(|out| out["properties"]["x"].clone())
    };
    assert_eq!(
        pick("#/$defs/Item%20Size").unwrap(),
        json!({"type": "integer"})
    );
    assert_eq!(
        pick("#/$defs/Item%2520Size").unwrap(),
        json!({"type": "string"})
    );
    // Unencoded and percent-encoded 3-byte UTF-8, plus JSON Pointer escapes (`~1` -> `/`, `~0` -> `~`).
    assert_eq!(pick("#/$defs/★").unwrap(), json!({"type": "number"}));
    assert_eq!(
        pick("#/$defs/%E2%98%85").unwrap(),
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
    // Decoded `/` acts as a JSON Pointer delimiter, which exceeds a single definition name.
    assert_eq!(pick("#/$defs/a%2Fb").unwrap_err().code, "ref-unsupported");
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
        message.contains("[ref-recursive]") && message.matches("$defs/Node").count() >= 2,
        "{message}"
    );
    let mutual = json!({"properties": {"a": {"$ref": "#/$defs/A"}},
                        "$defs": {"A": {"items": {"$ref": "#/$defs/B"}}, "B": {"anyOf": [{"$ref": "#/$defs/A"}]}}});
    let message = err(mutual);
    assert!(
        message.contains("[ref-recursive]")
            && message.contains("$defs/A")
            && message.contains("$defs/B"),
        "{message}"
    );
    let root = err(json!({"type": "object", "properties": {"x": {"$ref": "#"}}}));
    assert!(root.contains("[ref-recursive]"), "{root}");
}

#[test]
fn external_missing_and_other_pointers_are_refused_clearly() {
    for (doc, want) in [
        (
            json!({"properties": {"a": {"$ref": "https://example.test/x.json"}}}),
            "[ref-external]",
        ),
        (
            json!({"properties": {"a": {"$ref": "other.json#/$defs/A"}}}),
            "[ref-external]",
        ),
        (
            json!({"properties": {"a": {"$ref": "#/$defs/Nope"}}, "$defs": {}}),
            "[ref-missing]",
        ),
        (
            json!({"properties": {"a": {"$ref": "#/properties/b"}, "b": {"type": "string"}}}),
            "[ref-unsupported]",
        ),
        (
            json!({"properties": {"a": {"$ref": "#/$defs/A/properties/x"}}, "$defs": {"A": {}}}),
            "[ref-unsupported]",
        ),
        (json!({"properties": {"a": {"$ref": 5}}}), "[bad-ref]"),
        (json!({"$defs": []}), "[bad-defs]"),
        (
            json!({"properties": {"a": {"type": "object", "$defs": {"X": {}}}}}),
            "[defs-nested]",
        ),
    ] {
        let message = err(doc.clone());
        assert!(message.contains(want), "{doc}: {message}");
    }
}

#[test]
fn an_exponential_expansion_stops_at_the_size_limit() {
    // 25 levels referencing the next definition twice expand to 2^25 nodes.
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
        message.contains("[ref-too-large]")
            || message.contains("[ref-too-deep]")
            || message.contains("[too-deep]"),
        "{message}"
    );
}

#[test]
fn a_schema_without_references_is_returned_as_it_is() {
    let doc = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
    assert_eq!(expand(doc.clone()).unwrap(), doc);
    assert_eq!(expand(json!(true)).unwrap(), json!(true));
}

//! 해시와 정규 JSON. 키를 직접 정렬해, serde_json의 `preserve_order`가 켜져도 결과가 같다.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write;

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 키를 정렬하고 공백 없이 쓴 JSON. 내부 해시(판본)용이며 RFC 8785와 같다고 보장하지 않는다.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_value(&map[key], out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn canonical_sorts_keys_at_every_level() {
        let v = json!({"b": [{"z": 1, "a": null}], "a": "x\"y"});
        assert_eq!(canonical_json(&v), r#"{"a":"x\"y","b":[{"a":null,"z":1}]}"#);
    }
}

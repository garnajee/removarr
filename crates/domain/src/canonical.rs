//! Deterministic serialization: JSON with recursively sorted object keys, and SHA-256 helpers.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::DomainError;

/// Serializes `value` to canonical JSON bytes (object keys sorted, no whitespace).
///
/// The output does not depend on map insertion order, even if `serde_json` is built with
/// `preserve_order` somewhere in the dependency graph.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, DomainError> {
    let v = serde_json::to_value(value).map_err(|e| DomainError::Serialization(e.to_string()))?;
    let mut out = Vec::new();
    write_value(&v, &mut out)?;
    Ok(out)
}

/// Canonical JSON as a `String` (for JSON database columns).
pub fn canonical_json_string<T: Serialize + ?Sized>(value: &T) -> Result<String, DomainError> {
    let bytes = canonical_json(value)?;
    String::from_utf8(bytes).map_err(|e| DomainError::Serialization(e.to_string()))
}

fn write_value(v: &Value, out: &mut Vec<u8>) -> Result<(), DomainError> {
    let ser = |e: serde_json::Error| DomainError::Serialization(e.to_string());
    match v {
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, k).map_err(ser)?;
                out.push(b':');
                if let Some(child) = map.get(k) {
                    write_value(child, out)?;
                }
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        scalar => serde_json::to_writer(&mut *out, scalar).map_err(ser)?,
    }
    Ok(())
}

/// Lower-case hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    s
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_recursively() {
        let v = json!({"b": 1, "a": {"z": [3, {"y": 1, "x": 2}], "c": null}});
        let s = canonical_json_string(&v).unwrap();
        assert_eq!(s, r#"{"a":{"c":null,"z":[3,{"x":2,"y":1}]},"b":1}"#);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

//! Column encoding helpers: canonical JSON, enum text, integer casts and text scrubbing.

use removarr_domain::canonical::canonical_json_string;
use removarr_domain::error::PortError;
use removarr_domain::ids::RawUuid;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::error::{domain, integrity, map_sqlx};

/// Canonical JSON for a JSON column (sorted keys, so equal values compare equal as text).
pub(crate) fn enc<T: Serialize + ?Sized>(v: &T) -> Result<String, PortError> {
    canonical_json_string(v).map_err(domain)
}

pub(crate) fn dec<T: DeserializeOwned>(s: &str) -> Result<T, PortError> {
    serde_json::from_str(s).map_err(|_| integrity("stored JSON does not match its type"))
}

/// Text form of a serde enum: the bare string for unit variants, canonical JSON otherwise.
pub(crate) fn enum_text<T: Serialize>(v: &T) -> Result<String, PortError> {
    let value = serde_json::to_value(v).map_err(|e| integrity(e.to_string()))?;
    match value {
        Value::String(s) => Ok(s),
        other => enc(&other),
    }
}

pub(crate) fn from_enum_text<T: DeserializeOwned>(s: &str) -> Result<T, PortError> {
    if let Ok(v) = serde_json::from_value(Value::String(s.to_string())) {
        return Ok(v);
    }
    dec(s)
}

/// u64 values are stored as the same 64 bits in a SQLite INTEGER.
pub(crate) fn to_i64(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

pub(crate) fn from_i64(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

pub(crate) fn to_u32(v: i64) -> Result<u32, PortError> {
    u32::try_from(v).map_err(|_| integrity("stored integer out of range"))
}

pub(crate) fn uuid_text(u: RawUuid) -> String {
    u.to_string()
}

pub(crate) fn parse_uuid(s: &str) -> Result<RawUuid, PortError> {
    RawUuid::parse_str(s).map_err(|_| integrity("stored id is not a uuid"))
}

pub(crate) fn col<'r, T>(row: &'r SqliteRow, name: &str) -> Result<T, PortError>
where
    T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get(name).map_err(map_sqlx)
}

/// Removes URLs and credential-looking tokens from free text before it is persisted, with the
/// one shared rule (`removarr_domain::redact::sanitize`). Tracker URLs and secrets must never
/// reach the database (MASTER 2 and 3).
pub(crate) fn scrub(text: &str) -> String {
    removarr_domain::redact::sanitize(text)
}

/// Applies [`scrub`] to every string leaf of a JSON value.
pub(crate) fn scrub_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(scrub(s)),
        Value::Array(a) => Value::Array(a.iter().map(scrub_value).collect()),
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), scrub_value(v))).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_secrets_are_removed() {
        let s = scrub(
            "GET http://tracker.example/announce?passkey=abc failed apikey=xyz Authorization: Bearer tok\ndone",
        );
        assert!(!s.contains("tracker"));
        assert!(!s.contains("abc") && !s.contains("xyz") && !s.contains("tok"));
        assert!(s.contains("failed") && s.contains("done"));
    }

    #[test]
    fn scrub_is_as_strict_as_the_shared_sanitizer() {
        let s = scrub("announce/x?passkey=abc123 {\"password\":\"pw-9\"} Cookie: sid=zzz9");
        for leaked in ["abc123", "pw-9", "zzz9"] {
            assert!(!s.contains(leaked), "{leaked} in {s}");
        }
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(scrub("No such file: a/b.mkv"), "No such file: a/b.mkv");
        assert_eq!(scrub("line1\nline2"), "line1\nline2");
    }

    #[test]
    fn u64_roundtrip_keeps_high_bit() {
        for v in [0u64, 1, u64::MAX, 1 << 63] {
            assert_eq!(from_i64(to_i64(v)), v);
        }
    }

    #[test]
    fn enum_text_is_bare_for_unit_variants() {
        use removarr_domain::plan::PlanStatus;
        assert_eq!(
            enum_text(&PlanStatus::PartiallyExecuted).unwrap(),
            "partially_executed"
        );
        assert_eq!(
            from_enum_text::<PlanStatus>("partially_executed").unwrap(),
            PlanStatus::PartiallyExecuted
        );
    }
}

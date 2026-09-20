//! Generic `serde_json` value accessors with defaults.
//!
//! No provider meaning here: typed getters with defaults, object accessors,
//! and stored-value truthiness.

use serde_json::{Map, Value};
use std::sync::OnceLock;

/// `v` as an object.
pub fn as_object<'a>(v: &'a Value) -> Option<&'a Map<String, Value>> {
    v.as_object()
}

/// `v` as an object, or an empty map when it is not one.
pub fn as_object_or_empty<'a>(v: &'a Value) -> &'a Map<String, Value> {
    v.as_object().unwrap_or_else(|| empty_map_ref())
}

/// Owned `bool` for `key`; `false` when missing or not a bool.
pub fn get_bool_value(v: &Value, key: &str) -> bool {
    matches!(v.get(key), Some(Value::Bool(true)))
}

/// Owned `bool` for `key` from a map; `false` when missing or not a bool.
pub fn get_bool_map(map: &Map<String, Value>, key: &str) -> bool {
    matches!(map.get(key), Some(Value::Bool(true)))
}

/// Owned `String` for `key`; empty string when missing or not a string.
pub fn get_string(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Owned `"name"` value; first non-empty string wins, else `fallback`.
pub fn get_name_or(v: &Value, fallback: &str) -> String {
    match v.get("name") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => fallback.to_string(),
    }
}

/// Stored-value truthiness: `None` and null are false; numbers, strings,
/// arrays, and objects are false when zero or empty.
pub fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().map(|f| f != 0.0).unwrap_or(false)
            }
        }
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn empty_map_ref() -> &'static Map<String, Value> {
    static EMPTY: OnceLock<Map<String, Value>> = OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

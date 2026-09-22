//! TOML text primitives shared by both config writers.

use crate::{Res, fail};
use serde_json::Value;

/// Python `tomllib`-compatible escaping of a scalar JSON value into TOML text.
pub fn toml_escape(value: &Value) -> Res<String> {
    match value {
        Value::Bool(b) => Ok(if *b { "true".into() } else { "false".into() }),
        Value::Number(n) => Ok(number_to_string(n)),
        Value::String(s) => Ok(format!(
            "\"{}\"",
            s.replace('\\', "\\\\").replace('"', "\\\"")
        )),
        other => fail(format!(
            "unsupported TOML value type: {}",
            json_type_name(other)
        )),
    }
}

/// Bare TOML key when it is alphanumeric/_/-, else a quoted string.
pub fn toml_subkey(ident: &str) -> String {
    if !ident.is_empty()
        && ident
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        ident.to_string()
    } else {
        toml_escape(&Value::String(ident.to_string())).unwrap_or_default()
    }
}

/// Rejects invalid TOML text using a real parser.
pub fn validate_toml_text(text: &str) -> Res<()> {
    match text.parse::<toml::Table>() {
        Ok(_) => Ok(()),
        Err(e) => fail(format!("invalid TOML write: {e}")),
    }
}

fn number_to_string(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else if let Some(f) = n.as_f64() {
        // Fields are always ints in practice; mirror str(int) for safety.
        format!("{}", f as i64)
    } else {
        n.to_string()
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        _ => "unknown",
    }
}

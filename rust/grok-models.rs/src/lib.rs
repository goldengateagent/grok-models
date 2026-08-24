//! Native port of `grok-models.py` (behavior-identical).
//!
//! Module map:
//! - `difflib`:   Python difflib port (`get_close_matches`) for hint messages
//! - `jsonio`:    ordered JSON load/dump + atomic writes
//! - `core`:      model id/table-key helpers, sorting, TOML field building
//! - `toml_out`:  `[model.*]` table emission and owned-section stripping
//! - `sync`:      models.dev reconciliation and config.toml writing
//! - `commands`:  CLI command implementations (renderers, toggle, ...)
//! - `cli`:       argparse-equivalent parser
//! - `fallback`:  numbered (non-TTY) interactive flows
//! - `theme`:     Tokyo Nights palette, truecolor SGR, opacity compensation
//! - `tui`:       raw-mode ANSI screens (curses equivalent)
//! - `flow`:      interactive `--config` orchestration (TUI + numbered)

pub mod cli;
pub mod commands;
pub mod core;
pub mod difflib;
pub mod fallback;
pub mod flow;
pub mod jsonio;
pub mod paths;
pub mod sync;
pub mod theme;
pub mod toml_out;
pub mod tui;

use serde_json::Value;

/// Fatal error mapped to exit code 1, mirroring `SyncError`.
#[derive(Debug)]
pub struct SyncError(pub String);

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SyncError {}

pub type Res<T> = Result<T, SyncError>;

pub fn fail<T>(message: impl Into<String>) -> Res<T> {
    Err(SyncError(message.into()))
}

// ---------------------------------------------------------------------------
// Small serde_json helpers mirroring Python dict access semantics.
// ---------------------------------------------------------------------------

pub fn jget<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
}

pub fn as_obj<'a>(v: &'a Value) -> Option<&'a serde_json::Map<String, Value>> {
    v.as_object()
}

pub fn obj_or_empty<'a>(v: &'a Value) -> &'a serde_json::Map<String, Value> {
    v.as_object().unwrap_or_else(|| empty_map_ref())
}

fn empty_map_ref() -> &'static serde_json::Map<String, Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(serde_json::Map::new)
}

/// Python truthiness for the JSON values this tool stores.
pub fn truthy(v: Option<&Value>) -> bool {
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

/// `m.get(key, default)` where the value must be a bool to be meaningful.
/// Accepts `&Value` for any input shape — a `Map` is treated as the whole value.
pub fn get_bool(v: &Value, key: &str, default: bool) -> bool {
    match v {
        Value::Object(o) => match o.get(key) {
            Some(Value::Bool(b)) => *b,
            _ => default,
        },
        _ => default,
    }
}

/// `get_bool` for a `&Map` argument (auto-wraps).
pub fn get_bool_obj(o: &serde_json::Map<String, Value>, key: &str, default: bool) -> bool {
    get_bool(&Value::Object(o.clone()), key, default)
}

/// `get_bool` for an existing `&Value` argument (alias, same logic).
pub fn get_bool_val(v: &Value, key: &str, default: bool) -> bool {
    get_bool(v, key, default)
}

pub fn first_env_key_from(o: &serde_json::Map<String, Value>) -> String {
    match o.get("env_key") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

pub fn provider_label_from(o: &serde_json::Map<String, Value>) -> String {
    core::provider_label(&Value::Object(o.clone()))
}

/// Python `m.get("env_key")` where only strings count; empty string otherwise.
pub fn get_str(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Python `m.get("name") or fallback`: first non-empty string wins.
pub fn name_or(v: &Value, fallback: &str) -> String {
    match v.get("name") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => fallback.to_string(),
    }
}

//! Native port of `grok-models.py` (behavior-identical).
//!
//! Module map:
//! - `difflib`:   Python difflib port (`get_close_matches`) for hint messages
//! - `jsonio`:    ordered JSON load/dump + atomic writes
//! - `paths`:     GROK_HOME / CODEX_HOME locations
//! - `core`:      model id/table-key helpers, sorting, TOML field building
//! - `benchmarks`: model benchmark score tables
//! - `toml_out`:  `[model.*]` table emission and owned-section stripping
//! - `sync`:      provider entry writes, models.dev reconciliation,
//!                config.toml writing
//! - `cli`:       CLI surface — `cli::args` argparse-equivalent parser and
//!                help text, `cli::commands` command implementations
//! - `fallback`:  numbered (non-TTY) interactive flows
//! - `theme`:     Tokyo Nights palette, truecolor SGR, opacity compensation
//! - `tui`:       Ratatui/Crossterm screens (curses equivalent)
//! - `flow`:      interactive TUI orchestration (TUI + numbered fallback)

pub mod benchmarks;
pub mod cli;
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

/// Fatal error mapped to exit code 1, mirroring the Python `SyncError`.
///
/// `warnings` holds diagnostics the operation produced before it failed, so a
/// caller that only receives the error still has them to print. Rendered lines
/// rather than structured values: this type sits at the crate root and must not
/// depend on `sync`'s warning enum.
#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub warnings: Vec<String>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            warnings: Vec::new(),
        }
    }

    /// Attaches diagnostics produced before the failure.
    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

pub type Res<T> = Result<T, Error>;

pub fn fail<T>(message: impl Into<String>) -> Res<T> {
    Err(Error::new(message))
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

/// `get_bool` for a `&Map` argument (no clone).
pub fn get_bool_map(o: &serde_json::Map<String, Value>, key: &str, default: bool) -> bool {
    match o.get(key) {
        Some(Value::Bool(b)) => *b,
        _ => default,
    }
}

pub fn provider_label_from(o: &serde_json::Map<String, Value>) -> String {
    core::provider_label(o)
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

/// Serializes tests that mutate the process-global `GROK_HOME` so parallel
/// threads never race over the shared environment variable.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};
    static LOCK: Mutex<()> = Mutex::new(());
    pub fn grok_home_lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

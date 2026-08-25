//! `[model.*]` TOML generation — string-built exactly like the Python
//! (`toml_escape` / `emit_model_table` / `strip_owned_toml_sections`) so the
//! output is byte-identical. The `toml` crate is used only to validate the
//! generated text, mirroring Python's `tomllib` check.

use crate::{core, fail, Res};
use serde_json::Value;
use std::path::Path;

pub const TOML_SCALAR_FIELDS: [&str; 9] = [
    "model",
    "base_url",
    "name",
    "env_key",
    "api_backend",
    "supports_reasoning_effort",
    "reasoning_effort",
    "context_window",
    "description",
];

fn toml_escape(value: &Value) -> Res<String> {
    match value {
        Value::Bool(b) => Ok(if *b { "true".into() } else { "false".into() }),
        Value::Number(n) => Ok(number_to_string(n)),
        Value::String(s) => Ok(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))),
        other => fail(format!("unsupported TOML value type: {}", json_type_name(other))),
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

/// `emit_model_table`
pub fn emit_model_table(table_key: &str, fields: &serde_json::Map<String, Value>) -> Res<String> {
    let mut lines: Vec<String> = vec![format!("[model.{table_key}]")];
    for key in TOML_SCALAR_FIELDS {
        if key == "api_backend" {
            // Python: fields.get(key) or 'chat_completions'
            let v = fields.get(key).cloned().unwrap_or(Value::Null);
            let chosen = if crate::truthy(Some(&v)) {
                v
            } else {
                Value::String("chat_completions".into())
            };
            lines.push(format!("{key} = {}", toml_escape(&chosen)?));
            continue;
        }
        // Both the OPTIONAL_META slice and the generic branch reduce to
        // "write only when present", same as Python's control flow.
        if fields.contains_key(key) {
            lines.push(format!("{key} = {}", toml_escape(&fields[key])?));
        }
    }
    let empty = Vec::new();
    let efforts = fields
        .get("reasoning_efforts")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for row in efforts {
        lines.push(String::new());
        lines.push(format!("[[model.{table_key}.reasoning_efforts]]"));
        for rk in ["id", "value", "label", "default"] {
            match row.get(rk) {
                Some(v) => lines.push(format!("{rk} = {}", toml_escape(v)?)),
                None => return fail("unsupported TOML value type: KeyError"),
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

fn is_table_header(line: &str) -> bool {
    let stripped = line.trim_start();
    stripped.starts_with('[') && stripped.contains(']')
}

fn owned_table_key(header: &str) -> Option<String> {
    let inner = header.trim();
    let inner = if inner.starts_with("[[") && inner.ends_with("]]") {
        &inner[2..inner.len() - 2]
    } else if inner.starts_with('[') && inner.ends_with(']') {
        &inner[1..inner.len() - 1]
    } else {
        return None;
    };
    let inner = inner.trim();
    if !inner.starts_with("model.") {
        return None;
    }
    let rest = &inner["model.".len()..];
    Some(rest.splitn(2, '.').next().unwrap_or("").to_string())
}

fn is_owned_header(header: &str, provider_ids: &[String]) -> bool {
    match owned_table_key(header) {
        None => false,
        Some(key) => provider_ids.iter().any(|pid| key.starts_with(&format!("{pid}-"))),
    }
}

/// `splitlines(keepends=True)` equivalent (ASCII/newline-oriented).
fn split_lines_keepends(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            out.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `strip_owned_toml_sections`
pub fn strip_owned_toml_sections(text: &str, provider_ids: &[String]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines = split_lines_keepends(text);
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if is_table_header(lines[i]) && is_owned_header(lines[i], provider_ids) {
            i += 1;
            while i < lines.len() && !is_table_header(lines[i]) {
                i += 1;
            }
            continue;
        }
        out.push(lines[i]);
        i += 1;
    }
    out.concat()
}

/// `write_toml_stdlib`: kept sections + regenerated tables. `removed_keys`
/// holds full table keys (provider-modelid) to drop from the existing file —
/// exact matches only, used for deleted-provider cleanup.
pub fn write_toml_stdlib(
    path: &Path,
    provider_ids: &[String],
    tables: &[(String, serde_json::Map<String, Value>)],
    removed_keys: &std::collections::HashSet<String>,
) -> Res<String> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };
    let kept = strip_removed_and_unowned_sections(&existing, provider_ids, removed_keys);
    let mut chunks: Vec<String> = vec![kept.trim_end().to_string()];
    for (table_key, fields) in tables {
        let t = emit_model_table(table_key, fields)?;
        chunks.push(t.trim_end().to_string());
    }
    let joined: Vec<&String> = chunks.iter().filter(|c| !c.is_empty()).collect();
    let refs: Vec<&str> = joined.iter().map(|s| s.as_str()).collect();
    let mut text = refs.join("\n\n");
    text.push('\n');
    Ok(text)
}

/// Drop every `[model.*]` section whose full key is in `removed_keys`
/// (exact match), plus sections owned by `provider_ids` (prefix match, the
/// tool's own rebuildable tables).
fn strip_removed_and_unowned_sections(
    text: &str,
    provider_ids: &[String],
    removed_keys: &std::collections::HashSet<String>,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines = split_lines_keepends(text);
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if is_table_header(lines[i]) {
            let key = owned_table_key(lines[i]);
            let is_removed = key.as_deref().is_some_and(|k| removed_keys.contains(k));
            let is_owned = is_owned_header(lines[i], provider_ids);
            if is_removed || is_owned {
                i += 1;
                while i < lines.len() && !is_table_header(lines[i]) {
                    i += 1;
                }
                continue;
            }
        }
        out.push(lines[i]);
        i += 1;
    }
    out.concat()
}

/// `validate_toml_text` — parse with a real TOML parser like tomllib.
pub fn validate_toml_text(text: &str) -> Res<()> {
    match text.parse::<toml::Value>() {
        Ok(_) => Ok(()),
        Err(e) => fail(format!("invalid TOML write: {e}")),
    }
}

/// `write_config_toml`: backup then atomically rewrite config.toml.
/// `removed_keys` are full table keys (provider-modelid) to drop from the
/// existing file — exact matches only.
pub fn write_config_toml(
    path: &Path,
    provider_ids: &[String],
    tables: &[(String, serde_json::Map<String, Value>)],
    removed_keys: &std::collections::HashSet<String>,
) -> Res<std::path::PathBuf> {
    if path.exists() {
        let bak = path.with_file_name(format!(
            "{}.bak",
            path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default()
        ));
        std::fs::copy(path, &bak).map_err(|e| {
            crate::SyncError(format!("failed to write {}: {}", bak.display(), e))
        })?;
    }
    let text = write_toml_stdlib(path, provider_ids, tables, removed_keys)?;
    validate_toml_text(&text)?;
    crate::jsonio::atomic_write(path, &text)?;
    Ok(path.to_path_buf())
}

// Re-exported for sync.rs convenience.
pub use core::table_model_id;

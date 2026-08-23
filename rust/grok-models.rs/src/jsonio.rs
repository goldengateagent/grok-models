//! Ordered JSON load/dump + atomic writes, mirroring the Python helpers.
//!
//! serde_json is built with `preserve_order` so object key order matches the
//! input file / models.dev payload exactly, like Python dicts.

use crate::paths;
use crate::{fail, Res};
use serde_json::Value;
use std::io::Write;
use std::path::Path;

pub fn atomic_write(path: &Path, text: &str) -> Res<()> {
    let io_err = |e| SyncErrIo(format!("failed to write {}: {}", path.display(), e));
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
    }
    // Python: tmp = path.with_name(path.name + ".tmp"); tmp.replace(path)
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default()
    ));
    std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(io_err)?;
    std::fs::rename(&tmp, path).map_err(io_err)?;
    Ok(())
}

// Local shim so jsonio doesn't re-export SyncError plumbing everywhere.
struct SyncErrIo(String);
impl From<SyncErrIo> for crate::SyncError {
    fn from(e: SyncErrIo) -> Self {
        crate::SyncError(e.0)
    }
}

/// Serialize with Python's `json.dumps(obj, indent=2, ensure_ascii=False)`
/// formatting plus trailing newline.
pub fn dumps_pretty(v: &Value) -> String {
    let mut out = serde_json::to_string_pretty(v).expect("serializable JSON");
    out.push('\n');
    out
}

pub fn dump_json(path: &Path, v: &Value) -> Res<()> {
    atomic_write(path, &dumps_pretty(v))
}

pub fn load_json(path: &Path, default: &Value) -> Res<Value> {
    if !path.exists() {
        dump_json(path, default)?;
        return Ok(default.clone());
    }
    let text = std::fs::read_to_string(path).map_err(|e| crate::SyncError(format!(
        "invalid JSON in {}: {}", path.display(), e
    )))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => {
            if !v.is_object() {
                return fail(format!("{} must contain a JSON object", path.display()));
            }
            Ok(v)
        }
        Err(e) => fail(format!("invalid JSON in {}: {}", path.display(), e)),
    }
}

/// `load_providers()`: providers.json with the same validation and default
/// creation as the Python tool.
pub fn load_providers() -> Res<Value> {
    let default = serde_json::json!({ "providers": [] });
    let mut data = load_json(&paths::providers_path(), &default)?;
    let obj = data.as_object_mut().unwrap();
    obj.entry("providers".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !obj["providers"].is_array() {
        return fail("providers.json: 'providers' must be a list");
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_format_matches_python() {
        let v = serde_json::json!({"b": true, "list": [1, 2], "s": "x\"y", "empty": {}, "e": []});
        assert_eq!(
            dumps_pretty(&v),
            "{\n  \"b\": true,\n  \"list\": [\n    1,\n    2\n  ],\n  \"s\": \"x\\\"y\",\n  \"empty\": {},\n  \"e\": []\n}\n"
        );
    }
}

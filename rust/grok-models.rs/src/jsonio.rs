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
    // Python: tmp = path.with_name(path.name + ".tmp"). The counter keeps
    // concurrent writes in one process from sharing (and racing on) a tmp
    // name; Python needs no equivalent because of the GIL.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
        seq,
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

// ---------------------------------------------------------------------------
// Canonical providers.json layout. Every read and write goes through these
// shapes, so entries come out identical no matter which code path (import,
// add-provider, sync) produced them: fields in canonical order, providers
// alphabetically by display name, models alphabetically by display name.
// ---------------------------------------------------------------------------

pub const TOP_LEVEL_KEY_ORDER: [&str; 7] =
    [
        "include_descriptions",
        "write_codex_config_toml",
        "codex_model_provider",
        "last_updated",
        "last_synced",
        "providers",
        "removed_providers",
    ];
pub const PROVIDER_KEY_ORDER: [&str; 7] = [
    "id",
    "name",
    "env_key",
    "base_url",
    "enabled",
    "auth_models_list",
    "models",
];
const MODEL_KEY_ORDER: [&str; 8] = [
    "enabled",
    "name",
    "description",
    "modalities",
    "context_window",
    "supports_reasoning_effort",
    "reasoning_effort",
    "reasoning_efforts",
];

/// Default for the top-level include_descriptions flag when providers.json
/// does not carry it yet (off).
pub const INCLUDE_DESCRIPTIONS_DEFAULT: bool = false;
/// Default for write_codex_config_toml when providers.json does not carry it.
pub const WRITE_CODEX_CONFIG_TOML_DEFAULT: bool = false;
/// Default for codex_model_provider when providers.json does not carry it.
pub const CODEX_MODEL_PROVIDER_DEFAULT: &str = "";

/// models.dev `description` for one model entry, or None when absent/empty.
pub fn catalog_description(minfo: &Value) -> Option<&str> {
    minfo
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// models.dev `modalities` object, or None when absent/not an object.
pub fn catalog_modalities(minfo: &Value) -> Option<Value> {
    match minfo.get("modalities") {
        Some(v) if v.is_object() => Some(v.clone()),
        _ => None,
    }
}

/// Insert the catalog description into a model entry map (seed path).
pub fn seed_description(entry: &mut serde_json::Map<String, Value>, minfo: &Value) {
    if let Some(desc) = catalog_description(minfo) {
        entry.insert("description".into(), Value::String(desc.to_string()));
    }
}

/// Rebuild an object with known keys first in canonical order; any unknown
/// keys are preserved after them in their original order.
pub fn order_keys(
    data: &serde_json::Map<String, Value>,
    key_order: &[&str],
) -> serde_json::Map<String, Value> {
    let mut ordered = serde_json::Map::new();
    for key in key_order {
        if let Some(v) = data.get(*key) {
            ordered.insert((*key).to_string(), v.clone());
        }
    }
    for (k, v) in data {
        if !key_order.contains(&k.as_str()) {
            ordered.insert(k.clone(), v.clone());
        }
    }
    ordered
}

fn str_field<'a>(o: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    o.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Sort providers alphabetically by display name (id as fallback), lowercase.
pub fn provider_sort_key(provider: &Value) -> String {
    let key = match provider.as_object() {
        Some(o) => {
            let name = str_field(o, "name");
            if name.is_empty() { str_field(o, "id") } else { name }
        }
        None => "",
    };
    key.to_lowercase()
}

/// Sort models by display name (falling back to the model id), lowercase.
pub fn model_name_key(mid: &str, minfo: &Value) -> String {
    let name = minfo
        .as_object()
        .map(|o| str_field(o, "name"))
        .filter(|s| !s.is_empty())
        .unwrap_or(mid);
    name.to_lowercase()
}

/// Canonical form of one provider entry: ordered fields, models sorted
/// alphabetically by display name.
pub fn order_provider_entry(
    provider: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut entry = order_keys(provider, &PROVIDER_KEY_ORDER);
    if let Some(models) = entry.get("models").and_then(Value::as_object).cloned() {
        let mut pairs: Vec<(&String, &Value)> = models.iter().collect();
        pairs.sort_by_key(|(mid, minfo)| model_name_key(mid, minfo));
        let sorted: serde_json::Map<String, Value> = pairs
            .into_iter()
            .map(|(mid, minfo)| {
                let v = match minfo.as_object() {
                    Some(o) => Value::Object(order_keys(o, &MODEL_KEY_ORDER)),
                    None => minfo.clone(),
                };
                (mid.clone(), v)
            })
            .collect();
        entry.insert("models".to_string(), Value::Object(sorted));
    }
    entry
}

/// Canonicalize the `providers` array of a doc: ordered fields per entry,
/// list sorted by provider sort key. Used on write only.
fn canonicalize_providers(data: &mut Value) {
    if let Some(Value::Array(providers)) = data.get_mut("providers") {
        let mut entries: Vec<Value> = providers
            .iter()
            .map(|p| match p.as_object() {
                Some(o) => Value::Object(order_provider_entry(o)),
                None => p.clone(),
            })
            .collect();
        entries.sort_by_key(provider_sort_key);
        *providers = entries;
    }
}

/// Ids of configured providers with enabled=True, in file order.
pub fn enabled_provider_ids(doc: &Value) -> Vec<String> {
    doc.get("providers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let obj = p.as_object()?;
                    let pid = obj.get("id").and_then(Value::as_str)?;
                    if pid.is_empty() {
                        return None;
                    }
                    if !obj.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
                        return None;
                    }
                    Some(pid.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Current `codex_model_provider` string, or empty.
pub fn codex_model_provider_id(doc: &Value) -> String {
    doc.get("codex_model_provider")
        .and_then(Value::as_str)
        .unwrap_or(CODEX_MODEL_PROVIDER_DEFAULT)
        .to_string()
}

/// Persist the Codex provider pick. None disables writing but leaves
/// `codex_model_provider` so the next config write can clear the previously
/// emitted Codex block once.
pub fn set_codex_selection(doc: &mut Value, pid: Option<&str>) {
    if let Some(obj) = doc.as_object_mut() {
        match pid {
            Some(p) if !p.is_empty() => {
                obj.insert("write_codex_config_toml".into(), Value::Bool(true));
                obj.insert("codex_model_provider".into(), Value::String(p.to_string()));
            }
            _ => {
                obj.insert("write_codex_config_toml".into(), Value::Bool(false));
            }
        }
    }
}

/// If write is on but the configured provider is missing or disabled, turn
/// write off and keep `codex_model_provider` for a one-shot cleanup.
/// Does not invent keys when already unset.
pub fn reset_codex_if_invalid(doc: &mut Value) -> bool {
    let flag = doc
        .get("write_codex_config_toml")
        .and_then(Value::as_bool)
        .unwrap_or(WRITE_CODEX_CONFIG_TOML_DEFAULT);
    if !flag {
        return false;
    }
    let pid = codex_model_provider_id(doc);
    if !pid.is_empty() && enabled_provider_ids(doc).iter().any(|e| e == &pid) {
        return false;
    }
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("write_codex_config_toml".into(), Value::Bool(false));
    }
    true
}

/// Main-menu state token: provider id, or "disabled".
pub fn codex_status_token(doc: &Value) -> String {
    let flag = doc
        .get("write_codex_config_toml")
        .and_then(Value::as_bool)
        .unwrap_or(WRITE_CODEX_CONFIG_TOML_DEFAULT);
    if !flag {
        return "disabled".to_string();
    }
    let pid = doc
        .get("codex_model_provider")
        .and_then(Value::as_str)
        .unwrap_or(CODEX_MODEL_PROVIDER_DEFAULT);
    if pid.is_empty() {
        "disabled".to_string()
    } else {
        pid.to_string()
    }
}

/// Single write path for providers.json: this is the only sort. Providers
/// A–Z by display name, models A–Z by display name. `doc` is replaced with
/// the canonical form so memory matches the file.
pub fn dump_providers(path: &Path, doc: &mut Value) -> Res<()> {
    reset_codex_if_invalid(doc);
    let empty = serde_json::Map::new();
    let obj = doc.as_object().unwrap_or(&empty);
    let mut ordered = Value::Object(order_keys(obj, &TOP_LEVEL_KEY_ORDER));
    canonicalize_providers(&mut ordered);
    dump_json(path, &ordered)?;
    *doc = ordered;
    Ok(())
}

/// `load_providers()`: providers.json with the same validation and default
/// creation as the Python tool. Order is file order; sorting happens on write.
pub fn load_providers() -> Res<Value> {
    load_providers_from(&paths::providers_path())
}

/// `load_providers` against an explicit path (tests use this to stay off the
/// shared GROK_HOME environment).
pub fn load_providers_from(path: &Path) -> Res<Value> {
    let default = serde_json::json!({ "providers": [] });
    let mut data = load_json(path, &default)?;
    {
        let obj = data.as_object_mut().unwrap();
        obj.entry("providers".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !obj["providers"].is_array() {
            return fail("providers.json: 'providers' must be a list");
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn dump_format_matches_python() {
        let v = serde_json::json!({"b": true, "list": [1, 2], "s": "x\"y", "empty": {}, "e": []});
        assert_eq!(
            dumps_pretty(&v),
            "{\n  \"b\": true,\n  \"list\": [\n    1,\n    2\n  ],\n  \"s\": \"x\\\"y\",\n  \"empty\": {},\n  \"e\": []\n}\n"
        );
    }

    #[test]
    fn order_keys_known_first_unknown_preserved() {
        let data = serde_json::json!({"zz": 1, "enabled": true, "id": "p", "models": {}, "aa": 2});
        let ordered = order_keys(data.as_object().unwrap(), &PROVIDER_KEY_ORDER);
        let keys: Vec<String> = ordered.keys().cloned().collect();
        // Canonical fields first in order, then unknown keys in original order.
        assert_eq!(keys, ["id", "enabled", "models", "zz", "aa"]);
    }

    #[test]
    fn order_provider_entry_sorts_models_by_display_name() {
        let p = serde_json::json!({
            "models": {"zeta": {"enabled": true, "name": "Zeta"},
                       "alpha": {"name": "Alpha", "enabled": false}},
            "enabled": true, "id": "p", "name": "P"
        });
        let entry = order_provider_entry(p.as_object().unwrap());
        let keys: Vec<String> = entry.keys().cloned().collect();
        assert_eq!(keys, ["id", "name", "enabled", "models"]);
        let models = entry["models"].as_object().unwrap();
        let mids: Vec<String> = models.keys().cloned().collect();
        assert_eq!(mids, ["alpha", "zeta"]);
        // Per-model canonical field order too.
        let alpha: Vec<String> = models["alpha"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(alpha, ["enabled", "name"]);
    }

    #[test]
    fn order_provider_entry_places_modalities_after_description() {
        let p = serde_json::json!({
            "id": "p",
            "name": "P",
            "enabled": true,
            "models": {
                "m": {
                    "context_window": 1000,
                    "enabled": true,
                    "name": "M",
                    "description": "d",
                    "modalities": { "input": ["text"], "output": ["text"] }
                }
            }
        });
        let entry = order_provider_entry(p.as_object().unwrap());
        let keys: Vec<String> = entry["models"]["m"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            ["enabled", "name", "description", "modalities", "context_window"]
        );
    }

    #[test]
    fn dump_providers_sorts_by_display_name_and_canonicalizes() {
        let mut doc = serde_json::json!({
            "removed_providers": ["old"],
            "providers": [
                {"id": "b", "name": "Beta", "enabled": false,
                 "extra": 7,
                 "models": {"m2": {"name": "M Two", "enabled": false}}},
                {"id": "a", "enabled": true,
                 "models": {"m1": {"enabled": true}}}
            ]
        });
        let path = std::env::temp_dir().join(format!("gm-dumpproviders-{}.json", std::process::id()));
        dump_providers(&path, &mut doc).expect("dump");
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let expected = "{\n  \"providers\": [\n    {\n      \"id\": \"a\",\n      \"enabled\": true,\n      \"models\": {\n        \"m1\": {\n          \"enabled\": true\n        }\n      }\n    },\n    {\n      \"id\": \"b\",\n      \"name\": \"Beta\",\n      \"enabled\": false,\n      \"models\": {\n        \"m2\": {\n          \"enabled\": false,\n          \"name\": \"M Two\"\n        }\n      },\n      \"extra\": 7\n    }\n  ],\n  \"removed_providers\": [\n    \"old\"\n  ]\n}\n";
        assert_eq!(out, expected);
        let ids: Vec<&str> = doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "b"], "dump must write sorted order back into doc");
    }

    #[test]
    fn load_providers_preserves_file_order() {
        let raw = "{\n  \"providers\": [\n    {\"name\": \"Zed\", \"id\": \"z\", \"models\": {}},\n    {\"id\": \"a\", \"name\": \"Ay\", \"models\": {\"m\": {\"enabled\": true}}}\n  ]\n}";
        let path = std::env::temp_dir().join(format!("gm-loadprov-{}.json", std::process::id()));
        std::fs::write(&path, raw).unwrap();
        let doc = load_providers_from(&path).expect("load");
        let _ = std::fs::remove_file(&path);
        let ids: Vec<String> = doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["z", "a"], "read must keep file order");
    }

    #[test]
    fn dump_providers_keeps_last_updated_in_canonical_order_and_does_not_invent_it() {
        let mut with_stamp = serde_json::json!({
            "removed_providers": [],
            "last_updated": "08-26-2026 03:15 PM",
            "last_synced": "08-26-2026 04:20 PM",
            "include_descriptions": true,
            "providers": []
        });
        let path = std::env::temp_dir().join(format!("gm-dump-lastupd-{}.json", std::process::id()));
        dump_providers(&path, &mut with_stamp).expect("dump");
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let keys: Vec<&str> = with_stamp.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "include_descriptions",
                "last_updated",
                "last_synced",
                "providers",
                "removed_providers",
            ]
        );
        assert!(out.contains("\"last_updated\": \"08-26-2026 03:15 PM\""));
        assert!(out.contains("\"last_synced\": \"08-26-2026 04:20 PM\""));

        let mut with_codex = serde_json::json!({
            "providers": [],
            "write_codex_config_toml": true,
            "include_descriptions": false,
        });
        let path = std::env::temp_dir().join(format!("gm-dump-codexflag-{}.json", std::process::id()));
        dump_providers(&path, &mut with_codex).expect("dump");
        let _ = std::fs::remove_file(&path);
        let keys: Vec<&str> = with_codex.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "include_descriptions",
                "write_codex_config_toml",
                "providers"
            ]
        );
        assert_eq!(with_codex["write_codex_config_toml"], Value::Bool(false));
        assert!(with_codex.get("codex_model_provider").is_none());

        let mut without = serde_json::json!({ "providers": [] });
        let path = std::env::temp_dir().join(format!("gm-dump-nolastupd-{}.json", std::process::id()));
        dump_providers(&path, &mut without).expect("dump");
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            !out.contains("last_updated"),
            "dump must not invent last_updated: {out}"
        );
        assert!(
            !out.contains("last_synced"),
            "dump must not invent last_synced: {out}"
        );
    }

    fn sample_providers() -> Value {
        serde_json::json!({
            "providers": [
                {
                    "id": "openrouter",
                    "name": "OpenRouter",
                    "enabled": true,
                    "models": { "openrouter/free": { "enabled": true, "name": "Free" } }
                },
                {
                    "id": "ollama-cloud",
                    "name": "Ollama Cloud",
                    "enabled": true,
                    "models": { "gemma4:31b": { "enabled": true, "name": "Gemma" } }
                }
            ]
        })
    }

    #[test]
    fn set_codex_selection_writes_flag_and_provider() {
        let mut doc = sample_providers();
        set_codex_selection(&mut doc, Some("openrouter"));
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(true));
        assert_eq!(doc["codex_model_provider"], "openrouter");
        set_codex_selection(&mut doc, None);
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(doc["codex_model_provider"], "openrouter");
        assert_eq!(codex_status_token(&doc), "disabled");
    }

    #[test]
    fn dump_resets_codex_when_provider_disabled() {
        let mut doc = sample_providers();
        set_codex_selection(&mut doc, Some("openrouter"));
        doc["providers"][0]["enabled"] = Value::Bool(false);
        let path = std::env::temp_dir().join(format!("gm-codex-reset-dis-{}.json", std::process::id()));
        dump_providers(&path, &mut doc).expect("dump");
        let _ = std::fs::remove_file(&path);
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(doc["codex_model_provider"], "openrouter");
    }

    #[test]
    fn dump_resets_codex_when_provider_deleted() {
        let mut doc = sample_providers();
        set_codex_selection(&mut doc, Some("openrouter"));
        doc["providers"].as_array_mut().unwrap().remove(0);
        let path = std::env::temp_dir().join(format!("gm-codex-reset-del-{}.json", std::process::id()));
        dump_providers(&path, &mut doc).expect("dump");
        let _ = std::fs::remove_file(&path);
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(doc["codex_model_provider"], "openrouter");
    }

    #[test]
    fn dump_keeps_codex_when_provider_still_enabled() {
        let mut doc = sample_providers();
        set_codex_selection(&mut doc, Some("ollama-cloud"));
        let path = std::env::temp_dir().join(format!("gm-codex-keep-{}.json", std::process::id()));
        dump_providers(&path, &mut doc).expect("dump");
        let _ = std::fs::remove_file(&path);
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(true));
        assert_eq!(doc["codex_model_provider"], "ollama-cloud");
        assert_eq!(codex_status_token(&doc), "ollama-cloud");
    }

    #[test]
    fn dump_does_not_invent_codex_keys() {
        let mut doc = serde_json::json!({ "providers": [] });
        let path = std::env::temp_dir().join(format!("gm-codex-noinvent-{}.json", std::process::id()));
        dump_providers(&path, &mut doc).expect("dump");
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!out.contains("write_codex_config_toml"), "{out}");
        assert!(!out.contains("codex_model_provider"), "{out}");
    }
}

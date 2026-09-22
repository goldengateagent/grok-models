//! Codex outputs: config.toml tables and Codex model catalog JSON.

use crate::Res;
use crate::core;
use crate::env::paths;
use crate::jsonio;
use crate::toml_out;
use serde_json::{Map, Value};
use std::collections::HashSet;
/// Renders a TOML double-quoted string.
fn toml_quoted(s: &str) -> String {
    toml_out::toml_escape(&Value::String(s.to_string())).unwrap_or_default()
}

/// Renders a TOML key, quoting when needed.
fn toml_key(ident: &str) -> String {
    toml_out::toml_subkey(ident)
}

/// True for root-level keys the Codex writer owns.
fn is_codex_managed_key(stripped: &str) -> bool {
    stripped.starts_with("model =")
        || stripped.starts_with("model_provider =")
        || stripped.starts_with("model_catalog_json =")
}

/// Provider ids whose Codex sections this writer owns.
fn codex_owned_provider_ids(doc: &Value, extra_pid: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(arr) = doc.get("providers").and_then(Value::as_array) {
        for p in arr {
            if let Some(pid) = p.get("id").and_then(Value::as_str) {
                if !pid.is_empty() && !out.contains(&pid.to_string()) {
                    out.push(pid.to_string());
                }
            }
        }
    }
    if !extra_pid.is_empty() && !out.contains(&extra_pid.to_string()) {
        out.push(extra_pid.to_string());
    }
    out
}

/// Drops owned provider sections and managed root keys, keeping user text.
fn strip_codex_managed_sections(text: &str, provider_ids: &[String]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let owned: HashSet<&str> = provider_ids.iter().map(String::as_str).collect();
    let mut out = String::new();
    let mut in_root = true;
    let mut skip = false;
    for line in text.split_inclusive('\n') {
        let stripped = line.trim();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            in_root = false;
            skip = false;
            let header = stripped
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            if let Some(rest) = header.strip_prefix("model_providers.") {
                let pid = rest.trim().trim_matches('"');
                if owned.contains(pid) {
                    skip = true;
                    continue;
                }
            }
        }
        if skip {
            continue;
        }
        if in_root && is_codex_managed_key(stripped) {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Renders a sorted inline TOML table with quoted keys and values.
fn toml_inline_table(obj: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    let items: Vec<String> = keys
        .iter()
        .map(|k| {
            let v = obj.get(*k).and_then(Value::as_str).unwrap_or_default();
            format!("{} = {}", toml_quoted(k), toml_quoted(v))
        })
        .collect();
    format!("{{ {} }}", items.join(", "))
}

/// Table fields for one Codex provider block.
fn codex_provider_fields(provider: &Map<String, Value>, pid: &str) -> Map<String, Value> {
    let pname = provider
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(pid);
    let mut fields = Map::new();
    fields.insert("name".into(), Value::String(pname.to_string()));
    fields.insert(
        "base_url".into(),
        Value::String(
            provider
                .get("base_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    );
    fields.insert(
        "env_key".into(),
        Value::String(core::provider_env_key_from_json(provider)),
    );
    for header_key in ["extra_headers", "env_http_headers"] {
        if let Some(obj) = provider.get(header_key).and_then(Value::as_object) {
            if !obj.is_empty() {
                fields.insert(header_key.into(), Value::Object(obj.clone()));
            }
        }
    }
    fields
}

/// Renders one `[model_providers.<id>]` table.
fn emit_codex_provider_table(pid: &str, fields: &Map<String, Value>) -> String {
    let name = fields.get("name").and_then(Value::as_str).unwrap_or(pid);
    let base_url = fields.get("base_url").and_then(Value::as_str).unwrap_or("");
    let env_key = fields.get("env_key").and_then(Value::as_str).unwrap_or("");
    let wire_api = "responses";
    let mut out = format!(
        "[model_providers.{}]\nname = {}\nbase_url = {}\nenv_key = {}\nwire_api = {}\n",
        toml_key(pid),
        toml_quoted(name),
        toml_quoted(base_url),
        toml_quoted(env_key),
        toml_quoted(wire_api),
    );
    if let Some(obj) = fields.get("extra_headers").and_then(Value::as_object) {
        if !obj.is_empty() {
            out.push_str(&format!("http_headers = {}\n", toml_inline_table(obj)));
        }
    }
    if let Some(obj) = fields.get("env_http_headers").and_then(Value::as_object) {
        if !obj.is_empty() {
            out.push_str(&format!("env_http_headers = {}\n", toml_inline_table(obj)));
        }
    }
    out
}

fn first_enabled_model_id(provider: &Map<String, Value>) -> Option<String> {
    let models = provider.get("models")?.as_object()?;
    for (mid, m) in models {
        let enabled = m
            .as_object()
            .and_then(|o| o.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if enabled {
            return Some(mid.clone());
        }
    }
    None
}

/// Model context window as an integer, defaulting to 128000.
fn context_window_int(entry: &Map<String, Value>) -> i64 {
    match entry.get("context_window") {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .unwrap_or(128000),
        Some(Value::String(s)) => s.parse().unwrap_or(128000),
        _ => 128000,
    }
}

/// Reasoning effort rows plus the default level from a stored model.
fn catalog_reasoning_levels(entry: &Map<String, Value>) -> (Vec<Value>, Option<String>) {
    let mut levels = Vec::new();
    let mut default = None;
    if let Some(arr) = entry.get("reasoning_efforts").and_then(Value::as_array) {
        for item in arr {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let effort = obj
                .get("value")
                .or_else(|| obj.get("effort"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if effort.is_empty() {
                continue;
            }
            let desc = obj
                .get("label")
                .or_else(|| obj.get("description"))
                .and_then(Value::as_str)
                .unwrap_or(effort);
            levels.push(serde_json::json!({
                "effort": effort,
                "description": desc,
            }));
            if obj.get("default").and_then(Value::as_bool).unwrap_or(false) {
                default = Some(effort.to_string());
            }
        }
    }
    if let Some(stored) = entry.get("reasoning_effort").and_then(Value::as_str) {
        if !stored.is_empty() {
            default = Some(stored.to_string());
        }
    }
    (levels, default)
}

const CODEX_INPUT_MODALITY_VALUES: [&str; 3] = ["text", "image", "audio"];
/// Codex-allowed input modalities from a stored providers.json model.
fn codex_input_modalities(entry: &Map<String, Value>) -> Vec<String> {
    let Some(raw) = entry
        .get("modalities")
        .and_then(Value::as_object)
        .and_then(|m| m.get("input"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in raw {
        let Some(s) = item.as_str() else {
            continue;
        };
        if CODEX_INPUT_MODALITY_VALUES.contains(&s) && !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    }
    out
}

/// Builds the Codex model catalog JSON for one provider.
fn emit_codex_model_catalog(provider: &Map<String, Value>) -> Value {
    let mut models_out = Vec::new();
    let empty = Map::new();
    let models = provider
        .get("models")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (i, (mid, m)) in models.iter().enumerate() {
        let entry = m.as_object().cloned().unwrap_or_default();
        if !entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            continue;
        }
        let display = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(mid);
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let ctx = context_window_int(&entry);
        let (levels, default_level) = catalog_reasoning_levels(&entry);
        let mut item = serde_json::json!({
            "slug": mid,
            "display_name": display,
            "description": description,
            "context_window": ctx,
            "max_context_window": ctx,
            "supported_reasoning_levels": levels,
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": i,
            "base_instructions": "",
            "supports_reasoning_summaries": crate::json_utils::is_truthy(entry.get("supports_reasoning_effort")),
            "default_reasoning_summary": "none",
            "support_verbosity": false,
            "truncation_policy": { "mode": "tokens", "limit": 10000 },
            "effective_context_window_percent": 95,
            "experimental_supported_tools": [],
        });
        let input_modalities = codex_input_modalities(&entry);
        if !input_modalities.is_empty() {
            item.as_object_mut().unwrap().insert(
                "input_modalities".into(),
                Value::Array(input_modalities.into_iter().map(Value::String).collect()),
            );
        }
        if let Some(def) = default_level {
            item.as_object_mut()
                .unwrap()
                .insert("default_reasoning_level".into(), Value::String(def));
        }
        models_out.push(item);
    }
    serde_json::json!({ "models": models_out })
}

/// Writes the Codex model catalog JSON file.
fn write_codex_model_catalog(
    provider_id: &str,
    provider: &Map<String, Value>,
) -> Res<std::path::PathBuf> {
    let path = paths::codex_models_json_path(provider_id);
    let payload = emit_codex_model_catalog(provider);
    jsonio::dump_json(&path, &payload)?;
    Ok(path)
}

/// Removes the Codex model catalog JSON file when present.
fn remove_codex_model_catalog(provider_id: &str) -> Res<()> {
    if provider_id.is_empty() {
        return Ok(());
    }
    let path = paths::codex_models_json_path(provider_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::Error::new(format!(
            "failed to remove {}: {e}",
            path.display()
        ))),
    }
}

/// Sibling of `write_config_toml`: emit one Codex provider block at the top
/// of `$CODEX_HOME/config.toml`, plus the matching model catalog JSON.
///
/// Called when write is on, or once after disable/delete while
/// `codex_model_provider` is still set. That field is the Codex-side
/// memory of which table to clear; `removed_providers` is Grok-only.
pub fn codex_config_out(
    doc: &mut Value,
    _provider_ids: &[String],
    _tables: &[(String, Map<String, Value>)],
    _removed_keys: &HashSet<String>,
) -> Res<std::path::PathBuf> {
    let flag = doc
        .get("write_codex_config_toml")
        .and_then(Value::as_bool)
        .unwrap_or(crate::jsonio::WRITE_CODEX_CONFIG_TOML_DEFAULT);
    let mut pid = jsonio::codex_model_provider_id(doc);
    let remembered = pid.clone();
    // One-shot cleanup after disable or delete of the Codex provider:
    // drop the remembered provider, then strip the old block.
    if !flag && !pid.is_empty() {
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("codex_model_provider".into(), Value::String(String::new()));
        }
        jsonio::dump_providers(&paths::providers_path(), doc)?;
        pid.clear();
    }
    let owned = codex_owned_provider_ids(doc, &remembered);
    let path = paths::codex_config_toml_path();
    let provider = if pid.is_empty() {
        None
    } else {
        core::find_provider_by_id(doc, &pid)
    };
    let first_mid = provider.as_ref().and_then(first_enabled_model_id);
    let should_emit = flag && provider.is_some() && first_mid.is_some();

    if !should_emit {
        remove_codex_model_catalog(&remembered)?;
    }

    if !path.exists() && !should_emit {
        return Ok(path);
    }

    if path.exists() {
        let bak = path.with_file_name(format!(
            "{}.bak",
            path.file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default()
        ));
        std::fs::copy(&path, &bak)
            .map_err(|e| crate::Error::new(format!("failed to write {}: {}", bak.display(), e)))?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };
    let kept = strip_codex_managed_sections(&existing, &owned)
        .trim_matches('\n')
        .to_string();

    let prefix = if should_emit {
        let pid = pid.as_str();
        let provider = provider.unwrap();
        let first_mid = first_mid.unwrap();
        write_codex_model_catalog(pid, &provider)?;
        let catalog = paths::codex_models_json_toml_value(pid);
        let fields = codex_provider_fields(&provider, pid);
        Some(format!(
            "model = {}\nmodel_provider = {}\nmodel_catalog_json = {}\n\n{}",
            toml_quoted(&first_mid),
            toml_quoted(pid),
            toml_quoted(&catalog),
            emit_codex_provider_table(pid, &fields).trim_end()
        ))
    } else {
        None
    };

    let text = match (prefix.as_deref(), kept.is_empty()) {
        (Some(p), false) => format!("{p}\n\n{kept}\n"),
        (Some(p), true) => format!("{p}\n"),
        (None, false) => format!("{kept}\n"),
        (None, true) => String::new(),
    };
    if !text.is_empty() {
        toml_out::validate_toml_text(&text)?;
    }
    jsonio::atomic_write(&path, &text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_toml::update_config_toml;
    use crate::env::paths;
    use crate::env::test_support::{regex_lite_stamp, two_provider_doc};
    use crate::jsonio;
    use serde_json::Value;
    #[test]
    fn codex_config_toml_writes_when_flag_set_and_skips_when_false() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = two_provider_doc();
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        let stamped = jsonio::load_providers().unwrap();
        let last_synced = stamped
            .get("last_synced")
            .and_then(Value::as_str)
            .expect("last_synced");
        assert!(
            regex_lite_stamp(last_synced),
            "last_synced stamp not MM-DD-YYYY HH:MM AM/PM: {last_synced:?}"
        );
        assert!(
            !paths::codex_config_toml_path().exists(),
            "flag off must not write Codex config.toml"
        );

        jsonio::set_codex_selection(&mut doc, Some("openrouter"));
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        let text = std::fs::read_to_string(paths::codex_config_toml_path()).expect("codex toml");
        assert!(
            text.contains("[model_providers.openrouter]"),
            "missing provider table: {text}"
        );
        assert!(!text.contains("[model_providers.ollama-cloud]"), "{text}");
        assert!(text.contains("model = \"openrouter/free\""), "{text}");
        assert!(text.contains("model_provider = \"openrouter\""), "{text}");
        assert!(
            text.contains("model_catalog_json = \"$CODEX_HOME/openrouter-models.json\""),
            "{text}"
        );
        assert!(text.contains("env_key = \"OPENROUTER_API_KEY\""), "{text}");
        assert!(text.contains("wire_api = \"responses\""), "{text}");
        let catalog_path = paths::codex_models_json_path("openrouter");
        let catalog = std::fs::read_to_string(&catalog_path).expect("catalog");
        assert!(
            catalog.contains("\"slug\": \"openrouter/free\""),
            "{catalog}"
        );
    }

    #[test]
    fn codex_config_toml_only_writes_selected_provider_and_first_enabled_model() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = two_provider_doc();
        jsonio::set_codex_selection(&mut doc, Some("ollama-cloud"));
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        let text = std::fs::read_to_string(paths::codex_config_toml_path()).expect("codex toml");
        assert!(text.contains("[model_providers.ollama-cloud]"), "{text}");
        assert!(!text.contains("[model_providers.openrouter]"), "{text}");
        assert!(text.contains("model_provider = \"ollama-cloud\""), "{text}");
        // models are dump-sorted by display name: DeepSeek then Gemma
        assert!(
            text.contains("model = \"deepseek-v4-flash:preview\"")
                || text.contains("model = \"gemma4:31b\""),
            "{text}"
        );
        let catalog = std::fs::read_to_string(paths::codex_models_json_path("ollama-cloud"))
            .expect("catalog");
        assert!(catalog.contains("\"slug\": \"gemma4:31b\""), "{catalog}");
        assert!(
            catalog.contains("\"slug\": \"deepseek-v4-flash:preview\""),
            "{catalog}"
        );
        assert!(!catalog.contains("openrouter/free"), "{catalog}");
    }

    #[test]
    fn codex_config_toml_skips_when_selected_provider_has_no_enabled_models() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = two_provider_doc();
        jsonio::set_codex_selection(&mut doc, Some("openrouter"));
        doc["providers"][0]["models"]["openrouter/free"]["enabled"] = Value::Bool(false);
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        // Flag stays (provider still enabled); no Codex block because no models.
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(true));
        if paths::codex_config_toml_path().exists() {
            let text = std::fs::read_to_string(paths::codex_config_toml_path()).unwrap();
            assert!(
                !text.contains("[model_providers.openrouter]"),
                "must not emit a provider table with no models: {text}"
            );
            assert!(!text.contains("model = "), "{text}");
        }
    }

    fn catalog_json_for_modalities(input: Option<Value>) -> Value {
        let mut model = serde_json::json!({
            "enabled": true,
            "name": "M"
        });
        if let Some(inp) = input {
            model.as_object_mut().unwrap().insert(
                "modalities".into(),
                serde_json::json!({ "input": inp, "output": ["text"] }),
            );
        }
        let provider = serde_json::json!({
            "id": "p",
            "models": { "m": model }
        });
        emit_codex_model_catalog(provider.as_object().unwrap())
    }

    #[test]
    fn emit_codex_model_catalog_filters_input_modalities() {
        let missing = catalog_json_for_modalities(None);
        assert!(
            missing["models"][0].get("input_modalities").is_none(),
            "missing modalities must omit input_modalities: {missing}"
        );

        let full = catalog_json_for_modalities(Some(serde_json::json!([
            "text", "image", "video", "pdf", "audio"
        ])));
        assert_eq!(
            full["models"][0]["input_modalities"],
            serde_json::json!(["text", "image", "audio"])
        );

        let image_only = catalog_json_for_modalities(Some(serde_json::json!(["image"])));
        assert_eq!(
            image_only["models"][0]["input_modalities"],
            serde_json::json!(["image"])
        );

        let ignored = catalog_json_for_modalities(Some(serde_json::json!(["video", "pdf"])));
        assert!(
            ignored["models"][0].get("input_modalities").is_none(),
            "non-Codex modalities must omit the field: {ignored}"
        );
    }

    #[test]
    fn codex_catalog_writes_filtered_input_modalities() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = serde_json::json!({
            "providers": [{
                "id": "openrouter",
                "name": "OpenRouter",
                "enabled": true,
                "env_key": "OPENROUTER_API_KEY",
                "base_url": "https://openrouter.ai/api/v1",
                "models": {
                    "vision": {
                        "name": "Vision",
                        "enabled": true,
                        "modalities": {
                            "input": ["text", "image", "video", "pdf", "audio"],
                            "output": ["text"]
                        }
                    },
                    "plain": {
                        "name": "Plain",
                        "enabled": true
                    }
                }
            }]
        });
        jsonio::set_codex_selection(&mut doc, Some("openrouter"));
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();

        let catalog_path = paths::codex_models_json_path("openrouter");
        let catalog_text = std::fs::read_to_string(&catalog_path).unwrap_or_else(|e| {
            let entries: Vec<String> = std::fs::read_dir(catalog_path.parent().unwrap())
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            panic!(
                "catalog {catalog_path:?}: {e}; CODEX_HOME={:?}; siblings={entries:?}",
                std::env::var(crate::env::vars::CODEX_HOME_ENV)
            );
        });
        let catalog: Value = serde_json::from_str(&catalog_text).expect("parse catalog");
        let models = catalog["models"].as_array().expect("models array");
        let vision = models
            .iter()
            .find(|m| m["slug"] == "vision")
            .expect("vision model");
        let plain = models
            .iter()
            .find(|m| m["slug"] == "plain")
            .expect("plain model");
        assert_eq!(
            vision["input_modalities"],
            serde_json::json!(["text", "image", "audio"])
        );
        assert!(
            plain.get("input_modalities").is_none(),
            "plain model must omit input_modalities: {plain}"
        );
    }
}

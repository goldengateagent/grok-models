//! Generated config.toml files: provider tables, models, and web search.

use crate::Res;
use crate::codex_out::codex_config_out;
use crate::core;
use crate::env::paths;
use crate::grok_out;
use crate::jsonio;
use crate::providers::{last_updated_stamp, providers_list};
use crate::sync::config_warning_lines;
use serde_json::{Map, Value};
use std::collections::HashSet;
/// What a config.toml write did, for the caller to render.
pub struct UpdateConfigResponse {
    pub path: std::path::PathBuf,
    pub providers_without_base_url: Vec<String>,
}

/// Write phase (2 of 2): load providers.json from disk and render
/// config.toml from it alone — enabled providers, table fields, table
/// ownership, and pending deletions are all derived from the file.
pub fn update_config_toml() -> Res<UpdateConfigResponse> {
    let mut doc = jsonio::load_providers()?;
    let mut providers_without_base_url: Vec<String> = Vec::new();

    // Table ownership: configured providers plus remembered deletions.
    // Only entries carrying explicit provider+model ids participate;
    // nothing is ever stripped by provider id alone.
    let managed: HashSet<String> = providers_list(&doc)
        .iter()
        .filter(|p| p.is_object())
        .filter_map(|p| p.get("id").and_then(Value::as_str))
        .map(String::from)
        .collect();
    let has_pending_deletions = doc
        .get("removed_providers")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());

    let mut tables: Vec<(String, Map<String, Value>)> = Vec::new();

    let include_descriptions = doc
        .get("include_descriptions")
        .and_then(Value::as_bool)
        .unwrap_or(crate::jsonio::INCLUDE_DESCRIPTIONS_DEFAULT);

    for provider in providers_list(&doc) {
        if !provider.is_object() || provider.get("id").is_none() {
            continue;
        }
        if !provider
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            continue;
        }
        let pid = provider["id"].as_str().unwrap_or_default().to_string();
        // base_url comes straight from providers.json; empty means the
        // provider has none stored and the catalog had nothing to backfill.
        let base_url = provider
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or("");
        if base_url.is_empty() {
            providers_without_base_url.push(pid.clone());
        }
        let env_key = crate::json_utils::get_string(&provider, "env_key");
        let pname = provider
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&pid)
            .to_string();

        let default_model_entry = Value::Object(Map::new());
        let models = provider.get("models").and_then(Value::as_object).unwrap();
        for (mid, m) in models {
            let entry = m.as_object().unwrap_or_else(|| match &default_model_entry {
                Value::Object(o) => o,
                _ => unreachable!(),
            });
            let menabled = crate::json_utils::get_bool_map(entry, "enabled");
            if !menabled {
                continue;
            }
            // Assemble the table fields from stored values only. The name
            // falls back to a title-cased model id exactly like build_fields.
            let mut fields = Map::new();
            fields.insert("model".into(), Value::String(mid.clone()));
            fields.insert("base_url".into(), Value::String(base_url.to_string()));
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| core::first_letter_cap(mid));
            fields.insert("name".into(), Value::String(format!("{name} ({pname})")));
            fields.insert("env_key".into(), Value::String(env_key.clone()));
            let backend = entry
                .get("api_backend")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("chat_completions");
            fields.insert("api_backend".into(), Value::String(backend.to_string()));
            if let Some(ctx) = entry.get("context_window") {
                fields.insert("context_window".into(), ctx.clone());
            }
            for header_key in ["extra_headers", "env_http_headers"] {
                if let Some(obj) = provider.get(header_key).and_then(Value::as_object) {
                    if !obj.is_empty() {
                        fields.insert(header_key.into(), Value::Object(obj.clone()));
                    }
                }
            }
            if crate::json_utils::is_truthy(entry.get("supports_reasoning_effort")) {
                fields.insert("supports_reasoning_effort".into(), Value::Bool(true));
                if let Some(efforts) = entry.get("reasoning_efforts") {
                    fields.insert("reasoning_efforts".into(), efforts.clone());
                    // Default effort was precomputed when the entry was
                    // last updated.
                    if let Some(def) = entry.get("reasoning_effort") {
                        fields.insert("reasoning_effort".into(), def.clone());
                    }
                }
            }
            if include_descriptions {
                if let Some(desc) = crate::jsonio::catalog_description_map(entry) {
                    fields.insert("description".into(), Value::String(desc.to_string()));
                }
            }
            tables.push((core::table_model_id(&pid, mid), fields));
        }
    }

    // Deleted-provider tables are stripped here, immediately before the
    // write, using the model ids recorded at delete time — no models.dev
    // lookup, and no provider-id-prefix matching. Entries without a model
    // list cannot be targeted safely and are skipped.
    let raw_removed: Vec<Value> = doc
        .get("removed_providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let removed: Vec<(String, Vec<String>)> = raw_removed
        .iter()
        .filter_map(|r| match r.as_object() {
            Some(o) => {
                let pid = o.get("provider").and_then(Value::as_str)?.to_string();
                let models = o
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                Some((pid, models))
            }
            None => None, // entry carries no model ids: contributes nothing
        })
        .collect();
    // Exact table keys to remove from the existing config.toml, computed
    // from the model ids recorded at delete time. Entries without a model
    // list contribute nothing — nothing is ever removed by provider id alone.
    let mut removed_keys: HashSet<String> = HashSet::new();
    for (pid, models) in &removed {
        for mid in models {
            removed_keys.insert(core::table_model_id(pid, mid));
        }
    }

    let managed_ids: Vec<String> = managed.into_iter().collect();
    let web_search = jsonio::web_search_id(&doc);
    let path = grok_out::write_config_toml(
        &paths::config_toml_path(),
        &managed_ids,
        &tables,
        &removed_keys,
        &web_search,
    )?;
    if crate::jsonio::reset_codex_if_invalid(&mut doc) {
        jsonio::dump_providers(&paths::providers_path(), &mut doc)
            .map_err(|e| e.with_warnings(config_warning_lines(&providers_without_base_url)))?;
    }
    let write_codex = doc
        .get("write_codex_config_toml")
        .and_then(Value::as_bool)
        .unwrap_or(crate::jsonio::WRITE_CODEX_CONFIG_TOML_DEFAULT);
    if write_codex || !jsonio::codex_model_provider_id(&doc).is_empty() {
        codex_config_out(&mut doc, &managed_ids, &tables, &removed_keys)?;
    }

    // The deletion list has been consumed; clear it so it isn't reprocessed
    // forever, and persist that.
    if has_pending_deletions {
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("removed_providers".into(), Value::Array(Vec::new()));
        }
    }
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("last_synced".into(), Value::String(last_updated_stamp()));
    }
    jsonio::dump_providers(&paths::providers_path(), &mut doc)
        .map_err(|e| e.with_warnings(config_warning_lines(&providers_without_base_url)))?;

    Ok(UpdateConfigResponse {
        path,
        providers_without_base_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::paths;
    use crate::env::test_support::two_provider_doc;
    use crate::jsonio;
    use serde_json::Value;
    #[test]
    fn update_config_toml_uses_stored_api_backend() {
        let _homes = crate::env::test_support::TestHomes::setup();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "p",
                "name": "P",
                "enabled": true,
                "base_url": "https://example/v1",
                "models": {
                    "a": {
                        "name": "A",
                        "enabled": true,
                        "api_backend": "messages"
                    },
                    "b": { "name": "B", "enabled": true }
                }
            }]
        });
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        let text = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        let a = text
            .split("[model.p-a]")
            .nth(1)
            .and_then(|s| s.split("[model.").next())
            .expect("table p-a");
        assert!(
            a.contains("api_backend = \"messages\""),
            "stored backend missing: {text}"
        );
        let b = text
            .split("[model.p-b]")
            .nth(1)
            .and_then(|s| s.split("[model.").next())
            .expect("table p-b");
        assert!(
            b.contains("api_backend = \"chat_completions\""),
            "missing default backend: {text}"
        );
    }

    #[test]
    fn update_config_toml_resets_codex_when_provider_disabled() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = two_provider_doc();
        jsonio::set_codex_selection(&mut doc, Some("openrouter"));
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        for p in doc["providers"].as_array_mut().unwrap() {
            if p["id"] == "openrouter" {
                p["enabled"] = Value::Bool(false);
            }
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(doc["codex_model_provider"], "openrouter");
        update_config_toml().unwrap();
        let loaded = jsonio::load_providers().unwrap();
        assert_eq!(loaded["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(loaded["codex_model_provider"], "");
        if paths::codex_config_toml_path().exists() {
            let text = std::fs::read_to_string(paths::codex_config_toml_path()).unwrap();
            assert!(!text.contains("[model_providers.openrouter]"), "{text}");
            assert!(!text.contains("model = "), "{text}");
        }
    }

    #[test]
    fn disable_clears_codex_toml_once_then_leaves_user_edits() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = two_provider_doc();
        jsonio::set_codex_selection(&mut doc, Some("openrouter"));
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        let text = std::fs::read_to_string(paths::codex_config_toml_path()).expect("codex toml");
        assert!(text.contains("[model_providers.openrouter]"), "{text}");

        jsonio::set_codex_selection(&mut doc, None);
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(doc["codex_model_provider"], "openrouter");
        update_config_toml().unwrap();
        let loaded = jsonio::load_providers().unwrap();
        assert_eq!(loaded["codex_model_provider"], "");
        let cleared = std::fs::read_to_string(paths::codex_config_toml_path()).unwrap();
        assert!(
            !cleared.contains("[model_providers.openrouter]"),
            "{cleared}"
        );
        assert!(!cleared.contains("model = "), "{cleared}");
        assert!(
            !paths::codex_models_json_path("openrouter").exists(),
            "catalog json must be deleted on disable"
        );

        let manual = "# user edit after disable\napproval_policy = \"untrusted\"\n";
        std::fs::write(paths::codex_config_toml_path(), manual).unwrap();
        update_config_toml().unwrap();
        let after = std::fs::read_to_string(paths::codex_config_toml_path()).unwrap();
        assert_eq!(
            after, manual,
            "later writes must not re-enter Codex cleanup"
        );
    }

    #[test]
    fn delete_codex_provider_clears_table_and_catalog_like_disable() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let mut doc = two_provider_doc();
        jsonio::set_codex_selection(&mut doc, Some("openrouter"));
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        update_config_toml().unwrap();
        assert!(paths::codex_models_json_path("openrouter").exists());
        let with_user = format!(
            "{}\n[projects.\"/tmp/proj\"]\ntrust_level = \"trusted\"\n\n[model_providers.openai]\nname = \"OpenAI\"\n",
            std::fs::read_to_string(paths::codex_config_toml_path()).unwrap()
        );
        std::fs::write(paths::codex_config_toml_path(), with_user).unwrap();

        let providers = doc["providers"].as_array_mut().unwrap();
        providers.retain(|p| p["id"] != "openrouter");
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(doc["codex_model_provider"], "openrouter");

        update_config_toml().unwrap();
        let loaded = jsonio::load_providers().unwrap();
        assert_eq!(loaded["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(loaded["codex_model_provider"], "");
        let text = std::fs::read_to_string(paths::codex_config_toml_path()).unwrap();
        assert!(!text.contains("[model_providers.openrouter]"), "{text}");
        assert!(!text.contains("model = "), "{text}");
        assert!(text.contains("[model_providers.openai]"), "{text}");
        assert!(text.contains("trust_level = \"trusted\""), "{text}");
        assert!(
            !paths::codex_models_json_path("openrouter").exists(),
            "catalog json must be deleted with the provider table"
        );
    }
}

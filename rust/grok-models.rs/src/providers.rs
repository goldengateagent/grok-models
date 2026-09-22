//! providers.json updates: refresh, enable, delete, and add.

use crate::config_toml::{UpdateConfigResponse, update_config_toml};
use crate::core;
use crate::env::paths;
use crate::fetch::{
    ModelsDev, ModelsDevModel, ModelsDevProvider, OLLAMA_CLOUD_LOCAL_BASE_URL,
    USE_PROVIDER_MODELS_ENDPOINT, fetch_models_dev, live_fetch_error_status, provider_models_url,
    try_fetch_models_url, try_fetch_provider_models,
};
use crate::jsonio;
use crate::sync::{SyncWarning, response_warning_lines};
use crate::{Error, Res, fail};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
pub const OLLAMA_CLOUD_PROVIDER_ID: &str = "ollama-cloud";
#[derive(Default)]
pub struct UpdateProvidersResponse {
    pub providers_synced: u64,
    /// In the order the sync produced them.
    pub warnings: Vec<SyncWarning>,
}

pub fn catalog_models_map(provider: &ModelsDevProvider) -> &HashMap<String, ModelsDevModel> {
    &provider.models
}

pub fn items_from_catalog(
    catalog: &HashMap<String, ModelsDevModel>,
) -> Vec<(String, Option<String>)> {
    catalog
        .iter()
        .map(|(mid, minfo)| {
            let name = minfo.name.as_deref().and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            });
            (mid.clone(), name)
        })
        .collect()
}

pub fn catalog_name(catalog: &HashMap<String, ModelsDevModel>, mid: &str) -> Option<String> {
    catalog
        .get(mid)
        .and_then(|v| v.name.as_deref())
        .and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })
}

pub fn resolve_model_name(
    live_name: Option<&str>,
    stored_name: Option<&str>,
    catalog: &HashMap<String, ModelsDevModel>,
    mid: &str,
) -> Option<String> {
    if let Some(s) = live_name {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(s) = stored_name {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    catalog_name(catalog, mid)
}

pub fn get_api_backend(
    provider_id: &str,
    provider_npm: Option<&str>,
    model_npm: Option<&str>,
) -> &'static str {
    if matches!(provider_id, "openai" | "xai" | "meta") {
        return "responses";
    }

    let npm = model_npm
        .or(provider_npm)
        .unwrap_or("@ai-sdk/openai-compatible");

    match npm {
        "@ai-sdk/openai" => "responses",
        "@ai-sdk/anthropic" => "messages",
        _ => "chat_completions",
    }
}

pub fn write_api_backend(
    entry: &mut Map<String, Value>,
    provider_id: &str,
    provider_npm: Option<&str>,
) {
    let model_npm = entry.get("npm").and_then(Value::as_str).map(str::to_string);
    entry.insert(
        "api_backend".into(),
        Value::String(get_api_backend(provider_id, provider_npm, model_npm.as_deref()).to_string()),
    );
}

/// Fill a model's missing attributes (context window, reasoning effort
/// options) from its models.dev catalog entry. Existing values are never
/// overwritten — user-set preferences win. Catalog `modalities` and `npm`
/// are refreshed whenever the catalog carries them.
pub fn enrich_model_entry(
    entry: &mut Map<String, Value>,
    minfo: &ModelsDevModel,
    provider_id: &str,
    provider_npm: Option<&str>,
) {
    if let Some(model_npm) = minfo
        .provider
        .as_ref()
        .and_then(|p| p.npm.as_deref())
        .filter(|s| !s.is_empty())
    {
        entry.insert("npm".to_string(), Value::String(model_npm.to_string()));
    }
    write_api_backend(entry, provider_id, provider_npm);
    if let Some(mods) = minfo.modalities.clone().filter(|v| v.is_object()) {
        entry.insert("modalities".to_string(), mods);
    }
    if !entry.contains_key("context_window") {
        if let Some(ctx) = core::context_window_field(minfo.limit.as_ref()) {
            entry.insert("context_window".to_string(), ctx);
        }
    }
    if crate::json_utils::is_truthy(minfo.reasoning.as_ref()) {
        match core::efforts_from_models_dev(&minfo.reasoning_options) {
            Some(efforts) => {
                // Precompute the default effort (first row not named "none")
                // so the config.toml writer never needs the catalog to pick.
                let default_idx = efforts
                    .iter()
                    .position(|row| crate::json_utils::get_bool_map(row, "default"))
                    .unwrap_or(0);
                let default_value = efforts[default_idx]
                    .get("value")
                    .cloned()
                    .unwrap_or(Value::Null);
                for (key, value) in [
                    ("supports_reasoning_effort", Value::Bool(true)),
                    (
                        "reasoning_efforts",
                        Value::Array(efforts.into_iter().map(Value::Object).collect()),
                    ),
                    ("reasoning_effort", default_value),
                ] {
                    if !entry.contains_key(key) {
                        entry.insert(key.to_string(), value);
                    }
                }
            }
            None => {
                entry
                    .entry("supports_reasoning_effort".to_string())
                    .or_insert(Value::Bool(true));
            }
        }
    }
}

pub fn seed_models_from_items(
    items: &[(String, Option<String>)],
    catalog: &HashMap<String, ModelsDevModel>,
    provider_id: &str,
    provider_npm: Option<&str>,
) -> Map<String, Value> {
    let mut models_map = Map::new();
    for (mid, live_name) in items {
        let catalog_id = catalog_lookup_id(provider_id, mid);
        let mut entry = Map::new();
        if let Some(name) = resolve_model_name(live_name.as_deref(), None, catalog, catalog_id) {
            entry.insert("name".into(), Value::String(name));
        }
        if let Some(minfo) = catalog.get(catalog_id) {
            crate::jsonio::seed_description(&mut entry, minfo.description.as_deref());
            enrich_model_entry(&mut entry, minfo, provider_id, provider_npm);
        }
        if !entry.contains_key("api_backend") {
            write_api_backend(&mut entry, provider_id, provider_npm);
        }
        entry.insert("enabled".into(), Value::Bool(false));
        models_map.insert(mid.clone(), Value::Object(entry));
    }
    models_map
}

pub fn reconcile_models_map(
    models_map: &mut Map<String, Value>,
    items: &[(String, Option<String>)],
    catalog: &HashMap<String, ModelsDevModel>,
    provider_id: &str,
    provider_npm: Option<&str>,
) {
    let authority: HashSet<&str> = items.iter().map(|(m, _)| m.as_str()).collect();
    for (mid, live_name) in items {
        let is_new = !models_map.contains_key(mid);
        let slot = models_map
            .entry(mid.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !slot.is_object() {
            *slot = Value::Object(Map::new());
        }
        let obj = slot.as_object_mut().unwrap();
        let catalog_id = catalog_lookup_id(provider_id, mid);

        // Name: live /models wins, then the stored value, then the catalog.
        let stored = obj.get("name").and_then(Value::as_str).map(str::to_string);
        if let Some(name) =
            resolve_model_name(live_name.as_deref(), stored.as_deref(), catalog, catalog_id)
        {
            if obj.get("name") != Some(&Value::String(name.clone())) {
                obj.insert("name".into(), Value::String(name));
            }
        }

        // Fill missing attributes; refresh the description when the catalog
        // carries a different one. User-set values are never overwritten.
        if let Some(minfo) = catalog.get(catalog_id) {
            enrich_model_entry(obj, minfo, provider_id, provider_npm);
            if let Some(desc) = minfo.description.as_deref().filter(|s| !s.is_empty()) {
                if obj.get("description").and_then(Value::as_str) != Some(desc) {
                    obj.insert("description".into(), Value::String(desc.to_string()));
                }
            }
        }
        if !obj.contains_key("api_backend") {
            write_api_backend(obj, provider_id, provider_npm);
        }

        // New entries start disabled.
        if is_new {
            obj.insert("enabled".into(), Value::Bool(false));
        }
    }

    // Remove entries the authority list no longer carries.
    let stale: Vec<String> = models_map
        .keys()
        .filter(|mid| !authority.contains(mid.as_str()))
        .cloned()
        .collect();
    for mid in stale {
        models_map.remove(&mid);
    }
}

/// Stored model id for an ollama.com cloud model.
/// `name:tag` → `name:tag-cloud`; otherwise `name:cloud`.
pub fn ollama_cloud_stored_id(live_id: &str) -> String {
    if live_id.contains(':') {
        format!("{live_id}-cloud")
    } else {
        format!("{live_id}:cloud")
    }
}

/// models.dev key for a stored ollama-cloud id. Inverse of `ollama_cloud_stored_id`.
pub fn ollama_cloud_catalog_id(stored_id: &str) -> &str {
    if let Some(stem) = stored_id.strip_suffix(":cloud") {
        if !stem.contains(':') {
            return stem;
        }
    }
    if let Some(stem) = stored_id.strip_suffix("-cloud") {
        if stem.contains(':') {
            return stem;
        }
    }
    stored_id
}

pub fn catalog_lookup_id<'a>(provider_id: &str, stored_id: &'a str) -> &'a str {
    if provider_id == OLLAMA_CLOUD_PROVIDER_ID {
        ollama_cloud_catalog_id(stored_id)
    } else {
        stored_id
    }
}

pub fn is_ollama_cloud_provider(provider: &Map<String, Value>) -> bool {
    matches!(
        provider.get("id").and_then(Value::as_str),
        Some(OLLAMA_CLOUD_PROVIDER_ID)
    )
}

/// Suffix ollama.com/catalog ids, then append localhost /models that do not
/// end in `cloud`.
pub fn expand_ollama_cloud_items(
    items: Vec<(String, Option<String>)>,
    provider: &mut Map<String, Value>,
) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = items
        .into_iter()
        .map(|(mid, name)| (ollama_cloud_stored_id(&mid), name))
        .collect();
    let url = provider_models_url(OLLAMA_CLOUD_LOCAL_BASE_URL);
    let (local, _) = try_fetch_models_url(&url, "", provider);
    if let Some(local) = local {
        for (mid, name) in local {
            if !mid.ends_with("cloud") {
                let labeled = match name {
                    Some(n) if !n.is_empty() => format!("{n} (local)"),
                    _ => format!("{mid} (local)"),
                };
                out.push((mid, Some(labeled)));
            }
        }
    }
    out
}

pub fn authority_items_for_provider(
    provider_models_dev: &ModelsDevProvider,
    provider: &mut Map<String, Value>,
) -> (Vec<(String, Option<String>)>, Option<String>) {
    let base_url = core::get_json_str(provider, "base_url");
    let env_key = core::get_json_str(provider, "env_key");
    let catalog = catalog_models_map(provider_models_dev);
    let fetch_live = USE_PROVIDER_MODELS_ENDPOINT
        && (!base_url.is_empty() || is_ollama_cloud_provider(provider));
    if fetch_live {
        let (live, err) = try_fetch_provider_models(&base_url, &env_key, provider);
        if let Some(live) = live {
            return (live, None);
        }
        if let Some(ref err) = err {
            let msg = live_fetch_error_status(err);
            return (items_from_catalog(&catalog), Some(msg));
        }
        return (items_from_catalog(&catalog), err);
    }
    (items_from_catalog(&catalog), None)
}

pub fn providers_list(doc: &Value) -> Vec<Value> {
    doc.get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Local `MM-DD-YYYY HH:MM AM/PM` for providers.json `last_updated`.
pub fn last_updated_stamp() -> String {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    let hour24 = now.hour();
    let ampm = if hour24 < 12 { "AM" } else { "PM" };
    let hour12 = {
        let h = hour24 % 12;
        if h == 0 { 12 } else { h }
    };
    format!(
        "{:02}-{:02}-{} {:02}:{:02} {ampm}",
        now.month(),
        now.day(),
        now.year(),
        hour12,
        now.minute()
    )
}

/// Update phase (1 of 2): reconcile every configured provider's model list
/// in providers.json against fresh data (live /models with catalog fallback)
/// and backfill env_key/base_url. Fetches models.dev itself. Reads and
/// writes only providers.json — no config.toml involvement.
pub fn update_providers_json() -> Res<UpdateProvidersResponse> {
    let mut doc = jsonio::load_providers()?;
    let models_dev = fetch_models_dev()?;
    let mut stats = UpdateProvidersResponse::default();

    // Refresh every configured provider, enabled or not — a disabled
    // provider's stored model list must stay current so re-enabling it
    // doesn't surface stale data. (Only enabled providers reach config.toml;
    // that filter lives in update_config_toml.)
    for provider in providers_list(&doc) {
        if !provider.is_object() || provider.get("id").is_none() {
            continue;
        }
        let pid = provider["id"].as_str().unwrap_or_default().to_string();
        let Some(provider_models_dev) = models_dev.providers.get(&pid) else {
            stats.warnings.push(SyncWarning::NotInModelsDev {
                provider_id: pid.clone(),
            });
            continue;
        };

        let catalog_models = catalog_models_map(provider_models_dev);

        // Backfill provider-level fields from the catalog: env key, npm,
        // and a missing base_url (a stored non-empty base_url override wins).
        let new_env_key = provider_models_dev.env.first().cloned().unwrap_or_default();
        {
            let provider = core::find_provider_by_id_mut(&mut doc, &pid).unwrap();
            if !new_env_key.is_empty()
                && provider.get("env_key") != Some(&Value::String(new_env_key.clone()))
            {
                provider.insert("env_key".into(), Value::String(new_env_key.clone()));
            }
            if let Some(doc_url) = provider_models_dev.doc.as_deref().filter(|s| !s.is_empty()) {
                provider.insert("doc".into(), Value::String(doc_url.to_string()));
            }
            if let Some(provider_npm) = provider_models_dev.npm.as_deref().filter(|s| !s.is_empty())
            {
                provider.insert("npm".into(), Value::String(provider_npm.to_string()));
            }
            if !provider.get("models").is_some_and(Value::is_object) {
                provider.insert("models".into(), Value::Object(Map::new()));
            }
            let catalog_url = if pid == OLLAMA_CLOUD_PROVIDER_ID {
                OLLAMA_CLOUD_LOCAL_BASE_URL.to_string()
            } else {
                provider_models_dev.api.clone()
            };
            if core::get_json_str(provider, "base_url").is_empty() && !catalog_url.is_empty() {
                provider.insert("base_url".into(), Value::String(catalog_url));
            }
        }

        // Bring the stored model list in line with the authoritative one:
        // add/remove/rename entries, then update each entry's attributes
        // from the current catalog. A 401/403 on an unauthenticated
        // /models fetch sets auth_models_list on the provider.
        let (mut items, err) = {
            let provider = core::find_provider_by_id_mut(&mut doc, &pid).unwrap();
            authority_items_for_provider(&provider_models_dev, provider)
        };
        if let Some(e) = err {
            stats
                .warnings
                .push(SyncWarning::LiveFetchFailed { message: e });
        }
        if pid == OLLAMA_CLOUD_PROVIDER_ID {
            let provider = core::find_provider_by_id_mut(&mut doc, &pid).unwrap();
            items = expand_ollama_cloud_items(items, provider);
        }
        let provider = core::find_provider_by_id_mut(&mut doc, &pid).unwrap();
        let models_map = provider.get_mut("models").unwrap().as_object_mut().unwrap();
        reconcile_models_map(
            models_map,
            &items,
            catalog_models,
            &pid,
            provider_models_dev.npm.as_deref().filter(|s| !s.is_empty()),
        );
        stats.providers_synced += 1;
    }

    if let Some(obj) = doc.as_object_mut() {
        obj.insert("last_updated".into(), Value::String(last_updated_stamp()));
    }
    jsonio::dump_providers(&paths::providers_path(), &mut doc)
        .map_err(|e| e.with_warnings(response_warning_lines(&stats.warnings)))?;

    Ok(stats)
}

/// Persists one provider's enabled flag to providers.json.
pub fn set_provider_enabled(doc: &mut Value, provider_id: &str, enabled: bool) -> Res<()> {
    if let Some(arr) = doc.get_mut("providers").and_then(Value::as_array_mut) {
        if let Some(p) = arr
            .iter_mut()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(provider_id))
        {
            if let Some(obj) = p.as_object_mut() {
                obj.insert("enabled".into(), Value::Bool(enabled));
            }
        }
    }
    jsonio::dump_providers(&paths::providers_path(), doc)?;
    Ok(())
}

/// Delete one provider and flush the deletion to disk now (providers.json +
/// config.toml), then refresh `doc` so TUI labels such as last_synced match
/// the file.
pub fn delete_provider_and_flush(doc: &mut Value, provider_id: &str) -> Res<UpdateConfigResponse> {
    // Grab the enabled model ids before the entry is removed.
    let enabled = doc
        .get("providers")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|p| p.get("id").and_then(Value::as_str) == Some(provider_id))
        })
        .map(crate::core::enabled_model_ids)
        .unwrap_or_default();
    remove_provider(doc, provider_id);
    record_removed_provider(doc, provider_id, enabled);
    jsonio::dump_providers(&paths::providers_path(), doc)?;
    // Flush the deletion into config.toml now so a re-add of the same
    // provider this session can't collide with a pending deletion record.
    let written = update_config_toml()?;
    if let Ok(fresh) = jsonio::load_providers() {
        *doc = fresh;
    }
    Ok(written)
}

pub fn remove_provider(doc: &mut Value, provider_id: &str) {
    if let Some(arr) = doc.get_mut("providers").and_then(Value::as_array_mut) {
        arr.retain(|p| p.get("id").and_then(Value::as_str) != Some(provider_id));
    }
}

/// What an add did, for the caller to render.
pub struct AddProviderResponse {
    pub model_count: usize,
    pub already_present: bool,
    pub fetch_warning_url: Option<String>,
}

/// `add_provider_entry`: add provider with all models disabled and persist.
pub fn add_provider_entry(
    doc: &mut Value,
    api: &ModelsDev,
    provider_id: &str,
) -> Result<AddProviderResponse, Error> {
    let existing: Vec<String> = core::provider_entries(doc)
        .iter()
        .map(|p| p.get("id").map(id_to_string).unwrap_or_default())
        .collect();
    if existing.iter().any(|e| e == provider_id) {
        return Ok(AddProviderResponse {
            model_count: 0,
            already_present: true,
            fetch_warning_url: None,
        });
    }
    let provider_models_dev = match api.providers.get(provider_id) {
        Some(p) => p,
        _ => {
            return fail(format!(
                "provider '{}' not found in models.dev",
                provider_id
            ));
        }
    };
    let catalog = &provider_models_dev.models;
    let mut provider = Map::new();
    provider.insert("id".into(), Value::String(provider_id.to_string()));
    let name_val = provider_models_dev
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| provider_id.to_string());
    let name_val = if !name_val.is_empty() {
        Value::String(name_val)
    } else {
        Value::String(provider_id.to_string())
    };
    provider.insert("name".into(), name_val);
    let env = provider_models_dev.env.first().cloned().unwrap_or_default();
    if !env.is_empty() {
        provider.insert("env_key".into(), Value::String(env.clone()));
    }
    if let Some(doc_url) = provider_models_dev.doc.as_deref().filter(|s| !s.is_empty()) {
        provider.insert("doc".into(), Value::String(doc_url.to_string()));
    }
    if let Some(provider_npm) = provider_models_dev.npm.as_deref().filter(|s| !s.is_empty()) {
        provider.insert("npm".into(), Value::String(provider_npm.to_string()));
    }
    // Seed the provider-level base_url override from the catalog so the
    // config menu shows the configured endpoint even before any edit.
    // ollama-cloud talks to the local daemon, not the models.dev `api`.
    let api_url = if provider_id == OLLAMA_CLOUD_PROVIDER_ID {
        OLLAMA_CLOUD_LOCAL_BASE_URL.to_string()
    } else {
        provider_models_dev.api.clone()
    };
    if !api_url.is_empty() {
        provider.insert("base_url".into(), Value::String(api_url.clone()));
    }
    if provider_id.starts_with("opencode") {
        let mut extra = Map::new();
        extra.insert(
            "x-opencode-session".into(),
            Value::String(core::new_session_id()),
        );
        // User-Agent = "opencode/1.18.31"
        // extra.insert(
        //     "User-Agent".into(),
        //     Value::String("opencode/1.18.31".into()),
        // );
        provider.insert("extra_headers".into(), Value::Object(extra));
    }
    let (mut items, fetch_warning_url) =
        authority_items_for_provider(provider_models_dev, &mut provider);
    // Diagnostics produced from here on travel with any failure, so a caller
    // that only sees the error can still print them.
    let warning_lines: Vec<String> = fetch_warning_url
        .as_ref()
        .map(|url| vec![live_fetch_error_status(url)])
        .unwrap_or_default();
    if provider_id == OLLAMA_CLOUD_PROVIDER_ID {
        items = expand_ollama_cloud_items(items, &mut provider);
    }
    if items.is_empty() {
        return Err(Error::new(format!(
            "provider '{}' has no models in models.dev",
            provider_id
        ))
        .with_warnings(warning_lines));
    }
    let models_map = seed_models_from_items(
        &items,
        catalog,
        provider_id,
        provider_models_dev.npm.as_deref().filter(|s| !s.is_empty()),
    );
    let n_models = models_map.len();
    provider.insert("enabled".into(), Value::Bool(true));
    provider.insert("models".into(), Value::Object(models_map));

    doc.get_mut("providers")
        .and_then(Value::as_array_mut)
        .unwrap()
        .push(Value::Object(provider));
    jsonio::dump_providers(&paths::providers_path(), doc)
        .map_err(|e| e.with_warnings(warning_lines))?;
    Ok(AddProviderResponse {
        model_count: n_models,
        already_present: false,
        fetch_warning_url,
    })
}

pub fn id_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Records a deletion as `{ "provider": ..., "models": [...] }` so the next
/// write phase can target the provider's config.toml tables directly — no
/// models.dev lookup. `models` holds the enabled model ids at delete time.
pub fn record_removed_provider(doc: &mut Value, provider_id: &str, models: Vec<String>) {
    let obj = doc.as_object_mut().unwrap();
    let removed = obj
        .entry("removed_providers".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !removed.is_array() {
        *removed = Value::Array(Vec::new());
    }
    let arr = removed.as_array_mut().unwrap();
    if !arr.iter().any(|v| {
        v.as_object()
            .and_then(|o| o.get("provider"))
            .and_then(Value::as_str)
            == Some(provider_id)
            || v.as_str() == Some(provider_id)
    }) {
        arr.push(serde_json::json!({
            "provider": provider_id,
            "models": models,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::paths;
    use crate::env::test_support::{fixture_api, regex_lite_stamp};
    use crate::{core, jsonio};
    use serde_json::{Map, Value};
    use std::collections::HashMap;
    #[test]
    fn last_updated_stamp_is_local_12h_mm_dd_yyyy() {
        let s = last_updated_stamp();
        let re = regex_lite_stamp(&s);
        assert!(re, "last_updated stamp not MM-DD-YYYY HH:MM AM/PM: {s:?}");
    }

    #[test]
    fn ollama_cloud_stored_id_appends_cloud_suffix() {
        assert_eq!(ollama_cloud_stored_id("glm-5.3"), "glm-5.3:cloud");
        assert_eq!(ollama_cloud_stored_id("gemma4:31b"), "gemma4:31b-cloud");
        assert_eq!(ollama_cloud_stored_id("gpt-oss:20b"), "gpt-oss:20b-cloud");
    }

    #[test]
    fn ollama_cloud_catalog_id_inverts_stored_id() {
        assert_eq!(ollama_cloud_catalog_id("glm-5.3:cloud"), "glm-5.3");
        assert_eq!(ollama_cloud_catalog_id("gemma4:31b-cloud"), "gemma4:31b");
        assert_eq!(ollama_cloud_catalog_id("gpt-oss:20b-cloud"), "gpt-oss:20b");
        assert_eq!(ollama_cloud_catalog_id("local"), "local");
        assert_eq!(ollama_cloud_catalog_id("gemma4:31b"), "gemma4:31b");
    }

    #[test]
    fn seed_ollama_cloud_backfills_from_unsuffixed_catalog_id() {
        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "gemma4:31b": { "name": "Gemma 4" }
        }))
        .unwrap();
        let items = vec![
            ("gemma4:31b-cloud".into(), None),
            ("local".into(), Some("Local".into())),
        ];
        let map = seed_models_from_items(&items, &catalog, OLLAMA_CLOUD_PROVIDER_ID, None);
        assert_eq!(map["gemma4:31b-cloud"]["name"], "Gemma 4");
        assert_eq!(map["local"]["name"], "Local");
        assert_eq!(map["local"].get("description"), None);
    }

    /// Seed providers.json with one enabled provider, run run_sync against
    /// the fixture, then verify: for every enabled model, each field the
    /// generated [model.*] table carries has a matching source in the
    /// rewritten providers.json — proving phase 2 stores everything the
    /// config.toml writer needs.
    #[test]
    fn providers_json_holds_every_config_table_field() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let api = fixture_api();
        // Enable all three catalog models up front so sync reconciles them
        // through the existing-entries path, not just seeding.
        let mut doc = serde_json::json!({ "providers": [] });
        crate::sync::add_provider_entry(&mut doc, &api, "prov").expect("add provider");
        if let Some(arr) = doc.get_mut("providers").and_then(Value::as_array_mut) {
            let p = arr.first_mut().unwrap().as_object_mut().unwrap();
            p.insert("enabled".into(), Value::Bool(true));
            let models = p.get_mut("models").unwrap().as_object_mut().unwrap();
            for (_, m) in models.iter_mut() {
                m.as_object_mut()
                    .unwrap()
                    .insert("enabled".into(), Value::Bool(true));
            }
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("dump");

        update_providers_json().expect("update providers.json");
        let stored = jsonio::load_providers().expect("reload providers.json");

        let prov = stored["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "prov")
            .expect("provider present after sync");
        let models = prov["models"].as_object().unwrap();

        let include_descriptions = true;
        for (mid, minfo) in &api.providers["prov"].models {
            assert!(
                models.contains_key(mid),
                "{mid} missing from providers.json"
            );
            let entry = &models[mid];

            let fields = core::build_fields(
                mid,
                minfo,
                prov["base_url"].as_str().unwrap_or_default(),
                &core::provider_env_key_from_api(&prov),
                prov["name"].as_str().unwrap(),
                entry.get("name").and_then(Value::as_str),
                include_descriptions,
            )
            .expect("build_fields");

            for key in fields.keys() {
                match key.as_str() {
                    // Provider-level fields with homes outside the model
                    // entry (base_url/env_key on the provider, api_backend
                    // a constant); the model id itself is the map key.
                    "model" => {}
                    "base_url" | "env_key" | "api_backend" => {}
                    "name" => assert!(
                        entry.get("name").is_some_and(|v| v.is_string()),
                        "{mid}: table name has no JSON source"
                    ),
                    "description" => assert_eq!(
                        entry.get("description"),
                        fields.get("description"),
                        "{mid}: description mismatch"
                    ),
                    "context_window" => assert_eq!(
                        entry.get("context_window"),
                        fields.get("context_window"),
                        "{mid}: context_window mismatch"
                    ),
                    "supports_reasoning_effort" => assert_eq!(
                        entry.get("supports_reasoning_effort"),
                        fields.get("supports_reasoning_effort"),
                        "{mid}: supports_reasoning_effort mismatch"
                    ),
                    "reasoning_effort" => assert_eq!(
                        entry.get("reasoning_effort"),
                        fields.get("reasoning_effort"),
                        "{mid}: reasoning_effort mismatch"
                    ),
                    // Handled by the dedicated rows comparison below.
                    "reasoning_efforts" => {}
                    other => panic!("{mid}: unaccounted table field {other}"),
                }
            }
            // reasoning_efforts rows must match verbatim, including order.
            match (
                fields.get("reasoning_efforts"),
                entry.get("reasoning_efforts"),
            ) {
                (Some(tbl), Some(json)) => {
                    assert_eq!(tbl, json, "{mid}: reasoning_efforts mismatch")
                }
                (None, None) => {}
                (t, j) => {
                    panic!("{mid}: reasoning_efforts presence differs (table {t:?}, json {j:?})")
                }
            }
        }
    }

    /// Deletion flow: a provider is deleted and recorded with its enabled
    /// model ids; update_config_toml must remove exactly those tables from
    /// config.toml — including models that models.dev does not know about
    /// (the case the old lookup-based approach missed) — then clear the
    /// removed_providers list. Re-adding the provider afterwards must bring
    /// its tables back.
    #[test]
    fn delete_flow_targets_recorded_models_and_recovers_on_readd() {
        let _homes = crate::env::test_support::TestHomes::setup();

        // Catalog knows only "plain"; "live_only" simulates a model that came
        // from the provider /models endpoint.
        let mut api = fixture_api();
        api.providers
            .get_mut("prov")
            .unwrap()
            .models
            .remove("reason_no_opts");

        // Seed: provider with all catalog models enabled, synced into
        // config.toml.
        let mut doc = serde_json::json!({ "providers": [] });
        crate::sync::add_provider_entry(&mut doc, &api, "prov").expect("add");
        {
            let p = doc["providers"][0].as_object_mut().unwrap();
            p.insert("enabled".into(), Value::Bool(true));
            let models = p.get_mut("models").unwrap().as_object_mut().unwrap();
            // add_provider_entry seeds everything disabled; turn it all on.
            for (_, m) in models.iter_mut() {
                m.as_object_mut()
                    .unwrap()
                    .insert("enabled".into(), Value::Bool(true));
            }
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("seed");
        update_providers_json().expect("initial update");
        update_config_toml().expect("initial write");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(
            config.contains("[model.prov-plain]"),
            "initial sync missing prov-plain"
        );

        // Simulate a model that an earlier /models-based sync put into
        // config.toml but which is gone from every current source: stored
        // entry + table text, no catalog backing.
        {
            let p = doc["providers"][0].as_object_mut().unwrap();
            let models = p.get_mut("models").unwrap().as_object_mut().unwrap();
            models.insert(
                "live_only".into(),
                serde_json::json!({ "name": "Live Only", "enabled": true }),
            );
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("dump live_only");
        {
            let mut config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
            config.push_str("\n[model.prov-live_only]\nmodel = \"live_only\"\n");
            std::fs::write(paths::config_toml_path(), config).expect("append live_only table");
        }

        // Delete the provider: entry gone, deletion recorded with its model ids.
        let mut doc = jsonio::load_providers().expect("reload");
        let enabled = core::enabled_model_ids(&doc["providers"][0]);
        let enabled_set: std::collections::HashSet<String> = enabled.iter().cloned().collect();
        let expected: std::collections::HashSet<String> = [
            "full".to_string(),
            "plain".to_string(),
            "live_only".to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(enabled_set, expected);
        // Simulate the TUI delete: drop the entry from the providers array.
        if let Some(arr) = doc.get_mut("providers").and_then(Value::as_array_mut) {
            arr.retain(|p| p.get("id").and_then(Value::as_str) != Some("prov"));
        }
        crate::sync::record_removed_provider(&mut doc, "prov", enabled);
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("dump post-delete");

        // Flush phase 2 alone (what the TUI does on confirm).
        update_config_toml().expect("flush delete");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(
            !config.contains("[model.prov-plain]"),
            "known-model table must be removed"
        );
        assert!(
            !config.contains("[model.prov-live_only]"),
            "/models-only table must be removed"
        );

        // The list is consumed after use.
        let stored = jsonio::load_providers().expect("reload");
        assert_eq!(
            stored.get("removed_providers").and_then(Value::as_array),
            Some(&Vec::new()),
            "removed_providers must be cleared after cleanup"
        );

        // Re-add in-session: tables come back on next sync.
        let mut doc = jsonio::load_providers().expect("reload");
        crate::sync::add_provider_entry(&mut doc, &api, "prov").expect("re-add");
        {
            let p = doc["providers"][0].as_object_mut().unwrap();
            p.insert("enabled".into(), Value::Bool(true));
            let models = p.get_mut("models").unwrap().as_object_mut().unwrap();
            for (_, m) in models.iter_mut() {
                m.as_object_mut()
                    .unwrap()
                    .insert("enabled".into(), Value::Bool(true));
            }
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("dump re-add");
        update_providers_json().expect("re-add update");
        update_config_toml().expect("re-add write");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(
            config.contains("[model.prov-plain]"),
            "re-added provider's tables must return"
        );
    }

    #[test]
    fn seed_models_from_items_copies_catalog_modalities() {
        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "vision": {
                "name": "Vision",
                "modalities": {
                    "input": ["text", "image", "video", "pdf", "audio"],
                    "output": ["text"]
                }
            },
            "plain": { "name": "Plain" }
        }))
        .unwrap();
        let items = vec![
            ("vision".to_string(), Some("Vision".to_string())),
            ("plain".to_string(), Some("Plain".to_string())),
        ];
        let seeded = seed_models_from_items(&items, &catalog, "prov", None);
        assert_eq!(
            seeded["vision"]["modalities"],
            serde_json::json!({
                "input": ["text", "image", "video", "pdf", "audio"],
                "output": ["text"]
            })
        );
        assert!(
            seeded["plain"].get("modalities").is_none(),
            "models without catalog modalities must not gain the field"
        );
    }

    #[test]
    fn seed_models_from_items_copies_catalog_npm() {
        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "sdk": {
                "name": "Sdk",
                "provider": { "npm": "@ai-sdk/openai" }
            },
            "empty": {
                "name": "Empty",
                "provider": { "npm": "" }
            },
            "plain": { "name": "Plain" }
        }))
        .unwrap();
        let items = vec![
            ("sdk".to_string(), Some("Sdk".to_string())),
            ("empty".to_string(), Some("Empty".to_string())),
            ("plain".to_string(), Some("Plain".to_string())),
        ];
        let seeded = seed_models_from_items(&items, &catalog, "prov", None);
        assert_eq!(seeded["sdk"]["npm"], "@ai-sdk/openai");
        assert!(
            seeded["empty"].get("npm").is_none(),
            "empty catalog npm must not be stored"
        );
        assert!(
            seeded["plain"].get("npm").is_none(),
            "models without catalog npm must not gain the field"
        );
    }

    #[test]
    fn get_api_backend_provider_id_and_npm() {
        assert_eq!(get_api_backend("openai", None, None), "responses");
        assert_eq!(
            get_api_backend("xai", Some("@ai-sdk/anthropic"), None),
            "responses"
        );
        assert_eq!(
            get_api_backend(
                "prov",
                Some("@ai-sdk/openai-compatible"),
                Some("@ai-sdk/openai")
            ),
            "responses"
        );
        assert_eq!(
            get_api_backend("prov", Some("@ai-sdk/anthropic"), None),
            "messages"
        );
        assert_eq!(get_api_backend("prov", None, None), "chat_completions");
    }

    #[test]
    fn seed_models_from_items_writes_api_backend() {
        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "sdk": {
                "name": "Sdk",
                "provider": { "npm": "@ai-sdk/openai" }
            },
            "plain": { "name": "Plain" }
        }))
        .unwrap();
        let items = vec![
            ("sdk".to_string(), Some("Sdk".to_string())),
            ("plain".to_string(), Some("Plain".to_string())),
            ("live-only".to_string(), Some("Live".to_string())),
        ];
        let seeded =
            seed_models_from_items(&items, &catalog, "prov", Some("@ai-sdk/openai-compatible"));
        assert_eq!(seeded["sdk"]["api_backend"], "responses");
        assert_eq!(seeded["plain"]["api_backend"], "chat_completions");
        assert_eq!(seeded["live-only"]["api_backend"], "chat_completions");
    }

    #[test]
    fn reconcile_writes_api_backend_on_new_and_refreshes() {
        let mut models_map = Map::new();
        let items = vec![("m".to_string(), Some("M".to_string()))];
        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "m": {
                "name": "M",
                "provider": { "npm": "@ai-sdk/openai" }
            }
        }))
        .unwrap();
        reconcile_models_map(&mut models_map, &items, &catalog, "prov", None);
        assert_eq!(models_map["m"]["api_backend"], "responses");

        let catalog_anthropic: HashMap<String, ModelsDevModel> =
            serde_json::from_value(serde_json::json!({
                "m": {
                    "name": "M",
                    "provider": { "npm": "@ai-sdk/anthropic" }
                }
            }))
            .unwrap();
        reconcile_models_map(&mut models_map, &items, &catalog_anthropic, "prov", None);
        assert_eq!(models_map["m"]["api_backend"], "messages");
    }

    #[test]
    fn reconcile_backfills_api_backend_on_existing_without_catalog() {
        let mut models_map = match serde_json::json!({
            "live-only": {
                "enabled": true,
                "name": "Live"
            }
        }) {
            Value::Object(m) => m,
            other => panic!("expected object, got {other}"),
        };
        let items = vec![("live-only".to_string(), Some("Live".to_string()))];
        let catalog: HashMap<String, ModelsDevModel> =
            serde_json::from_value(serde_json::json!({})).unwrap();
        reconcile_models_map(
            &mut models_map,
            &items,
            &catalog,
            "prov",
            Some("@ai-sdk/openai-compatible"),
        );
        assert_eq!(models_map["live-only"]["api_backend"], "chat_completions");
    }

    #[test]
    fn reconcile_refreshes_npm_from_catalog_and_keeps_when_omitted() {
        let mut models_map = match serde_json::json!({
            "m": {
                "enabled": true,
                "name": "M",
                "npm": "@ai-sdk/openai"
            }
        }) {
            Value::Object(m) => m,
            other => panic!("expected object, got {other}"),
        };
        let items = vec![("m".to_string(), Some("M".to_string()))];

        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "m": {
                "name": "M",
                "provider": { "npm": "@ai-sdk/anthropic" }
            }
        }))
        .unwrap();
        reconcile_models_map(&mut models_map, &items, &catalog, "prov", None);
        assert_eq!(models_map["m"]["npm"], "@ai-sdk/anthropic");

        let catalog_no_npm: HashMap<String, ModelsDevModel> =
            serde_json::from_value(serde_json::json!({ "m": { "name": "M" } })).unwrap();
        reconcile_models_map(&mut models_map, &items, &catalog_no_npm, "prov", None);
        assert_eq!(
            models_map["m"]["npm"], "@ai-sdk/anthropic",
            "omitted catalog npm must not delete the stored value"
        );
    }

    #[test]
    fn reconcile_refreshes_modalities_from_catalog_and_keeps_when_omitted() {
        let mut models_map = match serde_json::json!({
            "m": {
                "enabled": true,
                "name": "M",
                "modalities": { "input": ["text"], "output": ["text"] }
            }
        }) {
            Value::Object(m) => m,
            other => panic!("expected object, got {other}"),
        };
        let items = vec![("m".to_string(), Some("M".to_string()))];

        let catalog: HashMap<String, ModelsDevModel> = serde_json::from_value(serde_json::json!({
            "m": {
                "name": "M",
                "modalities": {
                    "input": ["text", "image"],
                    "output": ["text"]
                }
            }
        }))
        .unwrap();
        reconcile_models_map(&mut models_map, &items, &catalog, "prov", None);
        assert_eq!(
            models_map["m"]["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );

        let catalog_no_mods: HashMap<String, ModelsDevModel> =
            serde_json::from_value(serde_json::json!({ "m": { "name": "M" } })).unwrap();
        reconcile_models_map(&mut models_map, &items, &catalog_no_mods, "prov", None);
        assert_eq!(
            models_map["m"]["modalities"]["input"],
            serde_json::json!(["text", "image"]),
            "omitted catalog modalities must not delete the stored value"
        );
    }

    #[test]
    fn add_provider_entry_copies_catalog_npm() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let api: ModelsDev = serde_json::from_value(serde_json::json!({
            "prov": {
                "name": "Prov",
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "m": {
                        "name": "M",
                        "provider": { "npm": "@ai-sdk/openai" }
                    }
                }
            },
            "empty": {
                "name": "Empty",
                "npm": "",
                "models": { "m": { "name": "M" } }
            }
        }))
        .unwrap();
        let mut doc = serde_json::json!({ "providers": [] });
        add_provider_entry(&mut doc, &api, "prov").expect("add provider");
        let prov = doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "prov")
            .expect("provider present");
        assert_eq!(prov["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(prov["models"]["m"]["npm"], "@ai-sdk/openai");

        add_provider_entry(&mut doc, &api, "empty").expect("add empty-npm provider");
        let empty = doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "empty")
            .expect("empty provider present");
        assert!(
            empty.get("npm").is_none(),
            "empty catalog npm must not be stored"
        );
    }

    #[test]
    fn add_provider_entry_uses_local_base_url_for_ollama_cloud() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let api: ModelsDev = serde_json::from_value(serde_json::json!({
            "ollama-cloud": {
                "name": "Ollama Cloud",
                "api": "https://ollama.com/v1",
                "env": ["OLLAMA_API_KEY"],
                "models": {
                    "gemma4:31b": { "name": "Gemma 4" },
                    "local-cloud": { "name": "Should Filter" }
                }
            }
        }))
        .unwrap();
        let mut doc = serde_json::json!({ "providers": [] });
        add_provider_entry(&mut doc, &api, "ollama-cloud").expect("add provider");
        let prov = doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "ollama-cloud")
            .expect("provider present");
        assert_eq!(prov["base_url"], OLLAMA_CLOUD_LOCAL_BASE_URL);
    }

    #[test]
    fn add_provider_entry_generates_fresh_opencode_session_header() {
        let _homes = crate::env::test_support::TestHomes::setup();

        let api: ModelsDev = serde_json::from_value(serde_json::json!({
            "opencode-go": {
                "name": "OpenCode Go",
                "api": "https://opencode.ai/v1",
                "env": ["OPENCODE_API_KEY"],
                "models": { "m": { "name": "M" } }
            }
        }))
        .unwrap();

        let mut session_ids = Vec::new();
        for _ in 0..2 {
            let mut doc = serde_json::json!({ "providers": [] });
            add_provider_entry(&mut doc, &api, "opencode-go").expect("add provider");
            let session = doc["providers"][0]["extra_headers"]["x-opencode-session"]
                .as_str()
                .expect("session header")
                .to_string();
            assert!(session.starts_with("ses_"), "{session}");
            assert!(session.ends_with("uwtb"), "{session}");
            assert_eq!(session.len(), 30, "{session}");
            session_ids.push(session);
        }
        assert_ne!(
            session_ids[0], session_ids[1],
            "each add must mint a new session"
        );
    }
}

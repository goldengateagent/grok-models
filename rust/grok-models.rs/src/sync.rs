//! `run_sync()` and its report strings, ported verbatim.

use crate::core;
use crate::jsonio;
use crate::paths;
use crate::toml_out;
use crate::{fail, Res};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

#[derive(Default)]
pub struct Stats {
    pub providers_synced: u64,
    pub models_added: u64,
    pub models_removed: u64,
    pub models_renamed: u64,
    pub models_missing: u64,
    pub providers_missing: u64,
    pub tables_written: u64,
}

/// Fetch models.dev api.json over HTTPS (ureq + rustls, 60s timeout).
pub fn fetch_models_dev() -> Res<Value> {
    fetch_json_url(MODELS_DEV_URL)
}

pub fn http_get_json(url: &str) -> Res<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("grok-models.py")
        .build();
    match agent
        .get(url)
        .set("Accept", "application/json")
        .call()
    {
        Ok(resp) => {
            let status = resp.status();
            if status != 200 {
                // Python reads the error body (first 300 chars) into the message.
                let body = resp
                    .into_string()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>();
                return fail(format!("HTTP {status} fetching {url}: {body}"));
            }
            let mut reader = resp.into_reader();
            let mut raw = Vec::new();
            reader
                .read_to_end(&mut raw)
                .map_err(|e| crate::SyncError(format!("HTTP failure fetching {url}: {e}")))?;
            let text = String::from_utf8_lossy(&raw).to_string();
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => Ok(v),
                Err(e) => fail(format!("invalid JSON from {url}: {e}")),
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp
                .into_string()
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect::<String>();
            fail(format!("HTTP {code} fetching {url}: {body}"))
        }
        Err(e) => fail(format!("HTTP failure fetching {url}: {e}")),
    }
}

fn fetch_json_url(url: &str) -> Res<Value> {
    http_get_json(url)
}

fn providers_list(doc: &Value) -> Vec<Value> {
    doc.get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `run_sync()` — reconcile providers.json with a live API payload.
///
/// `api` is injected by callers (the binary fetches from models.dev; tests
/// pass a fixture). Returns the config path written plus stats; `None` when
/// there was nothing to do ("No providers configured yet").
pub fn run_sync(api: &Value) -> Res<(Option<std::path::PathBuf>, Stats)> {
    let providers_path = paths::providers_path();
    let mut doc = jsonio::load_providers()?;
    let empty_vec = Vec::new();

    if providers_list(&doc).is_empty() {
        let removed = doc
            .get("removed_providers")
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec)
            .clone();
        if !removed.is_empty() {
            // Still strip tables left behind by deleted providers.
            let ids: Vec<String> = removed
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect();
            let _path = toml_out::write_config_toml(
                &paths::config_toml_path(),
                &ids,
                &[],
            )?;
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("removed_providers".into(), Value::Array(Vec::new()));
            }
            jsonio::dump_json(&providers_path, &doc)?;
        }
        println!("No providers configured yet. Add with --add-provider");
        return Ok((None, Stats::default()));
    }

    let api = ensure_obj(api);

    let mut stats = Stats::default();

    let all_provider_ids: Vec<String> = providers_list(&doc)
        .iter()
        .filter(|p| p.is_object())
        .filter_map(|p| p.get("id").and_then(Value::as_str))
        .map(String::from)
        .collect();
    // This tool owns [model.*] tables only for providers it has configured.
    // Deleted providers are remembered in "removed_providers" so the next
    // sync strips their leftover tables.
    let mut managed_ids: HashSet<String> = all_provider_ids.iter().cloned().collect();
    for r in doc
        .get("removed_providers")
        .and_then(Value::as_array)
        .unwrap_or(&empty_vec)
    {
        if let Some(s) = r.as_str() {
            managed_ids.insert(s.to_string());
        }
    }

    let mut tables: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut changed = false;

    for provider in providers_list(&doc) {
        if !provider.is_object() || provider.get("id").is_none() {
            continue;
        }
        if !provider.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
            continue;
        }
        let pid = provider["id"].as_str().unwrap_or_default().to_string();
        let pinfo = match api.get(&pid) {
            Some(p) if p.is_object() => p.clone(),
            _ => {
                println!(
                    "  warning: provider {} not found in models.dev; skipping",
                    core::py_repr(&pid)
                );
                stats.providers_missing += 1;
                continue;
            }
        };

        let api_models: Map<String, Value> =
            pinfo.get("models").and_then(Value::as_object).cloned().unwrap_or_default();

        let new_env_key = core::api_env_key(&pinfo);
        // Work on the entry inside the doc so mutations persist.
        {
            let prov_obj = find_provider_mut(&mut doc, &pid).unwrap();
            if !new_env_key.is_empty()
                && prov_obj.get("env_key") != Some(&Value::String(new_env_key.clone()))
            {
                prov_obj.insert("env_key".into(), Value::String(new_env_key.clone()));
                changed = true;
            }
            if !prov_obj.get("models").is_some_and(Value::is_object) {
                prov_obj.insert("models".into(), Value::Object(Map::new()));
                changed = true;
            }
        }
        let prov_obj = find_provider_mut(&mut doc, &pid).unwrap();
        let models_map = prov_obj
            .get_mut("models")
            .unwrap()
            .as_object_mut()
            .unwrap();

        // Additions / renames, in API order.
        for (mid, minfo) in &api_models {
            if !models_map.contains_key(mid) {
                let mut entry = Map::new();
                if let Some(name) = minfo.get("name") {
                    if !name.is_null() && crate::truthy(Some(name)) {
                        entry.insert("name".into(), name.clone());
                    }
                }
                entry.insert("enabled".into(), Value::Bool(false));
                models_map.insert(mid.clone(), Value::Object(entry));
                stats.models_added += 1;
                changed = true;
            } else {
                let m = models_map.get_mut(mid).unwrap();
                if m.is_object() {
                    let obj = m.as_object_mut().unwrap();
                    if let Some(api_name) = minfo.get("name") {
                        if crate::truthy(Some(api_name)) && obj.get("name") != Some(api_name) {
                            obj.insert("name".into(), api_name.clone());
                            stats.models_renamed += 1;
                            changed = true;
                        }
                    }
                }
            }
        }
        // Removals of stale entries.
        let stale: Vec<String> = models_map
            .keys()
            .filter(|mid| !api_models.contains_key(*mid))
            .cloned()
            .collect();
        for mid in stale {
            models_map.remove(&mid);
            stats.models_removed += 1;
            changed = true;
        }

        let base_url = pinfo.get("api").and_then(Value::as_str).unwrap_or("");
        let env_key = core::api_env_key(&pinfo);
        let pname = pinfo.get("name").and_then(Value::as_str).unwrap_or(&pid).to_string();
        if base_url.is_empty() {
            println!(
                "  warning: provider {} has no base URL (api) in models.dev; \
tables will have an empty base_url",
                core::py_repr(&pid)
            );
        }

        // Table emission pass, in models-map insertion order.
        for (mid, m) in models_map.iter_mut() {
            if !m.is_object() {
                let api_name = api_models
                    .get(mid)
                    .and_then(|v| v.get("name"))
                    .filter(|n| crate::truthy(Some(n)))
                    .cloned();
                let mut entry = Map::new();
                if let Some(n) = api_name {
                    entry.insert("name".into(), n);
                }
                entry.insert("enabled".into(), Value::Bool(false));
                *m = Value::Object(entry);
                changed = true;
            }
            let menabled = crate::get_bool(m, "enabled", true);
            if !menabled {
                continue;
            }
            let minfo = match api_models.get(mid) {
                Some(v) if v.is_object() => v,
                _ => {
                    println!(
                        "  warning: model {} not found in models.dev; skipping",
                        core::py_repr(mid)
                    );
                    stats.models_missing += 1;
                    continue;
                }
            };
            let fields = core::build_fields(mid, minfo, base_url, &env_key, &pname)?;
            let table_key = core::table_model_id(&pid, mid);
            tables.push((table_key, fields));
            stats.tables_written += 1;
        }
        stats.providers_synced += 1;
    }

    // Strip tables for providers deleted since the last sync.
    let removed: Vec<String> = doc
        .get("removed_providers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    if !removed.is_empty() {
        let mut removed_keys: HashSet<String> = HashSet::new();
        for pid in &removed {
            let models = api
                .get(pid)
                .and_then(Value::as_object)
                .and_then(|o| o.get("models"))
                .and_then(Value::as_object);
            if let Some(models) = models {
                for mid in models.keys() {
                    removed_keys.insert(core::table_model_id(pid, mid));
                }
            }
        }
        tables.retain(|(k, _)| !removed_keys.contains(k));
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("removed_providers".into(), Value::Array(Vec::new()));
        }
        changed = true;
    }

    if changed {
        jsonio::dump_json(&providers_path, &doc)?;
    }

    let managed: Vec<String> = managed_ids.into_iter().collect();
    let path = toml_out::write_config_toml(&paths::config_toml_path(), &managed, &tables)?;
    Ok((Some(path), stats))
}

fn ensure_obj(v: &Value) -> Value {
    if v.is_object() {
        v.clone()
    } else {
        Value::Object(Map::new())
    }
}

fn find_provider_mut<'a>(doc: &'a mut Value, pid: &str) -> Option<&'a mut Map<String, Value>> {
    doc.get_mut("providers")?
        .as_array_mut()?
        .iter_mut()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
        .and_then(Value::as_object_mut)
}

/// `print_summary`
pub fn print_summary(stats: &Stats, path: &std::path::Path) {
    println!();
    println!("Updated {}", path.display());
    println!("Sync Summary:");
    println!("  providers synced: {}", stats.providers_synced);
    println!("  models added: {}", stats.models_added);
    println!("  models removed: {}", stats.models_removed);
    println!("  models renamed: {}", stats.models_renamed);
    println!("  models missing (skipped): {}", stats.models_missing);
    println!("  providers missing (skipped): {}", stats.providers_missing);
    println!("  tables written: {}", stats.tables_written);
}

/// `print_relaunch`
pub fn print_relaunch() {
    println!("Relaunch Grok Build for model changes");
}

/// `print_env_requirements`
pub fn print_env_requirements(providers_doc: &Value) {
    let env_vars = core::enabled_provider_env_vars(providers_doc);
    if env_vars.is_empty() {
        return;
    }
    println!();
    println!("Required environment variables:");
    for env_var in &env_vars {
        println!("  {}", core::env_status_line(env_var));
    }
}

/// `print_sync_report`
pub fn print_sync_report(stats: &Stats, path: &std::path::Path, providers_doc: &Value) {
    print_summary(stats, path);
    print_env_requirements(providers_doc);
}

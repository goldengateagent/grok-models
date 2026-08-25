//! `run_sync()` and its report strings, ported verbatim.

use crate::core;
use crate::jsonio;
use crate::paths;
use crate::toml_out;
use crate::{fail, Res};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// When true, add-provider and sync take model ids from GET {base_url}/models
/// (OpenAI list). When false, the models.dev provider `models` object is the list.
pub const USE_PROVIDER_MODELS_ENDPOINT: bool = true;

#[derive(Default)]
pub struct Stats {
    pub providers_synced: u64,
    pub models_added: u64,
    pub models_removed: u64,
    pub models_renamed: u64,
    pub descriptions_updated: u64,
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

pub fn provider_models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// OpenAI-style `{ data: [{ id, name? }] }`. None if unusable/empty.
pub fn parse_openai_models_list(payload: &Value) -> Option<Vec<(String, Option<String>)>> {
    let data = payload.get("data")?.as_array()?;
    if data.is_empty() {
        return None;
    }
    let mut items = Vec::new();
    for row in data {
        let Some(obj) = row.as_object() else {
            continue;
        };
        let Some(Value::String(mid)) = obj.get("id") else {
            continue;
        };
        if mid.is_empty() {
            continue;
        }
        let name = match obj.get("name") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        items.push((mid.clone(), name));
    }
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

pub fn try_fetch_provider_models(
    base_url: &str,
    quiet: bool,
) -> Option<Vec<(String, Option<String>)>> {
    if base_url.is_empty() {
        return None;
    }
    let url = provider_models_url(base_url);
    match http_get_json(&url) {
        Err(e) => {
            if !quiet {
                println!("  warning: {}", e.0);
            }
            None
        }
        Ok(payload) => match parse_openai_models_list(&payload) {
            None => {
                if !quiet {
                    println!("  warning: no models list at {url}");
                }
                None
            }
            Some(items) => Some(items),
        },
    }
}

fn catalog_models_map(pinfo: &Value) -> Map<String, Value> {
    pinfo
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn items_from_catalog(catalog: &Map<String, Value>) -> Vec<(String, Option<String>)> {
    catalog
        .iter()
        .map(|(mid, minfo)| {
            let name = minfo.get("name").and_then(Value::as_str).and_then(|s| {
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

fn catalog_name(catalog: &Map<String, Value>, mid: &str) -> Option<String> {
    catalog
        .get(mid)
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })
}

fn resolve_model_name(
    live_name: Option<&str>,
    stored_name: Option<&str>,
    catalog: &Map<String, Value>,
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

/// Set the model's context window and reasoning effort options in its
/// providers.json entry, using the values from models.dev.
fn enrich_model_entry(entry: &mut Map<String, Value>, minfo: &Value) {
    if let Some(ctx) = core::context_window_field(minfo) {
        entry.insert("context_window".into(), ctx);
    }
    if crate::truthy(minfo.get("reasoning")) {
        match core::efforts_from_models_dev(minfo) {
            Some(efforts) => {
                // Precompute the default effort (first row not named "none")
                // so the config.toml writer never needs the catalog to pick.
                let default_idx = efforts
                    .iter()
                    .position(|row| crate::get_bool_val(&Value::Object(row.clone()), "default", false))
                    .unwrap_or(0);
                let default_value = efforts[default_idx].get("value").cloned().unwrap_or(Value::Null);
                entry.insert("supports_reasoning_effort".into(), Value::Bool(true));
                entry.insert(
                    "reasoning_efforts".into(),
                    Value::Array(efforts.into_iter().map(Value::Object).collect()),
                );
                entry.insert("reasoning_effort".into(), default_value);
            }
            None => {
                entry.insert("supports_reasoning_effort".into(), Value::Bool(true));
            }
        }
    }
}

pub fn seed_models_from_items(
    items: &[(String, Option<String>)],
    catalog: &Map<String, Value>,
) -> Map<String, Value> {
    let mut models_map = Map::new();
    for (mid, live_name) in items {
        let mut entry = Map::new();
        if let Some(name) = resolve_model_name(live_name.as_deref(), None, catalog, mid) {
            entry.insert("name".into(), Value::String(name));
        }
        if let Some(minfo) = catalog.get(mid) {
            crate::jsonio::seed_description(&mut entry, minfo);
            enrich_model_entry(&mut entry, minfo);
        }
        entry.insert("enabled".into(), Value::Bool(false));
        models_map.insert(mid.clone(), Value::Object(entry));
    }
    models_map
}

fn reconcile_models_map(
    models_map: &mut Map<String, Value>,
    items: &[(String, Option<String>)],
    catalog: &Map<String, Value>,
    stats: &mut Stats,
) -> bool {
    let authority: HashSet<&str> = items.iter().map(|(m, _)| m.as_str()).collect();
    let mut changed = false;
    for (mid, live_name) in items {
        if !models_map.contains_key(mid) {
            let mut entry = Map::new();
            if let Some(name) = resolve_model_name(live_name.as_deref(), None, catalog, mid) {
                entry.insert("name".into(), Value::String(name));
            }
            if let Some(minfo) = catalog.get(mid) {
                crate::jsonio::seed_description(&mut entry, minfo);
                enrich_model_entry(&mut entry, minfo);
            }
            entry.insert("enabled".into(), Value::Bool(false));
            models_map.insert(mid.clone(), Value::Object(entry));
            stats.models_added += 1;
            changed = true;
            continue;
        }
        let m = models_map.get_mut(mid).unwrap();
        if !m.is_object() {
            *m = Value::Object(Map::new());
            changed = true;
        }
        let obj = m.as_object_mut().unwrap();
        // Update this entry's stored attributes from the catalog. Delete
        // before writing so attributes the catalog dropped disappear too.
        for k in [
            "context_window",
            "supports_reasoning_effort",
            "reasoning_efforts",
            "reasoning_effort",
        ] {
            if obj.remove(k).is_some() {
                changed = true;
            }
        }
        if let Some(minfo) = catalog.get(mid) {
            if minfo.is_object() {
                enrich_model_entry(obj, minfo);
            }
        }
        let stored = obj.get("name").and_then(Value::as_str).map(str::to_string);
        if let Some(name) =
            resolve_model_name(live_name.as_deref(), stored.as_deref(), catalog, mid)
        {
            if obj.get("name") != Some(&Value::String(name.clone())) {
                obj.insert("name".into(), Value::String(name));
                stats.models_renamed += 1;
                changed = true;
            }
        }
    }
    let stale: Vec<String> = models_map
        .keys()
        .filter(|mid| !authority.contains(mid.as_str()))
        .cloned()
        .collect();
    for mid in stale {
        models_map.remove(&mid);
        stats.models_removed += 1;
        changed = true;
    }
    changed
}

/// Add missing / refresh changed model descriptions from the catalog
/// (`reconcile_descriptions`). Descriptions removed upstream are left
/// as-is (last known value wins).
fn reconcile_descriptions(
    models_map: &mut Map<String, Value>,
    catalog: &Map<String, Value>,
    stats: &mut Stats,
) -> bool {
    let mut changed = false;
    for (mid, m) in models_map.iter_mut() {
        let Some(desc) = catalog.get(mid).and_then(crate::jsonio::catalog_description)
        else {
            continue;
        };
        let Some(obj) = m.as_object_mut() else {
            continue;
        };
        if obj.get("description").and_then(Value::as_str) != Some(desc) {
            obj.insert("description".into(), Value::String(desc.to_string()));
            stats.descriptions_updated += 1;
            changed = true;
        }
    }
    changed
}

pub fn authority_items_for_provider(
    pinfo: &Value,
    base_url: &str,
    quiet: bool,
) -> Vec<(String, Option<String>)> {
    let catalog = catalog_models_map(pinfo);
    if USE_PROVIDER_MODELS_ENDPOINT && !base_url.is_empty() {
        if let Some(live) = try_fetch_provider_models(base_url, quiet) {
            return live;
        }
    }
    items_from_catalog(&catalog)
}

fn providers_list(doc: &Value) -> Vec<Value> {
    doc.get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn ensure_obj(v: &Value) -> Value {
    if v.is_object() {
        v.clone()
    } else {
        Value::Object(Map::new())
    }
}

/// Update phase (1 of 2): reconcile every configured provider's model list
/// in providers.json against fresh data (live /models with catalog fallback)
/// and backfill env_key/base_url. Reads and writes only providers.json —
/// no config.toml involvement.
fn update_providers_json(models_dev: &Value) -> Res<Stats> {
    let mut doc = jsonio::load_providers()?;
    let models_dev = ensure_obj(models_dev);
    let mut stats = Stats::default();
    let mut changed = false;

    // Refresh every configured provider, enabled or not — a disabled
    // provider's stored model list must stay current so re-enabling it
    // doesn't surface stale data. (Only enabled providers reach config.toml;
    // that filter lives in update_config_toml.)
    for provider in providers_list(&doc) {
        if !provider.is_object() || provider.get("id").is_none() {
            continue;
        }
        let pid = provider["id"].as_str().unwrap_or_default().to_string();
        let Some(pinfo) = models_dev.get(&pid).filter(|p| p.is_object()).cloned() else {
            println!(
                "  warning: provider {} not found in models.dev; skipping",
                core::py_repr(&pid)
            );
            stats.providers_missing += 1;
            continue;
        };

        let catalog_models = catalog_models_map(&pinfo);

        // Backfill provider-level fields from the catalog: env key and a
        // missing base_url (a stored non-empty base_url override wins).
        let new_env_key = core::api_env_key(&pinfo);
        let effective_base_url: String;
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
            let catalog_url = pinfo.get("api").and_then(Value::as_str).unwrap_or("");
            match prov_obj.get("base_url").and_then(Value::as_str) {
                Some(v) if !v.is_empty() => effective_base_url = v.to_string(),
                _ => {
                    if !catalog_url.is_empty() {
                        prov_obj.insert(
                            "base_url".into(),
                            Value::String(catalog_url.to_string()),
                        );
                        changed = true;
                    }
                    effective_base_url = catalog_url.to_string();
                }
            }
        }

        // Bring the stored model list in line with the authoritative one:
        // add/remove/rename entries, then update each entry's attributes
        // from the current catalog.
        let items = authority_items_for_provider(&pinfo, &effective_base_url, false);
        let prov_obj = find_provider_mut(&mut doc, &pid).unwrap();
        let models_map = prov_obj.get_mut("models").unwrap().as_object_mut().unwrap();
        if reconcile_models_map(models_map, &items, &catalog_models, &mut stats) {
            changed = true;
        }
        if reconcile_descriptions(models_map, &catalog_models, &mut stats) {
            changed = true;
        }
        stats.providers_synced += 1;
    }

    if changed {
        jsonio::dump_providers(&paths::providers_path(), &mut doc)?;
    }

    Ok(stats)
}

/// Write phase (2 of 2): load providers.json from disk and render
/// config.toml from it alone — enabled providers, table fields, table
/// ownership, and pending deletions are all derived from the file.
pub fn update_config_toml() -> Res<std::path::PathBuf> {
    let mut doc = jsonio::load_providers()?;

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
        if !provider.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
            continue;
        }
        let pid = provider["id"].as_str().unwrap_or_default().to_string();
        // base_url comes straight from providers.json; empty means the
        // provider has none stored and the catalog had nothing to backfill.
        let base_url = provider.get("base_url").and_then(Value::as_str).unwrap_or("");
        if base_url.is_empty() {
            println!(
                "  warning: provider {} has no base URL; \
tables will have an empty base_url",
                core::py_repr(&pid)
            );
        }
        let env_key = core::first_env_key(&provider);
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
            let menabled = crate::get_bool_val(&Value::Object(entry.clone()), "enabled", true);
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
            fields.insert(
                "name".into(),
                Value::String(format!("{name} ({pname})")),
            );
            fields.insert("env_key".into(), Value::String(env_key.clone()));
            fields.insert(
                "api_backend".into(),
                Value::String("chat_completions".into()),
            );
            if let Some(ctx) = entry.get("context_window") {
                fields.insert("context_window".into(), ctx.clone());
            }
            if crate::truthy(entry.get("supports_reasoning_effort")) {
                fields.insert(
                    "supports_reasoning_effort".into(),
                    Value::Bool(true),
                );
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
                if let Some(desc) =
                    crate::jsonio::catalog_description(&Value::Object(entry.clone()))
                {
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
            None => None, // legacy string entry: no targeted keys
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

    let path = toml_out::write_config_toml(
        &paths::config_toml_path(),
        &managed.into_iter().collect::<Vec<String>>(),
        &tables,
        &removed_keys,
    )?;

    // The deletion list has been consumed; clear it so it isn't reprocessed
    // forever, and persist that.
    if has_pending_deletions {
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("removed_providers".into(), Value::Array(Vec::new()));
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc)?;
    }

    Ok(path)
}

/// `run_sync()` — reconcile providers.json with a live API payload, then
/// rewrite config.toml from it.
pub fn run_sync(models_dev: &Value) -> Res<(Option<std::path::PathBuf>, Stats)> {
    // Phase 1: update the models in providers.json.
    let stats = update_providers_json(models_dev)?;

    // Phase 2: rewrite config.toml from providers.json.
    let path = update_config_toml()?;
    Ok((Some(path), stats))
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
    println!("  descriptions updated: {}", stats.descriptions_updated);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// These tests rewire the process-wide GROK_HOME, so they must not run
    /// concurrently with each other.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Fixture models.dev payload exercising every model info field variant:
    /// context window + reasoning efforts, reasoning without efforts, plain
    /// model, and a model missing from the catalog (live /models id only).
    fn fixture_api() -> Value {
        serde_json::json!({
            "prov": {
                "name": "Prov",
                "api": "https://api.prov.example/v1",
                "env_key": "PROV_API_KEY",
                "models": {
                    "full": {
                        "name": "Full Model",
                        "description": "A full model.",
                        "limit": { "context": 200000.0 },
                        "reasoning": true,
                        "reasoning_options": [
                            { "type": "effort",
                              "values": ["none", "low", "high"] }
                        ]
                    },
                    "reason_no_opts": {
                        "name": "Reason No Opts",
                        "reasoning": true
                    },
                    "plain": {
                        "name": "Plain Model",
                        "limit": { "context": 8192 }
                    }
                }
            }
        })
    }

    /// Seed providers.json with one enabled provider, run run_sync against
    /// the fixture, then verify: for every enabled model, each field the
    /// generated [model.*] table carries has a matching source in the
    /// rewritten providers.json — proving phase 2 stores everything the
    /// config.toml writer needs.
    #[test]
    fn providers_json_holds_every_config_table_field() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir()
            .join(format!("gm-sync-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create test GROK_HOME");
        std::env::set_var("GROK_HOME", &home);

        let api = fixture_api();
        // Enable all three catalog models up front so sync reconciles them
        // through the existing-entries path, not just seeding.
        let mut doc = serde_json::json!({ "providers": [] });
        crate::commands::add_provider_entry(&mut doc, &api, "prov", true).expect("add provider");
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

        run_sync(&api).expect("run_sync");
        let stored = jsonio::load_providers().expect("reload providers.json");

        let prov = stored["providers"]
            .as_array().unwrap().iter()
            .find(|p| p["id"] == "prov")
            .expect("provider present after sync");
        let models = prov["models"].as_object().unwrap();

        let include_descriptions = true;
        for (mid, minfo) in api["prov"]["models"].as_object().unwrap() {
            assert!(models.contains_key(mid), "{mid} missing from providers.json");
            let entry = &models[mid];

            let fields = core::build_fields(
                mid,
                minfo,
                prov["base_url"].as_str().unwrap_or_default(),
                &core::api_env_key(&prov),
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
            match (fields.get("reasoning_efforts"), entry.get("reasoning_efforts")) {
                (Some(tbl), Some(json)) => assert_eq!(tbl, json, "{mid}: reasoning_efforts mismatch"),
                (None, None) => {}
                (t, j) => panic!("{mid}: reasoning_efforts presence differs (table {t:?}, json {j:?})"),
            }
        }

        let _ = std::fs::remove_dir_all(&home);
    }

    /// Deletion flow: a provider is deleted and recorded with its enabled
    /// model ids; update_config_toml must remove exactly those tables from
    /// config.toml — including models that models.dev does not know about
    /// (the case the old lookup-based approach missed) — then clear the
    /// removed_providers list. Re-adding the provider afterwards must bring
    /// its tables back.
    #[test]
    fn delete_flow_targets_recorded_models_and_recovers_on_readd() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir()
            .join(format!("gm-delete-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create test GROK_HOME");
        std::env::set_var("GROK_HOME", &home);

        // Catalog knows only "plain"; "live_only" simulates a model that came
        // from the provider /models endpoint.
        let mut api = fixture_api();
        api["prov"]["models"]
            .as_object_mut()
            .unwrap()
            .remove("reason_no_opts");

        // Seed: provider with all catalog models enabled, synced into
        // config.toml.
        let mut doc = serde_json::json!({ "providers": [] });
        crate::commands::add_provider_entry(&mut doc, &api, "prov", true).expect("add");
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
        run_sync(&api).expect("initial sync");

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
            let mut config =
                std::fs::read_to_string(paths::config_toml_path()).expect("config");
            config.push_str("\n[model.prov-live_only]\nmodel = \"live_only\"\n");
            std::fs::write(paths::config_toml_path(), config).expect("append live_only table");
        }

        // Delete the provider: entry gone, deletion recorded with its model ids.
        let mut doc = jsonio::load_providers().expect("reload");
        let enabled = core::enabled_model_ids(&doc["providers"][0]);
        let enabled_set: std::collections::HashSet<String> =
            enabled.iter().cloned().collect();
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
        crate::fallback::record_removed_provider(&mut doc, "prov", enabled);
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("dump post-delete");

        // Flush phase 2 alone (what the TUI does on confirm).
        update_config_toml().expect("flush delete");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(!config.contains("[model.prov-plain]"), "known-model table must be removed");
        assert!(!config.contains("[model.prov-live_only]"), "/models-only table must be removed");

        // The list is consumed after use.
        let stored = jsonio::load_providers().expect("reload");
        assert_eq!(
            stored.get("removed_providers").and_then(Value::as_array),
            Some(&Vec::new()),
            "removed_providers must be cleared after cleanup"
        );

        // Re-add in-session: tables come back on next sync.
        let mut doc = jsonio::load_providers().expect("reload");
        crate::commands::add_provider_entry(&mut doc, &api, "prov", true).expect("re-add");
        {
            let p = doc["providers"][0].as_object_mut().unwrap();
            p.insert("enabled".into(), Value::Bool(true));
            let models = p.get_mut("models").unwrap().as_object_mut().unwrap();
            for (_, m) in models.iter_mut() {
                m.as_object_mut().unwrap().insert("enabled".into(), Value::Bool(true));
            }
        }
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("dump re-add");
        run_sync(&api).expect("sync after re-add");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(config.contains("[model.prov-plain]"), "re-added provider's tables must return");
    }

    /// A legacy bare-string removed_providers entry carries no model ids, so
    /// nothing may be stripped by provider id alone — the write phase leaves
    /// config.toml untouched and just consumes the entry.
    #[test]
    fn legacy_string_removal_entry_does_not_strip_without_model_ids() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir()
            .join(format!("gm-legacy-del-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create test GROK_HOME");
        std::env::set_var("GROK_HOME", &home);

        // providers.json with no providers but a legacy pending deletion;
        // a stale table left in config.toml.
        let mut doc = serde_json::json!({
            "providers": [],
            "removed_providers": ["prov"]
        });
        jsonio::dump_providers(&paths::providers_path(), &mut doc).expect("seed");
        std::fs::write(
            paths::config_toml_path(),
            "[model.prov-old]\nmodel = \"old\"\n",
        )
        .expect("write stale config");

        update_config_toml().expect("flush legacy delete");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(
            config.contains("[model.prov-old]"),
            "no model ids recorded — nothing may be stripped; config was:\n{config}"
        );
        let stored = jsonio::load_providers().expect("reload");
        assert_eq!(
            stored.get("removed_providers").and_then(Value::as_array),
            Some(&Vec::new())
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}

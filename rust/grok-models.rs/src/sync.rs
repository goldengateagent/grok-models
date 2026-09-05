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
    pub live_fetch_errors: Vec<String>,
}

const HTTP_TIMEOUT_SECS: u64 = 15;

/// Fetch models.dev api.json over HTTPS (ureq + rustls, 15s timeout).
pub fn fetch_models_dev() -> Res<Value> {
    fetch_json_url(MODELS_DEV_URL)
}

/// Value of `env_key` if that env var is set and non-empty.
pub fn env_api_key(env_key: &str) -> Option<String> {
    if env_key.is_empty() {
        return None;
    }
    match std::env::var(env_key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

pub fn http_get_json(url: &str) -> Res<Value> {
    http_get_json_with(url, None)
}

pub fn http_get_json_with(url: &str, api_key: Option<&str>) -> Res<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .timeout_connect(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent("grok-models.py")
        .build();
    let mut request = agent.get(url).set("Accept", "application/json");
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        request = request.set("Authorization", &format!("Bearer {key}"));
    }
    match request.call() {
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
        Err(e) => {
            let msg = e.to_string();
            if msg.to_ascii_lowercase().contains("timed out")
                || msg.to_ascii_lowercase().contains("timeout")
            {
                fail(format!("HTTP timeout fetching {url}"))
            } else {
                fail(format!("HTTP failure fetching {url}: {e}"))
            }
        }
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

/// TUI / CLI status when GET {base_url}/models fails.
pub fn live_fetch_error_status(detail: &str) -> String {
    if detail.is_empty() {
        return "error: fetch live model list failed".to_string();
    }
    if detail.starts_with("error ") {
        detail.to_string()
    } else {
        format!("error {detail}")
    }
}

fn is_http_auth_error(err: &crate::SyncError) -> bool {
    err.0.starts_with("HTTP 401 ") || err.0.starts_with("HTTP 403 ")
}

fn provider_auth_models_list(provider: Option<&Map<String, Value>>) -> bool {
    matches!(
        provider.and_then(|p| p.get("auth_models_list")),
        Some(Value::Bool(true))
    )
}

/// GET {base_url}/models. Returns (rows, None) or (None, Some(url)) on failure.
/// Never prints — callers decide how to surface the URL.
///
/// If the provider has `auth_models_list: true`, send Authorization: Bearer.
/// Otherwise fetch unauthenticated. On 401/403 with a usable env_key, set
/// `auth_models_list` true and retry with the key. Success leaves the flag
/// unchanged. Some public lists hang if a key is sent.
pub fn try_fetch_provider_models(
    base_url: &str,
    env_key: &str,
    provider: Option<&mut Map<String, Value>>,
) -> (Option<Vec<(String, Option<String>)>>, Option<String>) {
    if base_url.is_empty() {
        return (None, None);
    }
    let url = provider_models_url(base_url);
    let use_auth = provider_auth_models_list(provider.as_deref());
    let key = if use_auth {
        env_api_key(env_key)
    } else {
        None
    };
    let payload = match http_get_json_with(&url, key.as_deref()) {
        Ok(payload) => payload,
        Err(e) => {
            if use_auth || !is_http_auth_error(&e) {
                return (None, Some(e.0));
            }
            let Some(k) = env_api_key(env_key) else {
                return (None, Some(e.0));
            };
            if let Some(p) = provider {
                p.insert("auth_models_list".into(), Value::Bool(true));
            }
            match http_get_json_with(&url, Some(&k)) {
                Ok(payload) => payload,
                Err(retry_e) => return (None, Some(retry_e.0)),
            }
        }
    };
    match parse_openai_models_list(&payload) {
        None => (None, Some(format!("empty or invalid model list from {url}"))),
        Some(items) => (Some(items), None),
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

fn get_api_backend(provider_id: &str, provider_npm: Option<&str>, model_npm: Option<&str>) -> &'static str {
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

fn write_api_backend(
    entry: &mut Map<String, Value>,
    provider_id: &str,
    provider_npm: Option<&str>,
) {
    let model_npm = entry
        .get("npm")
        .and_then(Value::as_str)
        .map(str::to_string);
    entry.insert(
        "api_backend".into(),
        Value::String(get_api_backend(provider_id, provider_npm, model_npm.as_deref()).to_string()),
    );
}

/// Fill a model's missing attributes (context window, reasoning effort
/// options) from its models.dev catalog entry. Existing values are never
/// overwritten — user-set preferences win. Catalog `modalities` and `npm`
/// are refreshed whenever the catalog carries them.
fn enrich_model_entry(
    entry: &mut Map<String, Value>,
    minfo: &Value,
    provider_id: &str,
    provider_npm: Option<&str>,
) {
    if let Some(model_npm) = minfo.get("provider").and_then(jsonio::catalog_npm) {
        entry.insert("npm".to_string(), Value::String(model_npm.to_string()));
    }
    write_api_backend(entry, provider_id, provider_npm);
    if let Some(mods) = jsonio::catalog_modalities(minfo) {
        entry.insert("modalities".to_string(), mods);
    }
    if !entry.contains_key("context_window") {
        if let Some(ctx) = core::context_window_field(minfo) {
            entry.insert("context_window".to_string(), ctx);
        }
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
                for (key, value) in [
                    (
                        "supports_reasoning_effort",
                        Value::Bool(true),
                    ),
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
    catalog: &Map<String, Value>,
    provider_id: &str,
    provider_npm: Option<&str>,
) -> Map<String, Value> {
    let mut models_map = Map::new();
    for (mid, live_name) in items {
        let mut entry = Map::new();
        if let Some(name) = resolve_model_name(live_name.as_deref(), None, catalog, mid) {
            entry.insert("name".into(), Value::String(name));
        }
        if let Some(minfo) = catalog.get(mid) {
            crate::jsonio::seed_description(&mut entry, minfo);
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

fn reconcile_models_map(
    models_map: &mut Map<String, Value>,
    items: &[(String, Option<String>)],
    catalog: &Map<String, Value>,
    stats: &mut Stats,
    provider_id: &str,
    provider_npm: Option<&str>,
) {
    let authority: HashSet<&str> = items.iter().map(|(m, _)| m.as_str()).collect();
    for (mid, live_name) in items {
        let is_new = !models_map.contains_key(mid);
        let slot =
            models_map.entry(mid.clone()).or_insert_with(|| Value::Object(Map::new()));
        if !slot.is_object() {
            *slot = Value::Object(Map::new());
        }
        let obj = slot.as_object_mut().unwrap();

        // Name: live /models wins, then the stored value, then the catalog.
        let stored = obj.get("name").and_then(Value::as_str).map(str::to_string);
        if let Some(name) =
            resolve_model_name(live_name.as_deref(), stored.as_deref(), catalog, mid)
        {
            if obj.get("name") != Some(&Value::String(name.clone())) {
                obj.insert("name".into(), Value::String(name));
                stats.models_renamed += 1;
            }
        }

        // Fill missing attributes; refresh the description when the catalog
        // carries a different one. User-set values are never overwritten.
        if let Some(minfo) = catalog.get(mid) {
            if minfo.is_object() {
                enrich_model_entry(obj, minfo, provider_id, provider_npm);
                if let Some(desc) = crate::jsonio::catalog_description(minfo) {
                    if obj.get("description").and_then(Value::as_str) != Some(desc) {
                        obj.insert("description".into(), Value::String(desc.to_string()));
                        stats.descriptions_updated += 1;
                    }
                }
            }
        }
        if !obj.contains_key("api_backend") {
            write_api_backend(obj, provider_id, provider_npm);
        }

        // New entries start disabled.
        if is_new {
            obj.insert("enabled".into(), Value::Bool(false));
            stats.models_added += 1;
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
        stats.models_removed += 1;
    }
}

pub fn authority_items_for_provider(
    pinfo: &Value,
    base_url: &str,
    quiet: bool,
    env_key: &str,
    provider: Option<&mut Map<String, Value>>,
) -> (Vec<(String, Option<String>)>, Option<String>) {
    let catalog = catalog_models_map(pinfo);
    if USE_PROVIDER_MODELS_ENDPOINT && !base_url.is_empty() {
        let (live, err) = try_fetch_provider_models(base_url, env_key, provider);
        if let Some(live) = live {
            return (live, None);
        }
        if let Some(ref err) = err {
            let msg = live_fetch_error_status(err);
            if !quiet {
                println!("{msg}");
            }
            return (items_from_catalog(&catalog), Some(msg));
        }
        return (items_from_catalog(&catalog), err);
    }
    (items_from_catalog(&catalog), None)
}

fn providers_list(doc: &Value) -> Vec<Value> {
    doc.get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Local `MM-DD-YYYY HH:MM AM/PM` for providers.json `last_updated`.
pub fn last_updated_stamp() -> String {
    unsafe {
        let mut t: libc::time_t = 0;
        libc::time(&mut t);
        let tm = libc::localtime(&t);
        if tm.is_null() {
            return String::new();
        }
        let tm = *tm;
        let hour24 = tm.tm_hour;
        let ampm = if hour24 < 12 { "AM" } else { "PM" };
        let hour12 = {
            let h = hour24 % 12;
            if h == 0 { 12 } else { h }
        };
        format!(
            "{:02}-{:02}-{} {:02}:{:02} {ampm}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_year + 1900,
            hour12,
            tm.tm_min
        )
    }
}

/// Update phase (1 of 2): reconcile every configured provider's model list
/// in providers.json against fresh data (live /models with catalog fallback)
/// and backfill env_key/base_url. Fetches models.dev itself. Reads and
/// writes only providers.json — no config.toml involvement.
pub fn update_providers_json() -> Res<Stats> {
    update_providers_json_with(false)
}

pub fn update_providers_json_with(quiet: bool) -> Res<Stats> {
    let mut doc = jsonio::load_providers()?;
    let models_dev = fetch_models_dev()?;
    let mut stats = Stats::default();

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
            if !quiet {
                println!(
                    "  warning: provider {} not found in models.dev; skipping",
                    core::py_repr(&pid)
                );
            }
            stats.providers_missing += 1;
            continue;
        };

        let catalog_models = catalog_models_map(&pinfo);

        // Backfill provider-level fields from the catalog: env key, npm,
        // and a missing base_url (a stored non-empty base_url override wins).
        let new_env_key = core::api_env_key(&pinfo);
        let effective_base_url: String;
        let env_key: String;
        {
            let prov_obj = find_provider_mut(&mut doc, &pid).unwrap();
            if !new_env_key.is_empty()
                && prov_obj.get("env_key") != Some(&Value::String(new_env_key.clone()))
            {
                prov_obj.insert("env_key".into(), Value::String(new_env_key.clone()));
            }
            if let Some(doc_url) = jsonio::catalog_doc(&pinfo) {
                prov_obj.insert("doc".into(), Value::String(doc_url.to_string()));
            }
            if let Some(provider_npm) = jsonio::catalog_npm(&pinfo) {
                prov_obj.insert("npm".into(), Value::String(provider_npm.to_string()));
            }
            if !prov_obj.get("models").is_some_and(Value::is_object) {
                prov_obj.insert("models".into(), Value::Object(Map::new()));
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
                    }
                    effective_base_url = catalog_url.to_string();
                }
            }
            env_key = match prov_obj.get("env_key").and_then(Value::as_str) {
                Some(s) => s.to_string(),
                None => String::new(),
            };
        }

        // Bring the stored model list in line with the authoritative one:
        // add/remove/rename entries, then update each entry's attributes
        // from the current catalog. A 401/403 on an unauthenticated
        // /models fetch sets auth_models_list on the provider.
        let (items, err) = {
            let prov_obj = find_provider_mut(&mut doc, &pid).unwrap();
            authority_items_for_provider(
                &pinfo,
                &effective_base_url,
                quiet,
                &env_key,
                Some(prov_obj),
            )
        };
        if let Some(e) = err {
            stats.live_fetch_errors.push(e);
        }
        let prov_obj = find_provider_mut(&mut doc, &pid).unwrap();
        let models_map = prov_obj.get_mut("models").unwrap().as_object_mut().unwrap();
        reconcile_models_map(
            models_map,
            &items,
            &catalog_models,
            &mut stats,
            &pid,
            jsonio::catalog_npm(&pinfo),
        );
        stats.providers_synced += 1;
    }

    if let Some(obj) = doc.as_object_mut() {
        obj.insert("last_updated".into(), Value::String(last_updated_stamp()));
    }
    jsonio::dump_providers(&paths::providers_path(), &mut doc)?;

    Ok(stats)
}

fn toml_quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn toml_key(ident: &str) -> String {
    if ident
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        ident.to_string()
    } else {
        toml_quoted(ident)
    }
}

fn is_codex_managed_key(stripped: &str) -> bool {
    stripped.starts_with("model =")
        || stripped.starts_with("model_provider =")
        || stripped.starts_with("model_catalog_json =")
}

fn codex_owned_provider_ids(doc: &Value, extra_pid: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(arr) = doc.get("providers").and_then(Value::as_array) {
        for p in arr {
            if let Some(pid) = p.get("id").and_then(Value::as_str) {
                if !pid.is_empty() && !out.iter().any(|e| e == pid) {
                    out.push(pid.to_string());
                }
            }
        }
    }
    if !extra_pid.is_empty() && !out.iter().any(|e| e == extra_pid) {
        out.push(extra_pid.to_string());
    }
    out
}

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
            let header = stripped.trim_start_matches('[').trim_end_matches(']').trim();
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

fn emit_codex_provider_table(pid: &str, fields: &Map<String, Value>) -> String {
    let name = fields.get("name").and_then(Value::as_str).unwrap_or(pid);
    let base_url = fields.get("base_url").and_then(Value::as_str).unwrap_or("");
    let env_key = fields.get("env_key").and_then(Value::as_str).unwrap_or("");
    let wire_api = "responses";
    format!(
        "[model_providers.{}]\nname = {}\nbase_url = {}\nenv_key = {}\nwire_api = {}\n",
        toml_key(pid),
        toml_quoted(name),
        toml_quoted(base_url),
        toml_quoted(env_key),
        toml_quoted(wire_api),
    )
}

fn find_provider<'a>(doc: &'a Value, pid: &str) -> Option<&'a Map<String, Value>> {
    doc.get("providers")?
        .as_array()?
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
        .and_then(Value::as_object)
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

fn context_window_int(entry: &Map<String, Value>) -> i64 {
    match entry.get("context_window") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)).unwrap_or(128000),
        Some(Value::String(s)) => s.parse().unwrap_or(128000),
        _ => 128000,
    }
}

fn catalog_reasoning_levels(entry: &Map<String, Value>) -> (Vec<Value>, Option<String>) {
    let mut levels = Vec::new();
    let mut default = None;
    if let Some(arr) = entry.get("reasoning_efforts").and_then(Value::as_array) {
        for item in arr {
            let Some(obj) = item.as_object() else { continue };
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

fn emit_codex_model_catalog(provider: &Map<String, Value>) -> Value {
    let mut models_out = Vec::new();
    let empty = Map::new();
    let models = provider
        .get("models")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (i, (mid, m)) in models.iter().enumerate() {
        let entry = m.as_object().cloned().unwrap_or_default();
        if !entry.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
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
            "supports_reasoning_summaries": crate::truthy(entry.get("supports_reasoning_effort")),
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

fn write_codex_model_catalog(provider_id: &str, provider: &Map<String, Value>) -> Res<std::path::PathBuf> {
    let path = paths::codex_models_json_path(provider_id);
    let payload = emit_codex_model_catalog(provider);
    jsonio::dump_json(&path, &payload)?;
    Ok(path)
}

fn remove_codex_model_catalog(provider_id: &str) -> Res<()> {
    if provider_id.is_empty() {
        return Ok(());
    }
    let path = paths::codex_models_json_path(provider_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::SyncError(format!(
            "failed to remove {}: {e}",
            path.display()
        ))),
    }
}

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
        Value::String(core::first_env_key(&Value::Object(provider.clone()))),
    );
    fields
}

/// Sibling of `write_config_toml`: emit one Codex provider block at the top
/// of `$CODEX_HOME/config.toml`, plus the matching model catalog JSON.
///
/// Called when write is on, or once after disable/delete while
/// `codex_model_provider` is still set. That field is the Codex-side
/// memory of which table to clear; `removed_providers` is Grok-only.
pub fn codex_config_toml(
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
        find_provider(doc, &pid)
    };
    let first_mid = provider.and_then(first_enabled_model_id);
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
            path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default()
        ));
        std::fs::copy(&path, &bak).map_err(|e| {
            crate::SyncError(format!("failed to write {}: {}", bak.display(), e))
        })?;
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
        write_codex_model_catalog(pid, provider)?;
        let catalog = paths::codex_models_json_toml_value(pid);
        let fields = codex_provider_fields(provider, pid);
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

/// Write phase (2 of 2): load providers.json from disk and render
/// config.toml from it alone — enabled providers, table fields, table
/// ownership, and pending deletions are all derived from the file.
pub fn update_config_toml() -> Res<std::path::PathBuf> {
    update_config_toml_with(false)
}

pub fn update_config_toml_with(quiet: bool) -> Res<std::path::PathBuf> {
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
        if base_url.is_empty() && !quiet {
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
            let backend = entry
                .get("api_backend")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("chat_completions");
            fields.insert(
                "api_backend".into(),
                Value::String(backend.to_string()),
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
    let path = toml_out::write_config_toml(
        &paths::config_toml_path(),
        &managed_ids,
        &tables,
        &removed_keys,
    )?;
    if crate::jsonio::reset_codex_if_invalid(&mut doc) {
        jsonio::dump_providers(&paths::providers_path(), &mut doc)?;
    }
    let write_codex = doc
        .get("write_codex_config_toml")
        .and_then(Value::as_bool)
        .unwrap_or(crate::jsonio::WRITE_CODEX_CONFIG_TOML_DEFAULT);
    if write_codex || !jsonio::codex_model_provider_id(&doc).is_empty() {
        codex_config_toml(&mut doc, &managed_ids, &tables, &removed_keys)?;
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
    jsonio::dump_providers(&paths::providers_path(), &mut doc)?;

    Ok(path)
}

/// `run_sync()` — reconcile providers.json with a live API payload, then
/// rewrite config.toml from it.
pub fn run_sync() -> Res<(Option<std::path::PathBuf>, Stats)> {
    // Phase 1: update the models in providers.json.
    let stats = update_providers_json()?;

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
    use crate::test_support::grok_home_lock;

    #[test]
    fn last_updated_stamp_is_local_12h_mm_dd_yyyy() {
        let s = last_updated_stamp();
        let re = regex_lite_stamp(&s);
        assert!(re, "last_updated stamp not MM-DD-YYYY HH:MM AM/PM: {s:?}");
    }

    #[test]
    fn live_fetch_error_status_wraps_http_detail() {
        assert_eq!(
            live_fetch_error_status("HTTP timeout fetching https://api.example/v1/models"),
            "error HTTP timeout fetching https://api.example/v1/models"
        );
        assert_eq!(
            live_fetch_error_status("error already wrapped"),
            "error already wrapped"
        );
    }

    #[test]
    fn provider_auth_models_list_only_true() {
        let mut p = Map::new();
        assert!(!provider_auth_models_list(Some(&p)));
        p.insert("auth_models_list".into(), Value::Bool(false));
        assert!(!provider_auth_models_list(Some(&p)));
        p.insert("auth_models_list".into(), Value::Bool(true));
        assert!(provider_auth_models_list(Some(&p)));
        assert!(!provider_auth_models_list(None));
    }

    #[test]
    fn is_http_auth_error_matches_401_403_only() {
        assert!(is_http_auth_error(&crate::SyncError(
            "HTTP 401 fetching https://example/models: no".into()
        )));
        assert!(is_http_auth_error(&crate::SyncError(
            "HTTP 403 fetching https://example/models: no".into()
        )));
        assert!(!is_http_auth_error(&crate::SyncError(
            "HTTP 429 fetching https://example/models: slow".into()
        )));
        assert!(!is_http_auth_error(&crate::SyncError(
            "HTTP failure fetching https://example/models: timeout".into()
        )));
    }

    #[test]
    fn env_api_key_reads_set_var() {
        let _guard = grok_home_lock();
        const VAR: &str = "GROK_MODELS_TEST_FETCH_KEY";
        std::env::remove_var(VAR);
        assert_eq!(env_api_key(""), None);
        assert_eq!(env_api_key(VAR), None);
        std::env::set_var(VAR, "secret-token");
        assert_eq!(env_api_key(VAR).as_deref(), Some("secret-token"));
        std::env::set_var(VAR, "");
        assert_eq!(env_api_key(VAR), None);
        std::env::remove_var(VAR);
    }

    fn regex_lite_stamp(s: &str) -> bool {
        let b = s.as_bytes();
        // MM-DD-YYYY[space]HH:MM[space]AM|PM
        if b.len() != 19 {
            return false;
        }
        let digits = |i: usize| b[i].is_ascii_digit();
        digits(0)
            && digits(1)
            && b[2] == b'-'
            && digits(3)
            && digits(4)
            && b[5] == b'-'
            && digits(6)
            && digits(7)
            && digits(8)
            && digits(9)
            && b[10] == b' '
            && digits(11)
            && digits(12)
            && b[13] == b':'
            && digits(14)
            && digits(15)
            && b[16] == b' '
            && (&b[17..] == b"AM" || &b[17..] == b"PM")
    }

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
        let _guard = grok_home_lock();
        let home = std::env::temp_dir()
            .join(format!("gm-sync-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create test GROK_HOME");
        std::env::set_var("GROK_HOME", &home);
        let codex = std::env::temp_dir().join(format!("gm-sync-test-codex-{}", std::process::id()));
        std::fs::create_dir_all(&codex).expect("create test CODEX_HOME");
        std::env::set_var("CODEX_HOME", &codex);

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

        update_providers_json().expect("update providers.json");
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
        let _guard = grok_home_lock();
        let home = std::env::temp_dir()
            .join(format!("gm-delete-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create test GROK_HOME");
        std::env::set_var("GROK_HOME", &home);
        let codex = std::env::temp_dir().join(format!("gm-delete-test-codex-{}", std::process::id()));
        std::fs::create_dir_all(&codex).expect("create test CODEX_HOME");
        std::env::set_var("CODEX_HOME", &codex);

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
        update_providers_json().expect("re-add update");
        update_config_toml().expect("re-add write");

        let config = std::fs::read_to_string(paths::config_toml_path()).expect("config");
        assert!(config.contains("[model.prov-plain]"), "re-added provider's tables must return");
    }

    #[test]
    fn update_config_toml_uses_stored_api_backend() {
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("api-backend");
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

    fn isolate_codex_homes(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let pid = std::process::id();
        let grok = std::env::temp_dir().join(format!("gm-codex-{tag}-grok-{pid}"));
        let codex = std::env::temp_dir().join(format!("gm-codex-{tag}-codex-{pid}"));
        let _ = std::fs::remove_dir_all(&grok);
        let _ = std::fs::remove_dir_all(&codex);
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);
        (grok, codex)
    }

    fn two_provider_doc() -> Value {
        serde_json::json!({
            "providers": [
                {
                    "id": "openrouter",
                    "name": "OpenRouter",
                    "enabled": true,
                    "env_key": "OPENROUTER_API_KEY",
                    "base_url": "https://openrouter.ai/api/v1",
                    "models": {
                        "openrouter/free": { "name": "Free", "enabled": true }
                    }
                },
                {
                    "id": "ollama-cloud",
                    "name": "Ollama Cloud",
                    "enabled": true,
                    "env_key": "OLLAMA_API_KEY",
                    "base_url": "https://ollama.com/v1",
                    "models": {
                        "gemma4:31b": { "name": "Gemma", "enabled": true },
                        "deepseek-v4-flash:preview": { "name": "DeepSeek", "enabled": true }
                    }
                }
            ]
        })
    }

    #[test]
    fn codex_config_toml_writes_when_flag_set_and_skips_when_false() {
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("flag");

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
        assert!(catalog.contains("\"slug\": \"openrouter/free\""), "{catalog}");
    }


    #[test]
    fn codex_config_toml_only_writes_selected_provider_and_first_enabled_model() {
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("one");

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
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("nomodels");

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

    #[test]
    fn update_config_toml_resets_codex_when_provider_disabled() {
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("disable");

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
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("once");

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
        assert!(!cleared.contains("[model_providers.openrouter]"), "{cleared}");
        assert!(!cleared.contains("model = "), "{cleared}");
        assert!(
            !paths::codex_models_json_path("openrouter").exists(),
            "catalog json must be deleted on disable"
        );

        let manual = "# user edit after disable\napproval_policy = \"untrusted\"\n";
        std::fs::write(paths::codex_config_toml_path(), manual).unwrap();
        update_config_toml().unwrap();
        let after = std::fs::read_to_string(paths::codex_config_toml_path()).unwrap();
        assert_eq!(after, manual, "later writes must not re-enter Codex cleanup");
    }

    #[test]
    fn delete_codex_provider_clears_table_and_catalog_like_disable() {
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("delprov");

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

    #[test]
    fn seed_models_from_items_copies_catalog_modalities() {
        let catalog = serde_json::json!({
            "vision": {
                "name": "Vision",
                "modalities": {
                    "input": ["text", "image", "video", "pdf", "audio"],
                    "output": ["text"]
                }
            },
            "plain": { "name": "Plain" }
        });
        let items = vec![
            ("vision".to_string(), Some("Vision".to_string())),
            ("plain".to_string(), Some("Plain".to_string())),
        ];
        let seeded = seed_models_from_items(&items, catalog.as_object().unwrap(), "prov", None);
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
        let catalog = serde_json::json!({
            "sdk": {
                "name": "Sdk",
                "provider": { "npm": "@ai-sdk/openai" }
            },
            "empty": {
                "name": "Empty",
                "provider": { "npm": "" }
            },
            "plain": { "name": "Plain" }
        });
        let items = vec![
            ("sdk".to_string(), Some("Sdk".to_string())),
            ("empty".to_string(), Some("Empty".to_string())),
            ("plain".to_string(), Some("Plain".to_string())),
        ];
        let seeded = seed_models_from_items(&items, catalog.as_object().unwrap(), "prov", None);
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
        assert_eq!(get_api_backend("xai", Some("@ai-sdk/anthropic"), None), "responses");
        assert_eq!(
            get_api_backend("prov", Some("@ai-sdk/openai-compatible"), Some("@ai-sdk/openai")),
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
        let catalog = serde_json::json!({
            "sdk": {
                "name": "Sdk",
                "provider": { "npm": "@ai-sdk/openai" }
            },
            "plain": { "name": "Plain" }
        });
        let items = vec![
            ("sdk".to_string(), Some("Sdk".to_string())),
            ("plain".to_string(), Some("Plain".to_string())),
            ("live-only".to_string(), Some("Live".to_string())),
        ];
        let seeded = seed_models_from_items(
            &items,
            catalog.as_object().unwrap(),
            "prov",
            Some("@ai-sdk/openai-compatible"),
        );
        assert_eq!(seeded["sdk"]["api_backend"], "responses");
        assert_eq!(seeded["plain"]["api_backend"], "chat_completions");
        assert_eq!(seeded["live-only"]["api_backend"], "chat_completions");
    }

    #[test]
    fn reconcile_writes_api_backend_on_new_and_refreshes() {
        let mut models_map = Map::new();
        let items = vec![("m".to_string(), Some("M".to_string()))];
        let mut stats = Stats::default();
        let catalog = serde_json::json!({
            "m": {
                "name": "M",
                "provider": { "npm": "@ai-sdk/openai" }
            }
        });
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog.as_object().unwrap(),
            &mut stats,
            "prov",
            None,
        );
        assert_eq!(models_map["m"]["api_backend"], "responses");

        let catalog_anthropic = serde_json::json!({
            "m": {
                "name": "M",
                "provider": { "npm": "@ai-sdk/anthropic" }
            }
        });
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog_anthropic.as_object().unwrap(),
            &mut stats,
            "prov",
            None,
        );
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
        let mut stats = Stats::default();
        let catalog = serde_json::json!({});
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog.as_object().unwrap(),
            &mut stats,
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
        let mut stats = Stats::default();

        let catalog = serde_json::json!({
            "m": {
                "name": "M",
                "provider": { "npm": "@ai-sdk/anthropic" }
            }
        });
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog.as_object().unwrap(),
            &mut stats,
            "prov",
            None,
        );
        assert_eq!(models_map["m"]["npm"], "@ai-sdk/anthropic");

        let catalog_no_npm = serde_json::json!({ "m": { "name": "M" } });
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog_no_npm.as_object().unwrap(),
            &mut stats,
            "prov",
            None,
        );
        assert_eq!(
            models_map["m"]["npm"],
            "@ai-sdk/anthropic",
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
        let mut stats = Stats::default();

        let catalog = serde_json::json!({
            "m": {
                "name": "M",
                "modalities": {
                    "input": ["text", "image"],
                    "output": ["text"]
                }
            }
        });
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog.as_object().unwrap(),
            &mut stats,
            "prov",
            None,
        );
        assert_eq!(
            models_map["m"]["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );

        let catalog_no_mods = serde_json::json!({ "m": { "name": "M" } });
        reconcile_models_map(
            &mut models_map,
            &items,
            catalog_no_mods.as_object().unwrap(),
            &mut stats,
            "prov",
            None,
        );
        assert_eq!(
            models_map["m"]["modalities"]["input"],
            serde_json::json!(["text", "image"]),
            "omitted catalog modalities must not delete the stored value"
        );
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
        let _guard = grok_home_lock();
        let (_grok, _codex) = isolate_codex_homes("modalities");

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
        let catalog: Value = serde_json::from_str(
            &std::fs::read_to_string(&catalog_path).expect("catalog"),
        )
        .expect("parse catalog");
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

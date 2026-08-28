//! Non-interactive command implementations, ported verbatim.

use crate::core;
use crate::difflib;
use crate::fallback::prompt_line;
use crate::jsonio;
use crate::paths;
use crate::sync::{self};
use crate::{fail, Res};
use serde_json::{Map, Value};
use std::collections::HashMap;

fn usable(doc: &Value) -> Vec<Map<String, Value>> {
    doc.get("providers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|p| p.is_object() && p.get("id").is_some_and(|v| !v.is_null()))
                .filter_map(|p| p.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// `render_list_text` — plain-text listing (`--providers`, `--provider ID`).
pub fn render_list_text(
    doc: &Value,
    provider_filter: Option<&str>,
    providers_only: bool,
) -> Res<()> {
    let providers = usable(doc);
    if let Some(filter) = provider_filter {
        if !providers.iter().any(|p| p["id"].as_str() == Some(filter)) {
            let ids: Vec<String> = providers
                .iter()
                .map(|p| p["id"].as_str().unwrap_or_default().to_string())
                .collect();
            let hints = difflib::get_close_matches(filter, &ids);
            let hint = if hints.is_empty() {
                String::new()
            } else {
                format!(" (did you mean: {}?)", hints.join(", "))
            };
            return fail(format!(
                "unknown provider {}{}",
                core::py_repr(filter),
                hint
            ));
        }
    }
    println!("Configured providers");
    if providers.is_empty() {
        println!("No providers configured yet. Add with --add-provider");
        return Ok(());
    }

    // Python keeps full doc-order list, filtered to the one id when given.
    let shown_providers: Vec<&Map<String, Value>> = match provider_filter {
        None => providers.iter().collect(),
        Some(f) => vec![providers.iter().find(|p| p["id"].as_str() == Some(f)).unwrap()],
    };

    if providers_only && provider_filter.is_none() {
        let mut enabled_providers = 0usize;
        for provider in &shown_providers {
            let penabled = crate::get_bool_obj(provider, "enabled", true);
            if penabled {
                enabled_providers += 1;
            }
            println!("{}", provider_state_line(provider));
            let env = crate::first_env_key_from(provider);
            if !env.is_empty() {
                println!("    {}", core::env_status_line(&env));
            }
        }
        println!();
        println!(
            "Summary: {} providers · {} enabled",
            shown_providers.len(),
            enabled_providers
        );
        return Ok(());
    }

    let mut total_models = 0usize;
    let mut enabled_models = 0usize;
    let mut enabled_providers = 0usize;
    for (i, provider) in shown_providers.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let pid = provider["id"].as_str().unwrap_or_default();
        let pname = provider
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(pid);
        let penabled = crate::get_bool_obj(provider, "enabled", true);
        if penabled {
            enabled_providers += 1;
        }

        let empty = Map::new();
        let models_map = provider.get("models").and_then(Value::as_object).unwrap_or(&empty);
        let ids: Vec<String> = models_map.keys().cloned().collect();
        let en_count = ids
            .iter()
            .filter(|mid| {
                models_map
                    .get(*mid)
                    .map(|m| crate::get_bool_val(m, "enabled", true))
                    .unwrap_or(false)
            })
            .count();
        total_models += ids.len();
        enabled_models += en_count;

        let marker = if penabled { '●' } else { '○' };
        let state = if penabled { "enabled" } else { "disabled" };
        println!(
            "{} ({}) - {}  [{}]  {}/{} models",
            marker,
            pname,
            pid,
            state,
            en_count,
            ids.len()
        );

        if ids.is_empty() {
            println!("    (no models)");
            continue;
        }
        for mid in &ids {
            let m = models_map.get(mid);
            let menabled = m.map(|v| crate::get_bool_val(v, "enabled", true)).unwrap_or(false);
            let free_tag = if mid.to_lowercase().contains("free") {
                "  [free]"
            } else {
                ""
            };
            let mmark = if menabled { '●' } else { '○' };
            println!("    {} {}{}", mmark, mid, free_tag);
        }
    }

    println!();
    println!(
        "Summary: {} providers · {} enabled · {}/{} models enabled",
        shown_providers.len(),
        enabled_providers,
        enabled_models,
        total_models
    );
    Ok(())
}

/// `_provider_state_line`
fn provider_state_line(p: &Map<String, Value>) -> String {
    let penabled = crate::get_bool_obj(p, "enabled", true);
    let marker = if penabled { '●' } else { '○' };
    let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = p.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(pid);
    format!(
        "{} ({}) - {}  [{}]",
        marker,
        name,
        pid,
        if penabled { "enabled" } else { "disabled" }
    )
}

/// `render_models_text` (`--models`). Returns process exit code.
pub fn render_models_text() -> Res<i32> {
    let doc = jsonio::load_providers()?;
    let providers = usable(&doc);

    println!("Enabled models");

    let mut total_enabled = 0usize;
    let mut lines_out: Vec<String> = Vec::new();

    for provider in &providers {
        let pid = provider.get("id").and_then(Value::as_str).unwrap_or_default();
        let penabled = crate::get_bool_obj(provider, "enabled", true);
        let empty = Map::new();
        let mm = provider.get("models").and_then(Value::as_object).unwrap_or(&empty);
        let pname = provider
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(pid);
        for (mid, m) in mm {
            if !m.is_object() || !crate::get_bool_val(m, "enabled", true) {
                continue;
            }
            if !penabled {
                continue;
            }
            let mname = crate::name_or(m, mid);
            lines_out.push(format!("● {} ({}) - {}/{}", mname, pname, pid, mid));
            total_enabled += 1;
        }
    }
    for l in lines_out {
        println!("{}", l);
    }

    if total_enabled == 0 {
        println!("No enabled models. Enable with --enable or grok-models");
        return Ok(0);
    }

    println!();
    let mut env_rows: Vec<(String, String, String)> = Vec::new();
    for provider in &providers {
        if !crate::get_bool_obj(provider, "enabled", true) {
            continue;
        }
        let env = crate::first_env_key_from(provider);
        if !env.is_empty() {
            let pid = provider.get("id").and_then(Value::as_str).unwrap_or_default();
            let pname = provider
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(pid);
            env_rows.push((env.clone(), core::env_value(&env), pname.to_string()));
        }
    }
    if !env_rows.is_empty() {
        let maxlen = env_rows.iter().map(|(e, _, _)| e.len()).max().unwrap_or(0);
        for (env, value, pname) in &env_rows {
            println!("● {:<width$} = {}  ({})", env, value, pname, width = maxlen);
        }
    }
    println!("Summary: {} models enabled", total_enabled);
    Ok(0)
}

/// Target resolution result: (provider id, model id or None).
pub enum ResolvedTarget {
    Provider(String),
    Model(String, String),
}

/// `resolve_targets`
pub fn resolve_targets(doc: &Value, targets: &[String]) -> Res<Vec<ResolvedTarget>> {
    fn norm(s: &str) -> String {
        s.replace('.', "_").replace('/', "_").replace(':', "_")
    }

    let providers = usable(doc);
    let provider_ids: Vec<String> = providers
        .iter()
        .map(|p| p["id"].as_str().unwrap_or_default().to_string())
        .collect();

    let mut resolved: Vec<ResolvedTarget> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for target in targets {
        let (pid_raw, mid_raw) = match target.split_once('/') {
            None => (target.as_str(), None),
            Some((p, m)) => (p, Some(m)),
        };
        let matches: Vec<&Map<String, Value>> = providers
            .iter()
            .filter(|p| norm(p["id"].as_str().unwrap_or_default()) == norm(pid_raw))
            .collect();
        if matches.len() != 1 {
            let hints = difflib::get_close_matches(pid_raw, &provider_ids);
            let hint = if hints.is_empty() {
                String::new()
            } else {
                format!(" (did you mean: {}?)", hints.join(", "))
            };
            errors.push(format!(
                "unknown provider {}{}",
                core::py_repr(pid_raw),
                hint
            ));
            continue;
        }
        let provider = matches[0];
        let mid_raw = match mid_raw {
            None => {
                resolved.push(ResolvedTarget::Provider(
                    provider["id"].as_str().unwrap_or_default().to_string(),
                ));
                continue;
            }
            Some(m) => m,
        };
        let raw_ids: Vec<String> = provider
            .get("models")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let model_hits: Vec<String> = raw_ids
            .iter()
            .filter(|mid0| norm(mid0) == norm(mid_raw))
            .cloned()
            .collect();
        if model_hits.len() != 1 {
            let hints = difflib::get_close_matches(mid_raw, &raw_ids);
            let hint = if hints.is_empty() {
                String::new()
            } else {
                format!(" (did you mean: {}?)", hints.join(", "))
            };
            errors.push(format!(
                "unknown model {} for provider {}{}",
                core::py_repr(mid_raw),
                core::py_repr(pid_raw),
                hint
            ));
            continue;
        }
        resolved.push(ResolvedTarget::Model(
            provider["id"].as_str().unwrap_or_default().to_string(),
            model_hits[0].clone(),
        ));
    }
    if !errors.is_empty() {
        return fail(format!("cannot apply: {}", errors.join("; ")));
    }
    Ok(resolved)
}

/// `cmd_toggle --enable/--disable`. Returns exit code.
/// `--enable pid/mid` targets whose provider is absent from the doc: these
/// get auto-added (catalog-seeded) before resolution instead of failing.
/// Bare provider targets and disable targets never appear here.
fn missing_combo_providers(enable_targets: &[String], existing_ids: &[String]) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    for target in enable_targets {
        if let Some((pid, _mid)) = target.split_once('/') {
            let known = existing_ids.iter().any(|e| e == pid)
                || missing.iter().any(|m| m == pid);
            if !known {
                missing.push(pid.to_string());
            }
        }
    }
    missing
}

pub fn cmd_toggle(enable_targets: &[String], disable_targets: &[String]) -> Res<i32> {
    let providers_path = paths::providers_path();
    let mut doc = jsonio::load_providers()?;

    // A 'provider/model' enable target whose provider was never added used to
    // die in resolve_targets with "unknown provider". Add the provider first
    // (all models disabled, catalog-seeded), then the resolution below flips
    // just that model. Disable targets and bare provider ids keep the old
    // behavior.
    let existing_ids: Vec<String> = usable(&doc)
        .iter()
        .map(|p| p.get("id").and_then(Value::as_str).unwrap_or_default().to_string())
        .collect();
    let missing = missing_combo_providers(enable_targets, &existing_ids);
    if !missing.is_empty() {
        let api = sync::fetch_models_dev()?;
        for pid in &missing {
            add_provider_entry(&mut doc, &api, pid, false)?;
        }
    }

    let resolved_enable = resolve_targets(&doc, enable_targets)?;
    let resolved_disable = resolve_targets(&doc, disable_targets)?;

    // Later flags win when both lists hit the same target; first-insertion
    // order is kept (Python dict semantics).
    let mut applied_keys: Vec<(String, Option<String>)> = Vec::new();
    let mut applied: HashMap<(String, Option<String>), bool> = HashMap::new();
    for item in resolved_enable.into_iter().map(|r| (r, true)) {
        push_apply(&mut applied_keys, &mut applied, item.0, item.1);
    }
    for item in resolved_disable.into_iter().map(|r| (r, false)) {
        push_apply(&mut applied_keys, &mut applied, item.0, item.1);
    }

    fn push_apply(
        keys: &mut Vec<(String, Option<String>)>,
        map: &mut HashMap<(String, Option<String>), bool>,
        target: ResolvedTarget,
        want: bool,
    ) {
        let key = match target {
            ResolvedTarget::Provider(pid) => (pid, None),
            ResolvedTarget::Model(pid, mid) => (pid, Some(mid)),
        };
        if !keys.contains(&key) {
            keys.push(key.clone());
        }
        map.insert(key, want);
    }

    // Providers getting a model enabled while the provider itself is disabled.
    let mut disabled_provider_ids: std::collections::BTreeSet<String> = Default::default();
    for key in &applied_keys {
        let (pid, mid) = key;
        let want = applied[key];
        if mid.is_some() && want {
            let prov = find_by_id(&doc, pid);
            if let Some(prov) = prov {
                if !crate::get_bool(&Value::Object(prov.clone()), "enabled", true) {
                    disabled_provider_ids.insert(pid.clone());
                }
            }
        }
    }

    let mut changed = false;
    for key in &applied_keys {
        let (pid, mid) = key;
        let want = applied[key];
        let label = match mid {
            None => pid.clone(),
            Some(m) => format!("{pid}/{m}"),
        };
        let cur = find_by_id(&doc, pid);
        let cur = match cur {
            Some(c) => c,
            None => continue,
        };
        match mid {
            None => {
                let penabled = crate::get_bool(&Value::Object(cur.clone()), "enabled", true);
                if penabled == want {
                    println!(
                        "already {}: {}",
                        if want { "enabled" } else { "disabled" },
                        label
                    );
                    continue;
                }
                find_by_id_mut(&mut doc, pid)
                    .unwrap()
                    .insert("enabled".into(), Value::Bool(want));
                println!("{}: {}", if want { "enabled" } else { "disabled" }, label);
            }
            Some(mid_s) => {
                let slot = find_by_id_mut(&mut doc, pid).unwrap();
                let models = slot
                    .entry("models".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !models.is_object() {
                    *models = Value::Object(Map::new());
                }
                let mobj = models.as_object_mut().unwrap();
                let entry = mobj
                    .entry(mid_s.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !entry.is_object() {
                    *entry = Value::Object(Map::new());
                }
                let eobj = entry.as_object_mut().unwrap();
                let menabled = eobj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                if menabled == want {
                    println!(
                        "already {}: {}",
                        if want { "enabled" } else { "disabled" },
                        label
                    );
                    continue;
                }
                eobj.insert("enabled".into(), Value::Bool(want));
                println!("{}: {}", if want { "enabled" } else { "disabled" }, label);
            }
        }
        changed = true;
    }

    if !changed {
        return Ok(0);
    }

    jsonio::dump_providers(&providers_path, &mut doc)?;
    for pid in disabled_provider_ids {
        println!(
            "warning: provider {} is disabled; enable it too or its \
models won't be written to config.toml",
            core::py_repr(&pid)
        );
    }
    let (path, stats) = sync::run_sync()?;
    if let Some(path) = path {
        sync::print_sync_report(&stats, &path, &doc);
        sync::print_relaunch();
    }
    Ok(0)
}

fn find_by_id(doc: &Value, pid: &str) -> Option<Map<String, Value>> {
    doc.get("providers")?
        .as_array()?
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
        .and_then(|p| p.as_object().cloned())
}

fn find_by_id_mut<'a>(doc: &'a mut Value, pid: &str) -> Option<&'a mut Map<String, Value>> {
    doc.get_mut("providers")?
        .as_array_mut()?
        .iter_mut()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
        .and_then(Value::as_object_mut)
}

/// `cmd_disable_all`. Returns exit code.
pub fn cmd_disable_all() -> Res<i32> {
    let providers_path = paths::providers_path();
    let mut doc = jsonio::load_providers()?;
    let mut changed = false;
    if let Some(arr) = doc.get_mut("providers").and_then(Value::as_array_mut) {
        for provider in arr.iter_mut() {
            if !provider.is_object() {
                continue;
            }
            let models = provider.get_mut("models");
            let models = match models {
                Some(m) if m.is_object() => m,
                _ => continue,
            };
            let mobj = models.as_object_mut().unwrap();
            for (_, m) in mobj.iter_mut() {
                if m.is_object() && crate::get_bool_val(m, "enabled", true) {
                    m.as_object_mut()
                        .unwrap()
                        .insert("enabled".into(), Value::Bool(false));
                    changed = true;
                }
            }
        }
    }
    if !changed {
        println!("All models already disabled.");
        return Ok(0);
    }
    jsonio::dump_providers(&providers_path, &mut doc)?;
    let (path, stats) = sync::run_sync()?;
    if let Some(path) = path {
        sync::print_sync_report(&stats, &path, &doc);
        sync::print_relaunch();
    }
    Ok(0)
}

/// `add_provider_entry`: add provider with all models disabled and persist.
pub fn add_provider_entry(doc: &mut Value, api: &Value, provider_id: &str, quiet: bool) -> Res<Option<String>> {
    let existing: Vec<String> = usable(doc)
        .iter()
        .map(|p| p.get("id").map(id_to_string).unwrap_or_default())
        .collect();
    if existing.iter().any(|e| e == provider_id) {
        if !quiet {
            println!("Provider {} already exists.", core::py_repr(provider_id));
        }
        return Ok(None);
    }
    let pinfo = match api.get(provider_id) {
        Some(p) if p.is_object() => p.clone(),
        _ => return fail(format!("provider {} not found in models.dev", core::py_repr(provider_id))),
    };
    let catalog = pinfo
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("id".into(), Value::String(provider_id.to_string()));
    let name_val = pinfo.get("name").cloned().unwrap_or(Value::String(provider_id.to_string()));
    let name_val = if crate::truthy(Some(&name_val)) {
        name_val
    } else {
        Value::String(provider_id.to_string())
    };
    entry.insert("name".into(), name_val);
    let env = core::api_env_key(&pinfo);
    if !env.is_empty() {
        entry.insert("env_key".into(), Value::String(env));
    }
    // Seed the provider-level base_url override from the catalog so the
    // config menu shows the configured endpoint even before any edit.
    let api_url = pinfo.get("api").and_then(Value::as_str).unwrap_or_default();
    if !api_url.is_empty() {
        entry.insert("base_url".into(), Value::String(api_url.to_string()));
    }
    let (items, fetch_err_url) = crate::sync::authority_items_for_provider(&pinfo, api_url, quiet);
    if items.is_empty() {
        return fail(format!(
            "provider {} has no models in models.dev",
            core::py_repr(provider_id)
        ));
    }
    let models_map = crate::sync::seed_models_from_items(&items, &catalog);
    let n_models = models_map.len();
    entry.insert("enabled".into(), Value::Bool(true));
    entry.insert("models".into(), Value::Object(models_map));

    doc.get_mut("providers")
        .and_then(Value::as_array_mut)
        .unwrap()
        .push(Value::Object(entry));
    jsonio::dump_providers(&paths::providers_path(), doc)?;
    if !quiet {
        println!(
            "Added provider {} with {} models (all disabled).",
            core::py_repr(provider_id),
            n_models
        );
    }
    Ok(fetch_err_url)
}

fn id_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `search_providers`: search models.dev providers by term; pick via numbered menu.
pub fn search_providers(api: &Value, term: &str) -> Res<Option<String>> {
    let term_l = term.to_lowercase();
    let mut matches: Vec<(String, String)> = Vec::new();
    if let Some(obj) = api.as_object() {
        for (pid, pinfo) in obj {
            if !pinfo.is_object() {
                continue;
            }
            let name = pinfo.get("name").and_then(Value::as_str).unwrap_or("");
            if pid.to_lowercase().contains(&term_l) || name.to_lowercase().contains(&term_l) {
                matches.push((pid.clone(), name.to_string()));
            }
        }
    }
    if matches.is_empty() {
        println!("No providers matched that term.");
        return Ok(None);
    }
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    let shown_len = matches.len().min(50);
    for (i, (pid, name)) in matches.iter().take(shown_len).enumerate() {
        println!("  {}. {} ({})", i + 1, pid, name);
    }
    if matches.len() > shown_len {
        println!("  ... and {} more", matches.len() - shown_len);
    }
    loop {
        let choice = prompt_line("Select provider (number or id, 'cancel')", None)?
            .trim()
            .to_string();
        if choice.to_lowercase() == "cancel" {
            return Ok(None);
        }
        if !choice.is_empty() && choice.chars().all(|c| c.is_ascii_digit()) {
            let idx: usize = choice.parse().unwrap_or(0);
            if idx >= 1 && idx <= shown_len {
                return Ok(Some(matches[idx - 1].0.clone()));
            }
        }
        for (pid, _) in &matches {
            if pid == &choice {
                return Ok(Some(pid.clone()));
            }
        }
        println!("Pick a listed number or provider id, or 'cancel'.");
    }
}

/// `cmd_add_provider`
pub fn cmd_add_provider(provider_id: &str) -> Res<i32> {
    let mut doc = jsonio::load_providers()?;
    let api = sync::fetch_models_dev()?;
    add_provider_entry(&mut doc, &api, provider_id, false)?;
    Ok(0)
}

/// `cmd_codex`: persist the Codex provider pick (or 'disable').
pub fn cmd_codex(raw: &str) -> Res<i32> {
    let mut doc = jsonio::load_providers()?;
    let pid = raw.trim();
    if pid == "disabled" {
        jsonio::set_codex_selection(&mut doc, None);
        jsonio::dump_providers(&paths::providers_path(), &mut doc)?;
        println!("Codex Config disabled");
        return Ok(0);
    }
    if !jsonio::enabled_provider_ids(&doc).iter().any(|e| e == pid) {
        return Err(crate::SyncError(format!(
            "--codex requires 'disabled' or an enabled provider id (got {})",
            core::py_repr(pid)
        )));
    }
    jsonio::set_codex_selection(&mut doc, Some(pid));
    jsonio::dump_providers(&paths::providers_path(), &mut doc)?;
    println!("Codex Config {pid}");
    Ok(0)
}

/// `cmd_import`: seed `providers.json` from the `[model.*]` tables already in
/// `config.toml`, then enable those models. Reuses `--add-provider` and
/// `--enable`, so no custom reconcile code is needed (mirrors Python).
pub fn cmd_import() -> Res<i32> {
    let cfg_path = paths::config_toml_path();
    if !cfg_path.exists() {
        println!("No config.toml found; nothing to import.");
        return Ok(0);
    }
    let text = match std::fs::read_to_string(&cfg_path) {
        Ok(t) => t,
        Err(e) => return crate::fail(format!("failed to read {}: {e}", cfg_path.display())),
    };
    let toml_data: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return crate::fail(format!("failed to read {}: {e}", cfg_path.display())),
    };

    let model_tables = match toml_data.get("model") {
        Some(toml::Value::Table(t)) => t,
        _ => {
            println!("No [model.*] tables in config.toml; nothing to import.");
            return Ok(0);
        }
    };

    let mut provider_models: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (table_key, table) in model_tables {
        let model_id = match table.get("model").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let safe_model_id: String = model_id
            .chars()
            .map(|c| if c == '.' || c == '/' || c == ':' { '_' } else { c })
            .collect();
        let provider_id = match table_key.strip_suffix(&format!("-{safe_model_id}")) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => table_key.clone(),
        };
        provider_models.entry(provider_id).or_default().push(model_id);
    }

    if provider_models.is_empty() {
        println!("No [model.*] tables in config.toml; nothing to import.");
        return Ok(0);
    }

    // add-provider no-ops on a provider id that already exists in
    // providers.json, so re-call it here for every imported provider and
    // capture which ids it skipped. Those skipped providers need an
    // explicit enable so the later run_sync reconciles them against the
    // models.dev catalog (adds missing models, drops dead ones) before
    // the per-model enables run.
    let providers_doc_before_add = jsonio::load_providers()?;
    let existing_ids: Vec<String> = providers_doc_before_add
        .get("providers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("id").and_then(Value::as_str).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let enable_providers: Vec<String> = provider_models
        .keys()
        .filter(|pid| existing_ids.iter().any(|e| e == *pid))
        .cloned()
        .collect();

    for provider_id in provider_models.keys() {
        cmd_add_provider(provider_id)?;
    }

    if !enable_providers.is_empty() {
        cmd_toggle(&enable_providers, &[])?;
    }

    let enable_models: Vec<String> = provider_models
        .iter()
        .flat_map(|(provider_id, model_ids)| {
            model_ids
                .iter()
                .map(move |mid| format!("{provider_id}/{mid}"))
        })
        .collect();
    let disable_models: Vec<String> = Vec::new();
    cmd_toggle(&enable_models, &disable_models)?;
    Ok(0)
}

/// `cmd_search`
pub fn cmd_search(term: &str) -> Res<i32> {
    let api = sync::fetch_models_dev()?;
    let provider_id = search_providers(&api, term)?;
    match provider_id {
        None => Ok(0),
        Some(pid) => {
            let mut doc = jsonio::load_providers()?;
            add_provider_entry(&mut doc, &api, &pid, false)?;
            Ok(0)
        }
    }
}

/// `cmd_sync` (default run)
pub fn cmd_sync() -> Res<i32> {
    let doc = jsonio::load_providers()?;
    let (path, stats) = sync::run_sync()?;
    match path {
        None => Ok(0),
        Some(path) => {
            sync::print_sync_report(&stats, &path, &doc);
            sync::print_relaunch();
            Ok(0)
        }
    }
}

/// `resolve_targets` exposed for the harness binary (no network).
pub fn resolve_targets_local(doc: &Value, targets: &[String]) -> Res<Vec<ResolvedTarget>> {
    resolve_targets(doc, targets)
}

impl ResolvedTarget {
    pub fn _none() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_combo_providers_dedupes_and_ignores_bare_and_known() {
        let existing = vec!["opencode".to_string(), "grok".to_string()];
        let targets: Vec<String> = vec![
            "newprov/m1".into(),
            "opencode/m2".into(),
            "bareprovider".into(),
            "newprov/m3".into(),
            "grok".into(),
            "other/m4".into(),
        ];
        assert_eq!(
            missing_combo_providers(&targets, &existing),
            vec!["newprov".to_string(), "other".to_string()]
        );
    }

    #[test]
    fn missing_combo_providers_empty_when_all_known() {
        let existing = vec!["a".to_string()];
        let targets: Vec<String> = vec!["a/x".into(), "a/y".into(), "a".into()];
        assert!(missing_combo_providers(&targets, &existing).is_empty());
    }

    #[test]
    fn cmd_codex_sets_provider_or_disabled() {
        let _guard = crate::test_support::grok_home_lock();
        let pid = std::process::id();
        let grok = std::env::temp_dir().join(format!("gm-cmd-codex-grok-{pid}"));
        let codex = std::env::temp_dir().join(format!("gm-cmd-codex-codex-{pid}"));
        let _ = std::fs::remove_dir_all(&grok);
        let _ = std::fs::remove_dir_all(&codex);
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);

        let mut doc = serde_json::json!({
            "providers": [{
                "id": "openrouter",
                "name": "OpenRouter",
                "enabled": true,
                "models": { "openrouter/free": { "enabled": true } }
            }]
        });
        jsonio::dump_providers(&paths::providers_path(), &mut doc).unwrap();

        assert!(cmd_codex("true").is_err());
        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);
        cmd_codex("openrouter").expect("enable provider");
        let loaded = jsonio::load_providers_from(&grok.join("providers.json")).unwrap();
        assert_eq!(loaded["write_codex_config_toml"], Value::Bool(true));
        assert_eq!(loaded["codex_model_provider"], "openrouter");
        assert!(
            !codex.join("openrouter-models.json").exists(),
            "catalog json must NOT be written on enable; only at sync"
        );

        // Sync is the only path that writes the Codex sibling files.
        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);
        crate::sync::update_config_toml_with(false).unwrap();
        assert!(
            codex.join("openrouter-models.json").exists(),
            "catalog json must be written by sync"
        );

        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);
        cmd_codex("disabled").expect("disable");
        let loaded = jsonio::load_providers_from(&grok.join("providers.json")).unwrap();
        assert_eq!(loaded["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(
            loaded["codex_model_provider"], "openrouter",
            "disable alone must keep the remembered provider; only sync clears it"
        );
        assert!(
            codex.join("openrouter-models.json").exists(),
            "disable alone must not delete the catalog; only sync does"
        );

        // Next sync one-shot clears the remembered provider and deletes the catalog.
        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);
        crate::sync::update_config_toml_with(false).unwrap();
        let cleared = jsonio::load_providers_from(&grok.join("providers.json")).unwrap();
        assert_eq!(
            cleared["codex_model_provider"], "",
            "next sync must clear the remembered provider (one-shot)"
        );
        assert!(
            !codex.join("openrouter-models.json").exists(),
            "catalog json must be deleted on sync after disable"
        );

        std::env::set_var("GROK_HOME", &grok);
        std::env::set_var("CODEX_HOME", &codex);
        assert!(cmd_codex("missing").is_err());
    }
}

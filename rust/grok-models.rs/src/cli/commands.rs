//! Non-interactive command implementations.

use crate::core;
use crate::env::paths;
use crate::fallback::prompt_line;
use crate::jsonio;
use crate::sync;
use crate::{Res, fail};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Display name for a provider entry: `name` when set, else `id`.
fn provider_display_name<'a>(provider: &'a Map<String, Value>) -> &'a str {
    let pid = provider
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    provider
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(pid)
}

/// `render_list_text` — plain-text listing (`--providers`, `--provider ID`).
pub fn render_list_text(
    doc: &Value,
    provider_filter: Option<&str>,
    providers_only: bool,
) -> Res<()> {
    let providers = core::provider_entries(doc);
    if let Some(filter) = provider_filter {
        if !providers.iter().any(|p| p["id"].as_str() == Some(filter)) {
            return fail(format!("unknown provider '{filter}'"));
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
        Some(f) => vec![
            providers
                .iter()
                .find(|p| p["id"].as_str() == Some(f))
                .unwrap(),
        ],
    };

    if providers_only && provider_filter.is_none() {
        let mut enabled_providers = 0usize;
        for provider in &shown_providers {
            let penabled = crate::json_utils::get_bool_map(provider, "enabled");
            if penabled {
                enabled_providers += 1;
            }
            println!("{}", provider_state_line(provider));
            let env = core::provider_env_key_from_json(provider);
            if !env.is_empty() {
                println!("    {}", crate::env::vars::env_key_masked_display(&env));
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
        let pname = provider_display_name(provider);
        let penabled = crate::json_utils::get_bool_map(provider, "enabled");
        if penabled {
            enabled_providers += 1;
        }

        let empty = Map::new();
        let models_map = provider
            .get("models")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let ids: Vec<String> = models_map.keys().cloned().collect();
        let en_count = ids
            .iter()
            .filter(|mid| {
                models_map
                    .get(*mid)
                    .map(|m| crate::json_utils::get_bool_value(m, "enabled"))
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
            let menabled = m
                .map(|v| crate::json_utils::get_bool_value(v, "enabled"))
                .unwrap_or(false);
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

/// One-line provider state for `--providers`.
fn provider_state_line(p: &Map<String, Value>) -> String {
    let penabled = crate::json_utils::get_bool_map(p, "enabled");
    let marker = if penabled { '●' } else { '○' };
    let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = provider_display_name(p);
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
    let providers = core::provider_entries(&doc);

    println!("Enabled models");

    let mut total_enabled = 0usize;
    let mut lines_out: Vec<String> = Vec::new();

    for provider in &providers {
        let pid = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let penabled = crate::json_utils::get_bool_map(provider, "enabled");
        let empty = Map::new();
        let mm = provider
            .get("models")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let pname = provider_display_name(provider);
        for (mid, m) in mm {
            if !m.is_object() || !crate::json_utils::get_bool_value(m, "enabled") {
                continue;
            }
            if !penabled {
                continue;
            }
            let mname = crate::json_utils::get_name_or(m, mid);
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
        if !crate::json_utils::get_bool_map(provider, "enabled") {
            continue;
        }
        let env = core::provider_env_key_from_json(provider);
        if !env.is_empty() {
            let pname = provider_display_name(provider);
            env_rows.push((
                env.clone(),
                crate::env::vars::env_key_masked(&env),
                pname.to_string(),
            ));
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

/// Resolve CLI targets to provider ids and provider/model pairs.
pub fn resolve_targets(doc: &Value, targets: &[String]) -> Res<Vec<ResolvedTarget>> {
    fn norm(s: &str) -> String {
        s.replace('.', "_").replace('/', "_").replace(':', "_")
    }

    let providers = core::provider_entries(doc);

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
            errors.push(format!("unknown provider '{pid_raw}'"));
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
            errors.push(format!(
                "unknown model '{mid_raw}' for provider '{pid_raw}'"
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
            let known = existing_ids.iter().any(|e| e == pid) || missing.iter().any(|m| m == pid);
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
    let existing_ids: Vec<String> = core::provider_entries(&doc)
        .iter()
        .map(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let missing = missing_combo_providers(enable_targets, &existing_ids);
    if !missing.is_empty() {
        let api = sync::fetch_models_dev()?;
        for pid in &missing {
            let r = sync::add_provider_entry(&mut doc, &api, pid)?;
            report_add(pid, &r);
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
    let mut disabled_provider_ids: BTreeSet<String> = BTreeSet::new();
    for key in &applied_keys {
        let (pid, mid) = key;
        let want = applied[key];
        if mid.is_some() && want {
            let prov = core::find_provider_by_id(&doc, pid);
            if let Some(prov) = prov {
                if !crate::json_utils::get_bool_map(&prov, "enabled") {
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
        let cur = match core::find_provider_by_id(&doc, pid) {
            Some(c) => c,
            None => continue,
        };
        match mid {
            None => {
                let penabled = crate::json_utils::get_bool_map(&cur, "enabled");
                if penabled == want {
                    println!(
                        "already {}: {}",
                        if want { "enabled" } else { "disabled" },
                        label
                    );
                    continue;
                }
                core::find_provider_by_id_mut(&mut doc, pid)
                    .unwrap()
                    .insert("enabled".into(), Value::Bool(want));
                println!("{}: {}", if want { "enabled" } else { "disabled" }, label);
            }
            Some(mid_s) => {
                let slot = core::find_provider_by_id_mut(&mut doc, pid).unwrap();
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
            "warning: provider '{}' is disabled; enable it too or its \
models won't be written to config.toml",
            &pid
        );
    }
    let (written, response) = sync::run_sync()?;
    sync::print_sync_warnings(&response, &written);
    sync::print_sync_report(&written.path, &doc);
    sync::print_relaunch();
    Ok(0)
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
                if m.is_object() && crate::json_utils::get_bool_value(m, "enabled") {
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
    let (written, response) = sync::run_sync()?;
    sync::print_sync_warnings(&response, &written);
    sync::print_sync_report(&written.path, &doc);
    sync::print_relaunch();
    Ok(0)
}

/// Search models.dev providers by term; pick via numbered menu.
pub fn search_providers(api: &crate::sync::ModelsDev, term: &str) -> Res<Option<String>> {
    let term_l = term.to_lowercase();
    let mut matches: Vec<(String, String)> = Vec::new();
    for (pid, provider) in &api.providers {
        let name = provider.name.as_deref().unwrap_or("");
        if pid.to_lowercase().contains(&term_l) || name.to_lowercase().contains(&term_l) {
            matches.push((pid.clone(), name.to_string()));
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

/// Add a provider entry seeded from the models.dev catalog.
pub fn cmd_add_provider(provider_id: &str) -> Res<i32> {
    let mut doc = jsonio::load_providers()?;
    let api = sync::fetch_models_dev()?;
    let r = sync::add_provider_entry(&mut doc, &api, provider_id)?;
    report_add(provider_id, &r);
    Ok(0)
}

/// Renders the stdout report for one add, matching the Python tool's wording
/// and order: the live-fetch warning first, then the add line.
fn report_add(provider_id: &str, r: &sync::AddProviderResponse) {
    if let Some(url) = &r.fetch_warning_url {
        println!("{}", sync::live_fetch_error_status(url));
    }
    if r.already_present {
        println!("Provider '{}' already exists.", provider_id);
    } else {
        println!(
            "Added provider '{}' with {} models (all disabled).",
            provider_id, r.model_count
        );
    }
}

/// Persist the Codex provider pick (or 'disable').
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
        return Err(crate::Error::new(format!(
            "--codex requires 'disabled' or an enabled provider id (got '{}')",
            pid
        )));
    }
    jsonio::set_codex_selection(&mut doc, Some(pid));
    jsonio::dump_providers(&paths::providers_path(), &mut doc)?;
    println!("Codex Config {pid}");
    Ok(0)
}

/// Seed `providers.json` from existing `[model.*]` tables, then enable them.
/// Reuses `--add-provider` and `--enable`, so no custom reconcile code is needed.
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

    let mut provider_models: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (table_key, table) in model_tables {
        let model_id = match table.get("model").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let safe_model_id: String = model_id
            .chars()
            .map(|c| {
                if c == '.' || c == '/' || c == ':' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let provider_id = match table_key.strip_suffix(&format!("-{safe_model_id}")) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => table_key.clone(),
        };
        provider_models
            .entry(provider_id)
            .or_default()
            .push(model_id);
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
    cmd_toggle(&enable_models, &[])?;
    Ok(0)
}

/// Search models.dev for a term, add the picked provider.
pub fn cmd_search(term: &str) -> Res<i32> {
    let api = sync::fetch_models_dev()?;
    let provider_id = search_providers(&api, term)?;
    match provider_id {
        None => Ok(0),
        Some(pid) => {
            let mut doc = jsonio::load_providers()?;
            let r = sync::add_provider_entry(&mut doc, &api, &pid)?;
            report_add(&pid, &r);
            Ok(0)
        }
    }
}

/// Default run: reconcile providers.json into config.toml.
pub fn cmd_sync() -> Res<i32> {
    let doc = jsonio::load_providers()?;
    let (written, response) = sync::run_sync()?;
    sync::print_sync_warnings(&response, &written);
    sync::print_sync_report(&written.path, &doc);
    sync::print_relaunch();
    Ok(0)
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
        let _homes = crate::env::test_support::TestHomes::setup();
        let grok_home = &_homes.grok_home;
        let codex_home = &_homes.codex_home;

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
        cmd_codex("openrouter").expect("enable provider");
        let loaded = jsonio::load_providers_from(&grok_home.join("providers.json")).unwrap();
        assert_eq!(loaded["write_codex_config_toml"], Value::Bool(true));
        assert_eq!(loaded["codex_model_provider"], "openrouter");
        assert!(
            !codex_home.join("openrouter-models.json").exists(),
            "catalog json must NOT be written on enable; only at sync"
        );

        // Sync is the only path that writes the Codex sibling files.
        crate::sync::update_config_toml().unwrap();
        assert!(
            codex_home.join("openrouter-models.json").exists(),
            "catalog json must be written by sync"
        );

        cmd_codex("disabled").expect("disable");
        let loaded = jsonio::load_providers_from(&grok_home.join("providers.json")).unwrap();
        assert_eq!(loaded["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(
            loaded["codex_model_provider"], "openrouter",
            "disable alone must keep the remembered provider; only sync clears it"
        );
        assert!(
            codex_home.join("openrouter-models.json").exists(),
            "disable alone must not delete the catalog; only sync does"
        );

        // Next sync one-shot clears the remembered provider and deletes the catalog.
        crate::sync::update_config_toml().unwrap();
        let cleared = jsonio::load_providers_from(&grok_home.join("providers.json")).unwrap();
        assert_eq!(
            cleared["codex_model_provider"], "",
            "next sync must clear the remembered provider (one-shot)"
        );
        assert!(
            !codex_home.join("openrouter-models.json").exists(),
            "catalog json must be deleted on sync after disable"
        );

        assert!(cmd_codex("missing").is_err());
    }
}

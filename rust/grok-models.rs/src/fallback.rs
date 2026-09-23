//! Numbered (non-TTY) interactive flows and stdin prompt helpers — ported
//! verbatim from `_numbered_select`, `_config_models_numbered`,
//! `_config_models`, `_confirm_delete`, and `_numbered_config_flow`.

use crate::core;
use crate::env::paths;
use crate::jsonio;
use crate::{Res, fail};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};

/// `prompt_line`: print label (+ optional default), read a stripped line.
/// Empty input with a default returns the default. EOF is fatal.
pub fn prompt_line(label: &str, default: Option<&str>) -> Res<String> {
    let shown = match default {
        None => format!("{label}: "),
        Some(d) => format!("{label} [{d}]: "),
    };
    print!("{shown}");
    std::io::stdout().flush().ok();
    let raw = read_line_stripped()?;
    if raw.is_empty() {
        if let Some(d) = default {
            return Ok(d.to_string());
        }
    }
    Ok(raw)
}

fn read_line_stripped() -> Res<String> {
    let mut line = String::new();
    let n = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|_| SyncErrRead)?;
    if n == 0 {
        return fail("unexpected end of input");
    }
    Ok(line.trim().to_string())
}

struct SyncErrRead;
impl From<SyncErrRead> for crate::Error {
    fn from(_: SyncErrRead) -> Self {
        crate::Error::new("unexpected end of input")
    }
}

/// `_numbered_select`: numbered menu; returns chosen index or None to cancel.
pub fn numbered_select(
    options: &[String],
    title: Option<&str>,
    allow_cancel: bool,
    footer: Option<&str>,
) -> Res<Option<usize>> {
    if options.is_empty() {
        return Ok(None);
    }
    if let Some(t) = title {
        println!("{t}");
    }
    for (i, opt) in options.iter().enumerate() {
        println!("  {}. {}", i + 1, opt);
    }
    if let Some(f) = footer {
        println!("{f}");
    }
    loop {
        let prompt = if allow_cancel {
            "Select (number, or 'q' to cancel)"
        } else {
            "Select (number)"
        };
        let choice = prompt_line(prompt, None)?;
        if allow_cancel && choice.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        // Python str.isdigit()
        if !choice.is_empty() && choice.chars().all(|c| c.is_ascii_digit()) {
            let n: usize = choice.parse().unwrap_or(0);
            if n >= 1 && n <= options.len() {
                return Ok(Some(n - 1));
            }
        }
        println!("Invalid selection.");
    }
}

/// `_sort_model_indices` re-export for flows.
use core::sort_model_indices;

/// `_config_models_numbered`: filter + paged toggle list. Returns changed.
pub fn config_models_numbered(ids: &[String], models: &mut Map<String, Value>) -> Res<bool> {
    const PAGE: usize = 15;
    let mut changed = false;
    loop {
        let q = prompt_line("Filter substring (empty = all, 'q' done)", None)?;
        if q.eq_ignore_ascii_case("q") {
            return Ok(changed);
        }
        let filter = if q.is_empty() { None } else { Some(q.as_str()) };
        let sorted = sort_model_indices(ids, models, filter);
        let matches = &sorted.filtered;
        if matches.is_empty() {
            println!("No matches.");
            continue;
        }
        let enabled_count = sorted.enabled_count;
        let free_disabled_count = sorted.free_disabled_count;
        let total = matches.len();
        let mut page = 0usize;
        loop {
            let start = page * PAGE;
            let end = total.min(start + PAGE);
            for (n0, _i0) in (start..end).enumerate() {
                let n = start + n0 + 1;
                let i = matches[n0 + start];
                let mid = &ids[i];
                let enabled = models
                    .get(mid)
                    .map(|m| crate::json_utils::get_bool_value(m, "enabled"))
                    .unwrap_or(false);
                if n == start + enabled_count && enabled_count < total {
                    println!("  {}", "─".repeat(40));
                }
                let free_sep_idx = enabled_count + free_disabled_count;
                if n == start + free_sep_idx && free_disabled_count > 0 && free_sep_idx < total {
                    println!("  {}", "─".repeat(40));
                }
                println!("  {}. [{}] {}", n, if enabled { "x" } else { " " }, mid);
            }
            let more = end < total;
            let mut nav: Vec<&str> = Vec::new();
            if page > 0 {
                nav.push("p: prev");
            }
            if more {
                nav.push("n: next");
            }
            let nav_hint = if nav.is_empty() {
                String::new()
            } else {
                format!("  ({})", nav.join("  "))
            };
            let raw = prompt_line(
                &format!("Toggle a number{nav_hint}  (Enter for new filter)"),
                None,
            )?;
            if raw.is_empty() {
                break;
            }
            let lower = raw.to_lowercase();
            if lower == "n" && more {
                page += 1;
                continue;
            }
            if lower == "p" && page > 0 {
                page -= 1;
                continue;
            }
            if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
                let n: usize = raw.parse().unwrap_or(0);
                if n >= start + 1 && n <= end {
                    let i = matches[n - 1];
                    let mid = ids[i].clone();
                    let entry = models
                        .entry(mid)
                        .or_insert_with(|| Value::Object(Map::new()));
                    if !entry.is_object() {
                        *entry = Value::Object(Map::new());
                    }
                    let obj = entry.as_object_mut().unwrap();
                    let cur = crate::json_utils::get_bool_map(obj, "enabled");
                    obj.insert("enabled".into(), Value::Bool(!cur));
                    changed = true;
                    continue;
                }
            }
            if lower == "q" {
                return Ok(changed);
            }
            println!("Invalid selection.");
        }
    }
}

/// `_config_models`: numbered model configuration for one provider entry.
pub fn config_models(
    provider_id: &str,
    doc: &mut Value,
    selected: &mut Map<String, Value>,
) -> Res<bool> {
    let empty = Map::new();
    let models_is_map = selected.get("models").is_some_and(Value::is_object);
    let models_len = selected
        .get("models")
        .and_then(Value::as_object)
        .map(|m| m.len())
        .unwrap_or(0);
    if !models_is_map || models_len == 0 {
        println!(
            "No models for '{}'. Run a sync or re-add the provider.",
            provider_id
        );
        return Ok(false);
    }
    let ids: Vec<String> = selected
        .get("models")
        .and_then(Value::as_object)
        .unwrap_or(&empty)
        .keys()
        .cloned()
        .collect();
    let changed = {
        let mut models = selected.get_mut("models").unwrap().as_object_mut().unwrap();
        config_models_numbered(&ids, &mut models)?
    };
    // `selected` is an owned clone; write it back before persisting.
    if let Some(slot) = core::find_provider_by_id_mut(doc, provider_id) {
        *slot = selected.clone();
    }
    if changed {
        jsonio::dump_providers(&paths::providers_path(), doc)?;
    }
    let models_ref = selected.get("models").and_then(Value::as_object).unwrap();
    let enabled = ids
        .iter()
        .filter(|mid| {
            models_ref
                .get(*mid)
                .map(|m| crate::json_utils::get_bool_value(m, "enabled"))
                .unwrap_or(false)
        })
        .count();
    println!(
        "Updated models for '{}': {} enabled of {}.",
        provider_id,
        enabled,
        ids.len()
    );
    Ok(changed)
}

/// `_confirm_delete`
pub fn confirm_delete(label: &str) -> Res<bool> {
    loop {
        let confirm = prompt_line(&format!("Delete Provider {label}?"), Some("no"))?;
        let parsed: Option<bool> = if confirm.is_empty() {
            None
        } else {
            core::parse_bool(&confirm)
        };
        match parsed {
            None => println!("Enter yes or no."),
            Some(b) => return Ok(b),
        }
    }
}

fn provider_label_list(doc: &Value) -> Vec<(String, String)> {
    // Returns (id, label) pairs in loader order (name-sorted by load_providers).
    let mut entries: Vec<(String, String)> = Vec::new();
    for p in crate::core::provider_entries(doc) {
        let id = p
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let label = crate::core::provider_label(&p);
        entries.push((id, label));
    }
    entries
}

/// `_numbered_config_flow`: whole TUI flow over stdin/stdout.
/// Returns whether providers.json changed.
pub fn numbered_config_flow(doc: &mut Value) -> Res<bool> {
    let mut changed = false;
    loop {
        let entries = provider_label_list(doc);
        if entries.is_empty() {
            return Ok(changed);
        }
        let labels: Vec<String> = entries.iter().map(|(_, l)| l.clone()).collect();
        let pi = numbered_select(&labels, Some("Select a provider  (q quits)"), true, None)?;
        let pi = match pi {
            None => return Ok(changed),
            Some(i) => i,
        };
        let provider_id = entries[pi].0.clone();

        loop {
            let (selected_name, was_enabled, env_key, doc_url) = {
                let sel = core::find_provider_by_id(doc, &provider_id);
                let sel = match sel {
                    Some(s) => s,
                    None => return Ok(changed),
                };
                let name = sel
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&provider_id)
                    .to_string();
                let enabled = crate::json_utils::get_bool_map(&sel, "enabled");
                let env = crate::core::provider_env_key_from_json(&sel);
                let doc = sel
                    .get("doc")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                (name, enabled, env, doc)
            };

            let actions = vec![
                "Configure Models".to_string(),
                format!(
                    "{} provider",
                    if was_enabled { "Disable" } else { "Enable" }
                ),
                "Delete Provider".to_string(),
                "Back".to_string(),
            ];
            let mut footer_parts: Vec<String> = Vec::new();
            if !doc_url.is_empty() {
                footer_parts
                    .push("Provider docs: official documentation for this provider:".to_string());
                footer_parts.push(doc_url.clone());
            }
            if !env_key.is_empty() {
                footer_parts.push(format!(
                    "Required env var: {}",
                    crate::env::vars::env_key_masked_display(&env_key)
                ));
            }
            let footer = if footer_parts.is_empty() {
                None
            } else {
                Some(footer_parts.join("\n"))
            };
            let ai = numbered_select(
                &actions,
                Some(&format!("Provider: {selected_name}  (1-4)")),
                true,
                footer.as_deref(),
            )?;
            let ai = match ai {
                None => break,
                Some(i) => i,
            };
            if actions[ai] == "Back" {
                break;
            }
            match ai {
                0 => {
                    let mut sel = core::find_provider_by_id(doc, &provider_id).unwrap();
                    if config_models(&provider_id, doc, &mut sel)? {
                        changed = true;
                    }
                }
                1 => {
                    let enabled = !was_enabled;
                    crate::providers::set_provider_enabled(doc, &provider_id, enabled)?;
                    let verb = if was_enabled { "Disabled" } else { "Enabled" };
                    println!("{verb} provider '{provider_id}'.");
                    changed = true;
                }
                2 => {
                    let display = core::find_provider_by_id(doc, &provider_id)
                        .map(|p| crate::core::provider_display(&p))
                        .unwrap_or_else(|| format!("({provider_id}) - {provider_id}"));
                    if confirm_delete(&display)? {
                        let written =
                            crate::providers::delete_provider_and_flush(doc, &provider_id)?;
                        crate::sync::print_config_warnings(&written);
                        println!("Deleted Provider {display}.");
                        changed = true;
                    }
                    break;
                }
                _ => break,
            }
        }
    }
}

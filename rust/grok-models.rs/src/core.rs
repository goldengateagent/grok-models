//! Domain helpers ported verbatim from grok-models.py.

use crate::{fail, Res};
use serde_json::{Map, Value};

/// `first_letter_cap`
pub fn first_letter_cap(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out: String = c.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
    }
}

/// First env var name for a raw models.dev entry (`api_env_key`).
pub fn api_env_key(pinfo: &Value) -> String {
    match pinfo.get("env") {
        Some(Value::Array(list)) if !list.is_empty() => {
            if let Value::String(s) = &list[0] {
                s.clone()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Stored env var name from a providers.json entry (`first_env_key`).
pub fn first_env_key(provider: &Value) -> String {
    match provider.get("env_key") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Dots, slashes, and colons nest/break TOML bare keys; Grok table keys use '_'.
pub fn table_model_id(provider_id: &str, live_id: &str) -> String {
    let safe = live_id
        .replace('.', "_")
        .replace('/', "_")
        .replace(':', "_");
    format!("{provider_id}-{safe}")
}

/// `parse_bool` — accepts the same word sets.
pub fn parse_bool(raw: &str) -> Option<bool> {
    let s = raw.trim().to_lowercase();
    match s.as_str() {
        "y" | "yes" | "true" | "1" | "on" | "enable" | "enabled" => Some(true),
        "n" | "no" | "false" | "0" | "off" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

/// Python repr() of a string, used in messages like `{id!r}`.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp <= 0xff {
                    out.push_str(&format!("\\x{cp:02x}"));
                } else if cp <= 0xffff {
                    out.push_str(&format!("\\u{cp:04x}"));
                } else {
                    out.push_str(&format!("\\U{cp:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Result of `_sort_model_indices`.
pub struct SortedIndices {
    pub filtered: Vec<usize>,
    pub enabled_count: usize,
    pub free_disabled_count: usize,
}

fn model_enabled(models: &Map<String, Value>, mid: &str) -> bool {
    // Non-dict entries count as disabled, exactly like Python's isinstance gate.
    match models.get(mid) {
        Some(v) if v.is_object() => crate::get_bool_val(v, "enabled", true),
        _ => false,
    }
}

fn is_free(mid: &str) -> bool {
    mid.to_lowercase().contains("free")
}

fn model_display_name(models: &Map<String, Value>, mid: &str) -> String {
    match models.get(mid) {
        Some(v) => crate::name_or(v, mid),
        None => mid.to_string(),
    }
}

/// Enabled first, then free models, then alphabetical by display name
/// (id as tiebreaker). Optional substring filter matches model id or display name.
pub fn sort_model_indices(
    ids: &[String],
    models: &Map<String, Value>,
    filter_query: Option<&str>,
) -> SortedIndices {
    let filter_lower = filter_query.map(|q| q.to_lowercase());
    let mut base: Vec<usize> = ids
        .iter()
        .enumerate()
        .filter(|(_, id)| match &filter_lower {
            None => true,
            Some(q) => {
                id.to_lowercase().contains(q)
                    || model_display_name(models, id).to_lowercase().contains(q)
            }
        })
        .map(|(i, _)| i)
        .collect();

    let key_of = |mid: &str| -> (u8, u8, String, String) {
        (
            if model_enabled(models, mid) { 0 } else { 1 },
            if is_free(mid) { 0 } else { 1 },
            model_display_name(models, mid).to_lowercase(),
            mid.to_lowercase(),
        )
    };
    base.sort_by(|&a, &b| key_of(&ids[a]).cmp(&key_of(&ids[b])));

    let enabled_count = base.iter().filter(|&&i| model_enabled(models, &ids[i])).count();
    let free_disabled_count = base[enabled_count.min(base.len())..]
        .iter()
        .filter(|&&i| is_free(&ids[i]))
        .count();
    SortedIndices {
        filtered: base,
        enabled_count,
        free_disabled_count,
    }
}

/// `efforts_from_models_dev`: reasoning_options type=effort rows.
pub fn efforts_from_models_dev(minfo: &Value) -> Option<Vec<Map<String, Value>>> {
    let options = minfo.get("reasoning_options").and_then(|v| v.as_array())?;
    let values = options
        .iter()
        .filter_map(|opt| opt.as_object())
        .find(|o| o.get("type").and_then(Value::as_str) == Some("effort"))
        .and_then(|o| o.get("values"))
        .and_then(Value::as_array)?
        .clone();
    if values.is_empty() {
        return None;
    }
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for val in &values {
        let val_s = value_to_string(val)?;
        let mut row = Map::new();
        row.insert("id".into(), Value::String(val_s.clone()));
        row.insert("value".into(), Value::String(val_s.clone()));
        row.insert(
            "label".into(),
            Value::String(format!("{} Effort", first_letter_cap(&val_s))),
        );
        row.insert("default".into(), Value::Bool(false));
        rows.push(row);
    }
    if rows.is_empty() {
        return None;
    }
    let idx = rows.iter().position(|r| r["value"].as_str() != Some("none")).unwrap_or(0);
    rows[idx].insert("default".into(), Value::Bool(true));
    Some(rows)
}

fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// models.dev `limit.context` as an integer under Python `int(context)`
/// semantics (bools excluded, floats truncated). None when absent/non-numeric.
pub fn context_window_field(minfo: &Value) -> Option<Value> {
    let limit = minfo.get("limit").and_then(Value::as_object)?;
    let ctx = limit.get("context");
    let is_number = matches!(ctx, Some(Value::Number(_)));
    if !is_number {
        return None;
    }
    let n = ctx.unwrap().as_number().unwrap();
    let int_val: i64 = if let Some(i) = n.as_i64() {
        i
    } else if let Some(u) = n.as_u64() {
        u.clamp(0, i64::MAX as u64) as i64
    } else if let Some(f) = n.as_f64() {
        f.trunc() as i64
    } else {
        0
    };
    Some(Value::Number(int_val.into()))
}

/// `build_fields`: map a models.dev model entry to Grok Build [model.*] fields.
/// `include_descriptions` gates the trailing `description` field.
pub fn build_fields(
    model_id: &str,
    minfo: &Value,
    base_url: &str,
    env_key: &str,
    provider_name: &str,
    stored_name: Option<&str>,
    include_descriptions: bool,
) -> Res<Map<String, Value>> {
    let mut fields = Map::new();
    fields.insert("model".into(), Value::String(model_id.to_string()));
    fields.insert("base_url".into(), Value::String(base_url.to_string()));
    let name = match stored_name {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match minfo.get("name") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => first_letter_cap(model_id),
        },
    };
    fields.insert(
        "name".into(),
        Value::String(format!("{name} ({provider_name})")),
    );
    fields.insert("env_key".into(), Value::String(env_key.to_string()));
    fields.insert("api_backend".into(), Value::String("chat_completions".into()));

    if let Some(ctx) = context_window_field(minfo) {
        fields.insert("context_window".into(), ctx);
    }

    if crate::truthy(minfo.get("reasoning")) {
        match efforts_from_models_dev(minfo) {
            Some(efforts) => {
                let default_idx = efforts
                    .iter()
                    .position(|row| crate::get_bool_val(&Value::Object(row.clone()), "default", false))
                    .unwrap_or(0);
                let default_value = efforts[default_idx].get("value").cloned().unwrap_or(Value::Null);
                fields.insert("supports_reasoning_effort".into(), Value::Bool(true));
                fields.insert(
                    "reasoning_efforts".into(),
                    Value::Array(efforts.into_iter().map(Value::Object).collect()),
                );
                fields.insert("reasoning_effort".into(), default_value);
            }
            None => {
                fields.insert("supports_reasoning_effort".into(), Value::Bool(true));
            }
        }
    }
    if include_descriptions {
        if let Some(desc) = crate::jsonio::catalog_description(minfo) {
            fields.insert("description".into(), Value::String(desc.to_string()));
        }
    }
    Ok(fields)
}

/// The enabled model ids of a provider entry from providers.json.
pub fn enabled_model_ids(provider: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(models) = provider.get("models").and_then(Value::as_object) {
        for (mid, m) in models {
            let enabled =
                crate::get_bool_val(&Value::Object(m.as_object().cloned().unwrap_or_default()), "enabled", true);
            if enabled {
                out.push(mid.clone());
            }
        }
    }
    out
}

/// `_provider_label`
pub fn provider_label(p: &Value) -> String {
    let state = if p.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
        "enabled"
    } else {
        "disabled"
    };
    let pid = p["id"].as_str().unwrap_or_default();
    let name = p.get("name").and_then(Value::as_str).unwrap_or(pid);
    format!("({name}) - {pid} [{state}]")
}

/// `_env_value`: first 10 chars + ellipsis, quoted.
pub fn env_value(env_var: &str) -> String {
    let val = std::env::var(env_var).unwrap_or_default();
    if val.is_empty() {
        "\"\"".to_string()
    } else {
        format!("\"{}...\"", truncate_chars(&val, 10))
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `_env_status_line`: `ENV_VAR = "prefix..."`.
pub fn env_status_line(env_var: &str) -> String {
    if env_var.is_empty() {
        return String::new();
    }
    let val = std::env::var(env_var).unwrap_or_default();
    let shown = if val.is_empty() {
        "\"\"".to_string()
    } else {
        format!("\"{}...\"", truncate_chars(&val, 10))
    };
    format!("{env_var} = {shown}")
}

/// Required API-key env vars for all enabled providers, doc order, deduped.
pub fn enabled_provider_env_vars(providers_doc: &Value) -> Vec<String> {
    let mut env_vars: Vec<String> = Vec::new();
    let empty = Vec::new();
    let providers = providers_doc
        .get("providers")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for p in providers {
        if !p.is_object() {
            continue;
        }
        let pid_ok = p.get("id").is_some_and(|v| !v.is_null());
        if !pid_ok {
            continue;
        }
        if !p.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
            continue;
        }
        let env = first_env_key(p);
        if !env.is_empty() && !env_vars.contains(&env) {
            env_vars.push(env);
        }
    }
    env_vars
}

/// Guard used where Python would raise on a missing id field.
pub fn require_id(p: &Value) -> Res<&str> {
    match p.get("id").and_then(Value::as_str) {
        Some(s) => Ok(s),
        None => fail("providers.json entry missing 'id'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_label_swaps_name_and_id() {
        let p = json!({"id": "opencode-go", "name": "OpenCode Go", "enabled": true});
        assert_eq!(
            provider_label(&p),
            "(OpenCode Go) - opencode-go [enabled]"
        );
        let p = json!({"id": "x", "name": "X", "enabled": false});
        assert_eq!(provider_label(&p), "(X) - x [disabled]");
    }

    #[test]
    fn sort_model_indices_enabled_first_alpha_by_name_filters_name_or_id() {
        let ids = vec!["z-id".into(), "a-free".into(), "m-mid".into()];
        let models = json!({
            "z-id": {"name": "Alpha", "enabled": true},
            "a-free": {"name": "Zeta Free", "enabled": false},
            "m-mid": {"name": "Beta", "enabled": false},
        })
        .as_object()
        .unwrap()
        .clone();
        let sorted = sort_model_indices(&ids, &models, None);
        let ordered: Vec<&str> = sorted.filtered.iter().map(|&i| ids[i].as_str()).collect();
        assert_eq!(ordered, ["z-id", "a-free", "m-mid"]);
        assert_eq!(sorted.enabled_count, 1);
        assert_eq!(sorted.free_disabled_count, 1);

        let by_name = sort_model_indices(&ids, &models, Some("alpha"));
        assert_eq!(by_name.filtered.len(), 1);
        assert_eq!(ids[by_name.filtered[0]], "z-id");

        let by_id = sort_model_indices(&ids, &models, Some("a-free"));
        assert_eq!(by_id.filtered.len(), 1);
        assert_eq!(ids[by_id.filtered[0]], "a-free");
    }
}

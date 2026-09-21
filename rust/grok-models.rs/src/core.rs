//! Domain helpers ported verbatim from grok-models.py.

use crate::{Res, fail};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// `env[0]` on a models.dev provider object, or `""`.
pub fn provider_env_key_from_api(provider_models_dev: &Value) -> String {
    match provider_models_dev.get("env") {
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

/// String at `key` on a JSON object, or `""`.
pub fn get_json_str(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `env_key` on a providers.json provider entry, or `""`.
pub fn provider_env_key_from_json(p: &Map<String, Value>) -> String {
    get_json_str(p, "env_key")
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

/// Result of `_sort_model_indices`.
pub struct SortedIndices {
    pub filtered: Vec<usize>,
    pub enabled_count: usize,
    pub free_disabled_count: usize,
}

fn model_enabled(models: &Map<String, Value>, mid: &str) -> bool {
    // Non-dict entries count as disabled, exactly like Python's isinstance gate.
    match models.get(mid) {
        Some(v) if v.is_object() => crate::json_utils::get_bool_value(v, "enabled"),
        _ => false,
    }
}

fn is_free(mid: &str) -> bool {
    mid.to_lowercase().contains("free")
}

fn model_display_name(models: &Map<String, Value>, mid: &str) -> String {
    match models.get(mid) {
        Some(v) => crate::json_utils::get_name_or(v, mid),
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

    let enabled_count = base
        .iter()
        .filter(|&&i| model_enabled(models, &ids[i]))
        .count();
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
    let idx = rows
        .iter()
        .position(|r| r["value"].as_str() != Some("none"))
        .unwrap_or(0);
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
    fields.insert(
        "api_backend".into(),
        Value::String("chat_completions".into()),
    );

    if let Some(ctx) = context_window_field(minfo) {
        fields.insert("context_window".into(), ctx);
    }

    if crate::json_utils::is_truthy(minfo.get("reasoning")) {
        match efforts_from_models_dev(minfo) {
            Some(efforts) => {
                let default_idx = efforts
                    .iter()
                    .position(|row| crate::json_utils::get_bool_map(row, "default"))
                    .unwrap_or(0);
                let default_value = efforts[default_idx]
                    .get("value")
                    .cloned()
                    .unwrap_or(Value::Null);
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

/// Provider entries from providers.json that are objects with a non-null id.
pub fn provider_entries(doc: &Value) -> Vec<Map<String, Value>> {
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

/// A provider entry from providers.json by id, cloned.
pub fn find_provider_by_id(doc: &Value, provider_id: &str) -> Option<Map<String, Value>> {
    doc.get("providers")?
        .as_array()?
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(provider_id))
        .and_then(|p| p.as_object().cloned())
}

/// A provider entry from providers.json by id, borrowed mutably.
pub fn find_provider_by_id_mut<'a>(
    doc: &'a mut Value,
    provider_id: &str,
) -> Option<&'a mut Map<String, Value>> {
    doc.get_mut("providers")?
        .as_array_mut()?
        .iter_mut()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(provider_id))
        .and_then(Value::as_object_mut)
}

/// The enabled model ids of a provider entry from providers.json.
pub fn enabled_model_ids(provider: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(models) = provider.get("models").and_then(Value::as_object) {
        for (model_id, model) in models {
            let enabled = crate::json_utils::get_bool_value(model, "enabled");
            if enabled {
                out.push(model_id.clone());
            }
        }
    }
    out
}

/// `_provider_label`
pub fn provider_label(provider: &serde_json::Map<String, Value>) -> String {
    let state = if crate::json_utils::get_bool_map(provider, "enabled") {
        "enabled"
    } else {
        "disabled"
    };
    let provider_id = provider
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = provider
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(provider_id);
    format!("({name}) - {provider_id} [{state}]")
}

/// Main-list identity: `(name) - id`.
pub fn provider_display(provider: &serde_json::Map<String, Value>) -> String {
    let provider_id = provider
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = provider
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(provider_id);
    format!("({name}) - {provider_id}")
}

pub const PROVIDER_NAME_COL_MAX: usize = 25;
pub const MAIN_PROVIDER_NAME_COL_MAX: usize = 15;

fn clipped_paren_name(name: &str, max: usize) -> String {
    format!("({})", name.chars().take(max).collect::<String>())
}

/// Padded `(name) id [enabled/disabled]` rows (no env cell).
pub fn format_provider_id_rows(rows: &[(String, String, bool)]) -> Vec<String> {
    let names: Vec<String> = rows
        .iter()
        .map(|(name, _, _)| clipped_paren_name(name, PROVIDER_NAME_COL_MAX))
        .collect();
    let name_w = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let id_w = rows.iter().map(|(_, pid, _)| pid.len()).max().unwrap_or(0);
    let token_col = if rows.is_empty() {
        0
    } else {
        name_w + 1 + id_w + 1
    };
    names
        .iter()
        .zip(rows.iter())
        .map(|(nlab, (_, pid, enabled))| {
            let token = if *enabled { "[enabled]" } else { "[disabled]" };
            let head = format!("{:<name_w$} {:<id_w$}", nlab, pid);
            format!("{:<token_col$}{token}", head)
        })
        .collect()
}

/// `[disabled]` is the longer state token; pad `[enabled]` to this width so
/// the env column starts on one vertical line.
pub const PROVIDER_TOKEN_W: usize = 10;
/// Gap between the padded `[enabled]`/`[disabled]` token and the env box.
pub const PROVIDER_ENV_GAP: usize = 2;
/// Left/right inner padding of the env black box, in columns.
pub const PROVIDER_ENV_PAD: i32 = 1;
pub const MODEL_DESC_LABEL: &str = "Model Descriptions";
pub const WEB_SEARCH_LABEL: &str = "Web Search";
pub const CODEX_CONFIG_LABEL: &str = "Codex Config";
pub const UPDATE_LIST_LABEL: &str = "Update Model List";
pub const SYNC_CONFIG_LABEL: &str = "Sync Model Config";

/// Env-cell text on a main-menu provider row (`ENV = value`), if any.
pub fn provider_row_env_text(opt: &str) -> Option<&str> {
    for tok in ["[enabled]", "[disabled]"] {
        if let Some(p) = opt.find(tok) {
            let rest = opt[p + tok.len()..].trim_start_matches(' ');
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

/// Column where `[enabled]` / `[disabled]` / `[date]` start on the main menu.
/// Shared by provider rows and the Model Descriptions / Update Model List
/// trailing rows so the tokens form one vertical line.
pub fn provider_state_token_col(providers: &[Map<String, Value>]) -> usize {
    let name_w = providers
        .iter()
        .map(|p| {
            let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = p.get("name").and_then(Value::as_str).unwrap_or(pid);
            clipped_paren_name(name, MAIN_PROVIDER_NAME_COL_MAX).len()
        })
        .max()
        .unwrap_or(0);
    let id_w = providers
        .iter()
        .map(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .len()
        })
        .max()
        .unwrap_or(0);
    let provider_col = if providers.is_empty() {
        0
    } else {
        // "{name} - {id} " then token
        name_w + 3 + id_w + 1
    };
    provider_col
        .max(MODEL_DESC_LABEL.len() + 1)
        .max(WEB_SEARCH_LABEL.len() + 1)
        .max(CODEX_CONFIG_LABEL.len() + 1)
        .max(UPDATE_LIST_LABEL.len() + 1)
        .max(SYNC_CONFIG_LABEL.len() + 1)
}

pub fn pad_state_label(label: &str, token: &str, token_col: usize) -> String {
    let mut out = String::from(label);
    if out.len() < token_col {
        out.push_str(&" ".repeat(token_col - out.len()));
    }
    out.push_str(token);
    out
}

/// Padded main-menu provider rows: aligned dashes, aligned state tokens,
/// then a gap + env cell.
pub fn provider_menu_labels(providers: &[Map<String, Value>]) -> Vec<String> {
    let names: Vec<String> = providers
        .iter()
        .map(|p| {
            let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = p.get("name").and_then(Value::as_str).unwrap_or(pid);
            clipped_paren_name(name, MAIN_PROVIDER_NAME_COL_MAX)
        })
        .collect();
    let name_w = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let id_w = providers
        .iter()
        .map(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .len()
        })
        .max()
        .unwrap_or(0);
    let token_col = provider_state_token_col(providers);
    let env_w = providers
        .iter()
        .map(|p| provider_env_key_from_json(p).len())
        .max()
        .unwrap_or(0);
    names
        .iter()
        .zip(providers.iter())
        .map(|(name, p)| {
            let state = if p.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
                "enabled"
            } else {
                "disabled"
            };
            let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let token = format!("[{state}]");
            let head = format!("{:<name_w$} - {:<id_w$}", name, pid);
            let mut left = format!("{:<token_col$}{:<tw$}", head, token, tw = PROVIDER_TOKEN_W);
            let envk = provider_env_key_from_json(p);
            if !envk.is_empty() {
                left.push_str(&" ".repeat(PROVIDER_ENV_GAP));
                left.push_str(&format!("{envk:<env_w$} = "));
                left.push_str(&crate::env::vars::env_key_masked(&envk));
            }
            left
        })
        .collect()
}

/// Required API-key env vars for all enabled providers, doc order, deduped.
pub fn enabled_provider_env_vars(providers_doc: &Value) -> Vec<String> {
    let mut env_vars: Vec<String> = Vec::new();
    for p in provider_entries(providers_doc) {
        if !p.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
            continue;
        }
        let env = provider_env_key_from_json(&p);
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

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

// OpenCode picks the chars after the timestamp prefix from this alphabet.
const SESSION_ID_ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const SESSION_ID_RANDOM_LEN: usize = 10;
const SESSION_ID_SUFFIX: &str = "uwtb";

/// Fresh OpenCode session id: `ses_` + 12 hex + 10 base62 + `uwtb`.
///
/// `current` is ms-since-epoch shifted left 12 bits with the counter in the
/// low bits. Complementing it and keeping the low 6 bytes as big-endian hex
/// makes the time field order newest-first under plain string comparison.
pub fn new_session_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let current = ms.wrapping_shl(12).wrapping_add(counter);
    let stamp = format!("{:012x}", !current & 0xFFFF_FFFF_FFFF);

    // Discard bytes 248-255 so all 62 alphabet chars are equally likely.
    const LIMIT: u8 = 248; // 62 * 4
    let mut random_part = String::with_capacity(SESSION_ID_RANDOM_LEN);
    let mut buf = [0u8; 16];
    while random_part.len() < SESSION_ID_RANDOM_LEN {
        getrandom::getrandom(&mut buf).expect("OS random source unavailable");
        for b in buf {
            if b < LIMIT {
                random_part.push(SESSION_ID_ALPHABET[(b % 62) as usize] as char);
                if random_part.len() == SESSION_ID_RANDOM_LEN {
                    break;
                }
            }
        }
    }
    format!("ses_{stamp}{random_part}{SESSION_ID_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_label_swaps_name_and_id() {
        let p = json!({"id": "opencode-go", "name": "OpenCode Go", "enabled": true});
        assert_eq!(
            provider_label(p.as_object().unwrap()),
            "(OpenCode Go) - opencode-go [enabled]"
        );
        let p = json!({"id": "x", "name": "X", "enabled": false});
        assert_eq!(provider_label(p.as_object().unwrap()), "(X) - x [disabled]");
        assert_eq!(provider_display(p.as_object().unwrap()), "(X) - x");
        let rows = format_provider_id_rows(&[
            ("A".into(), "a".into(), true),
            ("Beta Name".into(), "long-id".into(), false),
        ]);
        let tok_a = rows[0].find('[').unwrap();
        let tok_b = rows[1].find('[').unwrap();
        assert_eq!(
            tok_a, tok_b,
            "state tokens must share a column:\n{}\n{}",
            rows[0], rows[1]
        );
        assert!(rows[0].starts_with("(A)"), "{}", rows[0]);
        assert!(rows[1].contains(" long-id"), "{}", rows[1]);
        assert!(!rows[1].contains(" - "), "{}", rows[1]);
        assert!(rows[0].ends_with("[enabled]"), "{}", rows[0]);
        assert!(rows[1].ends_with("[disabled]"), "{}", rows[1]);
    }

    #[test]
    fn provider_menu_labels_aligns_ids_tokens_and_env() {
        let a = json!({
            "id": "a", "name": "A", "enabled": true, "env_key": "A_KEY"
        })
        .as_object()
        .unwrap()
        .clone();
        let b = json!({
            "id": "long-id", "name": "Beta Name", "enabled": false, "env_key": "LONGER_API_KEY"
        })
        .as_object()
        .unwrap()
        .clone();
        let labels = provider_menu_labels(&[a.clone(), b.clone()]);
        let tok_a = labels[0].find('[').unwrap();
        let tok_b = labels[1].find('[').unwrap();
        assert_eq!(
            tok_a, tok_b,
            "state tokens must share a column:\n{}\n{}",
            labels[0], labels[1]
        );
        let env_a = labels[0].find("A_KEY").unwrap();
        let env_b = labels[1].find("LONGER_API_KEY").unwrap();
        assert_eq!(
            env_a, env_b,
            "env cells must share a column:\n{}\n{}",
            labels[0], labels[1]
        );
        assert_eq!(
            labels[0].find(" = "),
            labels[1].find(" = "),
            "equals must share a column:\n{}\n{}",
            labels[0],
            labels[1]
        );
        assert_eq!(
            &labels[0][tok_a..tok_a + PROVIDER_TOKEN_W],
            "[enabled] ",
            "[enabled] must pad to [disabled] width"
        );
        assert_eq!(&labels[1][tok_b..tok_b + PROVIDER_TOKEN_W], "[disabled]");
        let col = provider_state_token_col(&[a.clone(), b.clone()]);
        let desc = pad_state_label(MODEL_DESC_LABEL, "[enabled]", col);
        let upd = pad_state_label(UPDATE_LIST_LABEL, "[08-26-2026 03:15 PM]", col);
        let syn = pad_state_label(SYNC_CONFIG_LABEL, "[08-26-2026 03:15 PM]", col);
        assert_eq!(
            desc.find('['),
            Some(tok_a),
            "Model Descriptions token must line up"
        );
        assert_eq!(
            upd.find('['),
            Some(tok_a),
            "Update Model List token must line up"
        );
        assert_eq!(
            syn.find('['),
            Some(tok_a),
            "Sync Model Config token must line up"
        );
    }

    #[test]
    fn format_provider_id_rows_clips_name() {
        let rows = format_provider_id_rows(&[(
            "MiniMax Token Plan (minimaxi.com)".into(),
            "x".into(),
            true,
        )]);
        assert!(
            rows[0].starts_with("(MiniMax Token Plan (minim) "),
            "{}",
            rows[0]
        );
    }

    #[test]
    fn provider_menu_labels_clips_name() {
        let p = json!({
            "id": "x",
            "name": "MiniMax Token Plan (minimaxi.com)",
            "enabled": true,
        })
        .as_object()
        .unwrap()
        .clone();
        let labels = provider_menu_labels(&[p]);
        assert!(labels[0].starts_with("(MiniMax Token P) "), "{}", labels[0]);
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

    #[test]
    fn new_session_id_shape_and_uniqueness() {
        let a = new_session_id();
        let b = new_session_id();
        for id in [&a, &b] {
            assert!(id.starts_with("ses_"), "{id}");
            assert!(id.ends_with("uwtb"), "{id}");
            assert_eq!(id.len(), 30, "{id}"); // "ses_" + 12 + 10 + 4
            assert!(id[4..16].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
            assert!(
                id[16..26].chars().all(|c| c.is_ascii_alphanumeric()),
                "{id}"
            );
        }
        assert_ne!(a, b);
    }
}

//! Provider and catalog fetching over HTTPS.

use crate::Res;
use crate::providers::is_ollama_cloud_provider;
use serde_json::{Map, Value};
use std::collections::HashMap;
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// When true, add-provider and sync take model ids from GET {base_url}/models
/// (OpenAI list). When false, the models.dev provider `models` object is the list.
pub const USE_PROVIDER_MODELS_ENDPOINT: bool = true;
/// Local Ollama OpenAI-compatible endpoint; used instead of the models.dev `api`.
pub const OLLAMA_CLOUD_LOCAL_BASE_URL: &str = "http://127.0.0.1:11434/v1/";
/// Cloud catalog used after the regular /models update for ollama-cloud.
pub const OLLAMA_CLOUD_MODELS_BASE_URL: &str = "https://ollama.com/v1";
/// models.dev api.json: provider entries keyed by provider id.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelsDev {
    #[serde(flatten)]
    pub providers: HashMap<String, ModelsDevProvider>,
}

/// One models.dev provider entry.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelsDevProvider {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, ModelsDevModel>,
}

/// One models.dev model entry. Scalar fields are typed; `modalities`,
/// `limit`, `reasoning`, and `reasoning_options` keep tolerant shapes
/// because the code only probes them.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelsDevModel {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub provider: Option<ModelProviderRef>,
    #[serde(default)]
    pub modalities: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub limit: Option<Value>,
    #[serde(default)]
    pub reasoning: Option<Value>,
    #[serde(default)]
    pub reasoning_options: Vec<Value>,
}

/// Per-model provider override carrying an npm package.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelProviderRef {
    #[serde(default)]
    pub npm: Option<String>,
}

/// OpenAI-style `{ data: [{ id, name? }] }` list.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct OpenAiList {
    #[serde(default)]
    pub data: Vec<OpenAiModel>,
}

/// One OpenAI-style model row.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct OpenAiModel {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Fetch models.dev api.json over HTTPS (10s timeout).
pub fn fetch_models_dev() -> Res<ModelsDev> {
    crate::client::Client::new()
        .get_json(MODELS_DEV_URL)
        .map_err(|e| crate::Error::new(e.message))
}

/// Join a base URL to its OpenAI-style /models endpoint.
pub fn provider_models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// OpenAI-style `{ data: [{ id, name? }] }` rows. None if unusable/empty.
pub fn parse_openai_models_list(payload: &OpenAiList) -> Option<Vec<(String, Option<String>)>> {
    if payload.data.is_empty() {
        return None;
    }
    let mut items = Vec::new();
    for row in &payload.data {
        let Some(mid) = row.id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let name = row.name.clone().filter(|s| !s.is_empty());
        items.push((mid.to_string(), name));
    }
    if items.is_empty() { None } else { Some(items) }
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

/// True for HTTP 401/403 failures, which trigger the authenticated retry.
pub fn is_http_auth_error(err: &crate::Error) -> bool {
    err.message.starts_with("HTTP 401 ") || err.message.starts_with("HTTP 403 ")
}

/// Stored opt-in for sending Authorization on the first list attempt.
pub fn provider_auth_models_list(provider: &Map<String, Value>) -> bool {
    matches!(provider.get("auth_models_list"), Some(Value::Bool(true)))
}

/// Fetches `{base_url}/models`, retrying with the key on 401/403.
/// Sends Authorization when `auth_models_list` is true; public lists fetch
/// without a key because some hang with one. Returns rows or a failure URL.
pub fn try_fetch_provider_models(
    base_url: &str,
    env_key: &str,
    provider: &mut Map<String, Value>,
) -> (Option<Vec<(String, Option<String>)>>, Option<String>) {
    let url = if is_ollama_cloud_provider(provider) {
        provider_models_url(OLLAMA_CLOUD_MODELS_BASE_URL)
    } else if base_url.is_empty() {
        return (None, None);
    } else {
        provider_models_url(base_url)
    };
    try_fetch_models_url(&url, env_key, provider)
}

pub fn try_fetch_models_url(
    url: &str,
    env_key: &str,
    provider: &mut Map<String, Value>,
) -> (Option<Vec<(String, Option<String>)>>, Option<String>) {
    let use_auth = provider_auth_models_list(provider);
    let val = crate::env::vars::env_var_value(env_key);
    let key = if use_auth && !val.is_empty() {
        Some(val.as_str())
    } else {
        None
    };
    let payload: OpenAiList = match crate::client::Client::new()
        .get_json_with_key(&url, key)
        .map_err(|e| crate::Error::new(e.message))
    {
        Ok(payload) => payload,
        Err(e) => {
            if use_auth || !is_http_auth_error(&e) {
                return (None, Some(e.message));
            }
            if val.is_empty() {
                return (None, Some(e.message));
            }
            provider.insert("auth_models_list".into(), Value::Bool(true));
            match crate::client::Client::new()
                .get_json_with_key(&url, Some(&val))
                .map_err(|e| crate::Error::new(e.message))
            {
                Ok(payload) => payload,
                Err(retry_e) => return (None, Some(retry_e.message)),
            }
        }
    };
    match parse_openai_models_list(&payload) {
        None => (
            None,
            Some(format!("empty or invalid model list from {url}")),
        ),
        Some(items) => (Some(items), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(!provider_auth_models_list(&p));
        p.insert("auth_models_list".into(), Value::Bool(false));
        assert!(!provider_auth_models_list(&p));
        p.insert("auth_models_list".into(), Value::Bool(true));
        assert!(provider_auth_models_list(&p));
    }

    #[test]
    fn is_http_auth_error_matches_401_403_only() {
        assert!(is_http_auth_error(&crate::Error::new(
            "HTTP 401 fetching https://example/models: no"
        )));
        assert!(is_http_auth_error(&crate::Error::new(
            "HTTP 403 fetching https://example/models: no"
        )));
        assert!(!is_http_auth_error(&crate::Error::new(
            "HTTP 429 fetching https://example/models: slow"
        )));
        assert!(!is_http_auth_error(&crate::Error::new(
            "HTTP failure fetching https://example/models: timeout"
        )));
    }
}

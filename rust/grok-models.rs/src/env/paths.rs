//! GROK_HOME / CODEX_HOME file locations.
//!
//! Grok config resolves from `$GROK_HOME` when set, else `~/.grok/`.
//! Codex config resolves from `$CODEX_HOME` when set, else `~/.codex/`.

use super::vars::{CODEX_HOME_ENV, GROK_HOME_ENV, HOME_ENV, is_wsl};
use std::path::PathBuf;

/// `$HOME` when set, else `home` crate fallback, else `.`.
pub fn home_dir() -> PathBuf {
    if let Some(home) = nonempty_var(HOME_ENV) {
        return PathBuf::from(home);
    }
    // USERPROFILE on Windows; passwd fallback on Unix (`home` crate).
    if let Some(dir) = home::home_dir() {
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    PathBuf::from(".")
}

/// `$GROK_HOME/providers.json`, else `~/.grok/providers.json`.
pub fn providers_path() -> PathBuf {
    match nonempty_var(GROK_HOME_ENV) {
        Some(home) => PathBuf::from(home).join("providers.json"),
        None => home_dir().join(".grok").join("providers.json"),
    }
}

/// `$GROK_HOME/config.toml`, else `~/.grok/config.toml`.
pub fn config_toml_path() -> PathBuf {
    match nonempty_var(GROK_HOME_ENV) {
        Some(home) => PathBuf::from(home).join("config.toml"),
        None => home_dir().join(".grok").join("config.toml"),
    }
}

/// `$CODEX_HOME` if set and non-empty, else `~/.codex`.
pub fn codex_home() -> PathBuf {
    match nonempty_var(CODEX_HOME_ENV) {
        Some(home) => PathBuf::from(home),
        None => home_dir().join(".codex"),
    }
}

/// `config.toml` next to the Codex home directory.
pub fn codex_config_toml_path() -> PathBuf {
    codex_home().join("config.toml")
}

/// Catalog file next to config.toml: `$CODEX_HOME/<id>-models.json` or
/// `~/.codex/<id>-models.json`.
pub fn codex_models_json_path(provider_id: &str) -> PathBuf {
    codex_home().join(format!("{provider_id}-models.json"))
}

/// TOML `model_catalog_json` value matching `codex_models_json_path`.
/// WSL: always `~/...` — Windows Codex does not expand `$CODEX_HOME`.
pub fn codex_models_json_toml_value(provider_id: &str) -> String {
    if is_wsl() {
        return format!("~/.codex/{provider_id}-models.json");
    }
    if nonempty_var(CODEX_HOME_ENV).is_some() {
        format!("${CODEX_HOME_ENV}/{provider_id}-models.json")
    } else {
        format!("~/.codex/{provider_id}-models.json")
    }
}

fn nonempty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

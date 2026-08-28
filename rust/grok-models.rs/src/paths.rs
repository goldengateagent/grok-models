//! Path resolution.
//!
//! `providers.json` lives in the Grok home config directory so the binary
//! behaves identically no matter where it is run from: `$GROK_HOME` when set,
//! otherwise `~/.grok/`. The config path logic is the same:
//! `$GROK_HOME/config.toml` or `~/.grok/config.toml`.

use std::path::PathBuf;

pub fn providers_path() -> PathBuf {
    // Standalone binary: providers.json is resolved from the Grok home config
    // dir, never from the executable's directory or the current working
    // directory.
    match std::env::var("GROK_HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join("providers.json"),
        _ => home_dir().join(".grok").join("providers.json"),
    }
}

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    // Fall back to the passwd entry like Python's Path.home().
    unsafe {
        let uid = libc::getuid();
        let pw = libc::getpwuid(uid);
        if !pw.is_null() {
            let dir = (*pw).pw_dir;
            if !dir.is_null() {
                let cstr = std::ffi::CStr::from_ptr(dir);
                if let Ok(s) = cstr.to_str() {
                    if !s.is_empty() {
                        return PathBuf::from(s);
                    }
                }
            }
        }
    }
    PathBuf::from(".")
}

pub fn config_toml_path() -> PathBuf {
    match std::env::var("GROK_HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join("config.toml"),
        _ => home_dir().join(".grok").join("config.toml"),
    }
}

/// `$CODEX_HOME` if set and non-empty, else `~/.codex`.
pub fn codex_home() -> PathBuf {
    match std::env::var("CODEX_HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home),
        _ => home_dir().join(".codex"),
    }
}

pub fn codex_config_toml_path() -> PathBuf {
    codex_home().join("config.toml")
}

/// Catalog file next to config.toml: `$CODEX_HOME/<id>-models.json` or
/// `~/.codex/<id>-models.json`.
pub fn codex_models_json_path(provider_id: &str) -> PathBuf {
    codex_home().join(format!("{provider_id}-models.json"))
}

/// TOML `model_catalog_json` value matching `codex_models_json_path`.
pub fn codex_models_json_toml_value(provider_id: &str) -> String {
    match std::env::var("CODEX_HOME") {
        Ok(home) if !home.is_empty() => {
            let _home = home;
            format!("$CODEX_HOME/{provider_id}-models.json")
        }
        _ => format!("~/.codex/{provider_id}-models.json"),
    }
}

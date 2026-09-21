//! Process environment reads and masked key previews.
//!
//! `env_var_value` is WSL-aware: on WSL the Windows environment is read via
//! `powershell.exe`, otherwise the process environment is used directly.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Env var holding the Grok home dir.
pub const GROK_HOME_ENV: &str = "GROK_HOME";
/// Env var holding the Codex home dir.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";
/// Env var holding the user home dir.
pub const HOME_ENV: &str = "HOME";

/// WSL if /proc kernel strings contain microsoft.
pub fn is_wsl() -> bool {
    static IS_WSL: OnceLock<bool> = OnceLock::new();
    *IS_WSL.get_or_init(|| {
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
        #[cfg(target_os = "linux")]
        {
            fn has_microsoft(path: &str) -> bool {
                std::fs::read_to_string(path)
                    .map(|s| s.to_ascii_lowercase().contains("microsoft"))
                    .unwrap_or(false)
            }
            has_microsoft("/proc/version") || has_microsoft("/proc/sys/kernel/osrelease")
        }
    })
}

/// Process value of a named env var; unset → `""`. On WSL, the Windows env.
pub fn env_var_value(name: &str) -> String {
    if is_wsl() {
        get_windows_env_var(name)
    } else {
        std::env::var(name).unwrap_or_default()
    }
}

/// Quoted 10-char preview of the named key; `""` when unset or empty.
pub fn env_key_masked(env_key: &str) -> String {
    let val = env_var_value(env_key);
    if val.is_empty() {
        String::from("\"\"")
    } else {
        let mut out = String::with_capacity(16);
        out.push('"');
        out.extend(val.chars().take(10));
        out.push_str("...\"");
        out
    }
}

/// `NAME = "preview..."` for the named key.
pub fn env_key_masked_display(env_key: &str) -> String {
    format!("{env_key} = {}", env_key_masked(env_key))
}

/// Windows process env via powershell.exe. Cached per name for the process.
fn get_windows_env_var(name: &str) -> String {
    let mut bytes = name.bytes();
    let valid = match bytes.next() {
        Some(b'A'..=b'Z' | b'a'..=b'z' | b'_') => {
            bytes.all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
        }
        _ => false,
    };
    if !valid {
        return String::new();
    }
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(name).cloned() {
        return v;
    }
    let cmd = format!("[Environment]::GetEnvironmentVariable('{name}')");
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &cmd])
        .stdin(std::process::Stdio::null())
        .output();
    let value = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    };
    cache
        .lock()
        .unwrap()
        .insert(name.to_string(), value.clone());
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_home_helpers_point_env_at_fresh_dirs() {
        let homes = crate::env::test_support::TestHomes::setup();
        assert!(homes.grok_home.is_dir());
        assert!(homes.codex_home.is_dir());
        assert_ne!(homes.grok_home, homes.codex_home);
        assert_eq!(
            env_var_value(GROK_HOME_ENV),
            homes.grok_home.to_string_lossy()
        );
        assert_eq!(
            env_var_value(CODEX_HOME_ENV),
            homes.codex_home.to_string_lossy()
        );
    }
}

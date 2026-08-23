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

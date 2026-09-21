//! Test isolation for the process-global `GROK_HOME` and `CODEX_HOME`.
//!
//! Both homes are process-global, so tests that mutate them take `LOCK`
//! before writing env and hold the guard for the whole test.

use super::vars::{CODEX_HOME_ENV, GROK_HOME_ENV};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

/// Serialize env-mutating tests; hold the guard for the whole test.
pub fn grok_home_lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Pid-keyed temp dir shared by tests in this process; reset on setup.
pub fn test_grok_dir() -> PathBuf {
    std::env::temp_dir().join(format!("gm-test-grok-{}", std::process::id()))
}

/// Pid-keyed temp dir shared by tests in this process; reset on setup.
pub fn test_codex_dir() -> PathBuf {
    std::env::temp_dir().join(format!("gm-test-codex-{}", std::process::id()))
}

/// Best-effort wipe then recreate.
pub fn reset_test_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).expect("create test dir");
}

/// Point `GROK_HOME` at a fresh temp dir; caller must hold the lock.
pub fn set_test_grok_home() -> PathBuf {
    let dir = test_grok_dir();
    reset_test_dir(&dir);
    // Safe: the lock serializes env mutation across tests.
    unsafe { std::env::set_var(GROK_HOME_ENV, &dir) };
    dir
}

/// Point `CODEX_HOME` at a fresh temp dir; caller must hold the lock.
pub fn set_test_codex_home() -> PathBuf {
    let dir = test_codex_dir();
    reset_test_dir(&dir);
    // Safe: the lock serializes env mutation across tests.
    unsafe { std::env::set_var(CODEX_HOME_ENV, &dir) };
    dir
}

/// One-line bootstrap: locks env, freshens both homes, deletes both on drop.
pub struct TestHomes {
    _guard: MutexGuard<'static, ()>,
    pub grok_home: PathBuf,
    pub codex_home: PathBuf,
}

impl TestHomes {
    pub fn setup() -> Self {
        let guard = grok_home_lock();
        let grok_home = set_test_grok_home();
        let codex_home = set_test_codex_home();
        Self {
            _guard: guard,
            grok_home,
            codex_home,
        }
    }
}

impl Drop for TestHomes {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.grok_home);
        let _ = std::fs::remove_dir_all(&self.codex_home);
    }
}

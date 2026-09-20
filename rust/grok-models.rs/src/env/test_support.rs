//! Test isolation for the process-global `GROK_HOME`.
//!
//! Serializes tests that mutate the shared environment variable so parallel
//! threads never race over it.

use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

pub fn grok_home_lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

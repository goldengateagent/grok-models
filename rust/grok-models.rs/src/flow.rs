//! Interactive orchestration: Ratatui by default, the previous TUI with `--legacy`, numbered fallback when stdout is not a TTY.

use crate::Res;
use crate::{config_toml, core, env::paths, fallback, jsonio, sync, tui};

pub fn cmd_config() -> Res<i32> {
    cmd_config_inner(false)
}

/// Same interactive flow as [`cmd_config`], drawing the previous terminal interface.
pub fn cmd_config_legacy() -> Res<i32> {
    cmd_config_inner(true)
}

fn cmd_config_inner(legacy: bool) -> Res<i32> {
    let mut doc = jsonio::load_providers()?;
    let providers = core::provider_entries(&doc);

    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // The TUI can add the first provider itself (Add Provider); only the
    // numbered fallback bails on an empty providers.json.
    if !tty && providers.is_empty() {
        println!("No providers configured yet. Add with --add-provider");
        return Ok(0);
    }
    let changed = if tty {
        let tui_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if legacy {
                run_tui_safely(&mut doc)
            } else {
                crate::ratatui::run(&mut doc)
            }
        }));
        match tui_result {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                eprintln!("TUI failed: {e}");
                false
            }
            Err(_) => {
                eprintln!("TUI crashed");
                false
            }
        }
    } else {
        fallback::numbered_config_flow(&mut doc)?
    };
    let _ = providers;
    let _ = providers; // silence unused

    if changed {
        let written = config_toml::update_config_toml()?;
        sync::print_config_warnings(&written);
        sync::print_sync_report(&written.path, &doc);
        sync::print_relaunch();
    }
    Ok(0)
}

fn run_tui_safely(doc: &mut serde_json::Value) -> Res<bool> {
    let _ = paths::providers_path(); // touch path on this layout
    tui::run_config_flow(doc)
}

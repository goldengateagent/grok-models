//! `cmd_config` orchestration: TUI on a TTY, numbered fallback otherwise.

use crate::{fallback, jsonio, paths, sync, tui};
use crate::Res;

pub fn cmd_config() -> Res<i32> {
    let mut doc = jsonio::load_providers()?;
    let providers = usable(&doc);

    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // The TUI can add the first provider itself (➕ Add provider…); only the
    // numbered fallback bails on an empty providers.json.
    if !tty && providers.is_empty() {
        println!("No providers configured yet. Add with --add-provider");
        return Ok(0);
    }
    let changed = if tty {
        let tui_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tui_safely(&mut doc)
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
        let (path, stats) = sync::run_sync()?;
        if let Some(path) = path {
            sync::print_sync_report(&stats, &path, &doc);
            sync::print_relaunch();
        }
    }
    Ok(0)
}

fn run_tui_safely(doc: &mut serde_json::Value) -> Res<bool> {
    let _ = paths::providers_path(); // touch path on this layout
    tui::run_config_flow(doc)
}

fn usable(doc: &serde_json::Value) -> Vec<serde_json::Map<String, serde_json::Value>> {
    doc.get("providers")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|p| p.is_object() && p.get("id").is_some_and(|v| !v.is_null()))
                .filter_map(|p| p.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

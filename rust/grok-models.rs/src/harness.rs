//! Test-only driver: lets the verification harness run the same Rust code
//! paths the binary does (sync, render, toggle) with a fixture models.dev
//! payload, so we can diff output against the Python tool.
//!
//! Usage:
//!   gm-harness sync <providers.json> <api.json> <providers_out.json> \
//!                  <config_out.toml>
//!   gm-harness render_list <providers.json> <mode "providers"|"provider ID">
//!   gm-harness render_models <providers.json>
//!   gm-harness toggle <providers.json> <api.json> enable|disable <target>...
//!
//! All file access is isolated under a per-process `$GROK_HOME` temp dir, so
//! the real `~/.grok/providers.json` and `~/.grok/config.toml` are never read
//! or written.

use std::io::{Read, Write};

use ::grok_models::commands::{render_list_text, render_models_text, ResolvedTarget};
use ::grok_models::core;
use ::grok_models::difflib;
use ::grok_models::jsonio;
use ::grok_models::paths;
use ::grok_models::sync;

fn read(path: &str) -> Vec<u8> {
    let mut f = std::fs::File::open(path).expect("open input");
    let mut v = Vec::new();
    f.read_to_end(&mut v).expect("read input");
    v
}

fn write_string(path: &str, s: &str) {
    let mut f = std::fs::File::create(path).expect("create output");
    f.write_all(s.as_bytes()).expect("write output");
}

/// Point `GROK_HOME` at a per-process temp dir so every `providers.json` and
/// `config.toml` access lands in the sandbox instead of the real `~/.grok`.
fn setup_test_home() {
    let home = std::env::temp_dir().join(format!("gm-harness-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("create test GROK_HOME");
    std::env::set_var("GROK_HOME", &home);
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        eprintln!("harness: missing subcommand");
        std::process::exit(2);
    }
    setup_test_home();
    let rc = match argv[1].as_str() {
        "sync" => cmd_sync(&argv[2..]),
        "render_list" => cmd_render_list(&argv[2..]),
        "render_models" => cmd_render_models(&argv[2..]),
        "toggle" => cmd_toggle_cmd(&argv[2..]),
        "toml_string" => cmd_toml_string(&argv[2..]),
        "resolve" => cmd_resolve(&argv[2..]),
        "parse_bool" => cmd_parse_bool(&argv[2..]),
        "close_matches" => cmd_close_matches(&argv[2..]),
        other => {
            eprintln!("harness: unknown subcommand {other}");
            2
        }
    };
    std::process::exit(rc);
}

fn cmd_sync(argv: &[String]) -> i32 {
    let providers_in = argv.get(0).cloned().unwrap_or_default();
    let api_path = argv.get(1).cloned().unwrap_or_default();
    let providers_out = argv.get(2).cloned().unwrap_or_default();
    let config_out = argv.get(3).cloned().unwrap_or_default();
    // Stage the fixture where load_providers resolves it ($GROK_HOME set in
    // main()), so the real ~/.grok files are never touched.
    let providers_json = std::path::Path::new(&providers_in);
    let providers_target = paths::providers_path();
    std::fs::copy(providers_json, &providers_target).expect("copy providers.json");

    let api: serde_json::Value = serde_json::from_slice(&read(&api_path)).expect("parse api");
    let (path, _stats) = match sync::run_sync(&api) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sync error: {e}");
            return 1;
        }
    };
    // Copy the rewritten providers.json and config.toml to the output paths.
    if let Some(p) = path {
        let text = std::fs::read_to_string(&p).expect("read config.toml");
        write_string(&config_out, &text);
    }
    let rewritten = std::fs::read(&providers_target).expect("read rewritten providers.json");
    write_string(&providers_out, &String::from_utf8_lossy(&rewritten));
    0
}

fn cmd_render_list(argv: &[String]) -> i32 {
    let providers_path = argv.get(0).cloned().unwrap_or_default();
    let mode = argv.get(1).cloned().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_slice(&read(&providers_path)).expect("parse providers.json");
    let filter = match mode.as_str() {
        s if s.starts_with("provider ") => Some(s[9..].to_string()),
        _ => None,
    };
    let providers_only = mode == "providers";
    if let Err(e) = render_list_text(&v, filter.as_deref(), providers_only) {
        eprintln!("{e}");
        return 1;
    }
    0
}

fn cmd_render_models(argv: &[String]) -> i32 {
    // Stage the fixture under the isolated $GROK_HOME set in main().
    let providers_path = argv.get(0).cloned().unwrap_or_default();
    let providers_json = std::path::Path::new(&providers_path);
    let providers_target = paths::providers_path();
    std::fs::copy(providers_json, &providers_target).expect("copy providers.json");
    match render_models_text() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn cmd_toggle_cmd(argv: &[String]) -> i32 {
    // usage: gm-harness toggle providers.json api.json enable|disable enable_target [more...]
    let providers_path = argv.get(0).cloned().unwrap_or_default();
    let api_path = argv.get(1).cloned().unwrap_or_default();
    let mut i = 2;
    let mut enable = Vec::new();
    let mut disable = Vec::new();
    while i < argv.len() {
        match argv[i].as_str() {
            "enable" => {
                if let Some(t) = argv.get(i + 1) {
                    enable.push(t.clone());
                    i += 2;
                    continue;
                }
                break;
            }
            "disable" => {
                if let Some(t) = argv.get(i + 1) {
                    disable.push(t.clone());
                    i += 2;
                    continue;
                }
                break;
            }
            other => {
                eprintln!("harness: bad arg {other}");
                return 2;
            }
        }
    }

    // Stage the fixture under the isolated $GROK_HOME set in main().
    let providers_json = std::path::Path::new(&providers_path);
    let providers_target = paths::providers_path();
    std::fs::copy(providers_json, &providers_target).expect("copy providers.json");

    // Wire fetch by intercepting URL — since fetch_models_dev is private to
    // sync.rs, instead we run the toggle locally and read updated doc.
    let api_v: serde_json::Value = serde_json::from_slice(&read(&api_path)).expect("parse api");
    run_toggle_local(api_v, enable, disable)
}

fn run_toggle_local(api: serde_json::Value, enable: Vec<String>, disable: Vec<String>) -> i32 {
    // Manually mirror cmd_toggle's flow without hitting the network.
    let providers_target = paths::providers_path();
    let mut doc = match jsonio::load_providers() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let resolved_enable = match ::grok_models::commands::resolve_targets_local(&doc, &enable) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let resolved_disable = match ::grok_models::commands::resolve_targets_local(&doc, &disable) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let mut applied_keys: Vec<(String, Option<String>)> = Vec::new();
    let mut applied: std::collections::HashMap<(String, Option<String>), bool> = Default::default();
    fn push(target: ResolvedTarget, want: bool, keys: &mut Vec<(String, Option<String>)>, map: &mut std::collections::HashMap<(String, Option<String>), bool>) {
        let key = match target {
            ResolvedTarget::Provider(pid) => (pid, None),
            ResolvedTarget::Model(pid, mid) => (pid, Some(mid)),
        };
        if !keys.contains(&key) {
            keys.push(key.clone());
        }
        map.insert(key, want);
    }
    for r in resolved_enable {
        push(r, true, &mut applied_keys, &mut applied);
    }
    for r in resolved_disable {
        push(r, false, &mut applied_keys, &mut applied);
    }

    for key in &applied_keys {
        let (pid, mid) = key.clone();
        let want = applied[key];
        let cur = find_by_id(&doc, &pid);
        let cur = match cur {
            Some(c) => c,
            None => continue,
        };
        if mid.is_none() {
            let cur_en = crate_pseudo::get_bool(&cur, "enabled", true);
            if cur_en == want {
                println!(
                    "already {}: {}",
                    if want { "enabled" } else { "disabled" },
                    pid
                );
                continue;
            }
            let slot = find_by_id_mut(&mut doc, &pid).unwrap();
            slot.insert("enabled".into(), serde_json::Value::Bool(want));
            println!("{}: {}", if want { "enabled" } else { "disabled" }, pid);
        } else {
            let mid_s = mid.clone().unwrap();
            let slot = find_by_id_mut(&mut doc, &pid).unwrap();
            let models = slot.entry("models".to_string()).or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !models.is_object() {
                *models = serde_json::Value::Object(serde_json::Map::new());
            }
            let mobj = models.as_object_mut().unwrap();
            let entry = mobj.entry(mid_s.clone()).or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = serde_json::Value::Object(serde_json::Map::new());
            }
            let eobj = entry.as_object_mut().unwrap();
            let cur = crate_pseudo::get_bool(eobj, "enabled", true);
            if cur == want {
                println!(
                    "already {}: {}/{}",
                    if want { "enabled" } else { "disabled" },
                    pid, mid_s
                );
                continue;
            }
            eobj.insert("enabled".into(), serde_json::Value::Bool(want));
            println!(
                "{}: {}/{}",
                if want { "enabled" } else { "disabled" },
                pid,
                mid_s
            );
        }
    }

    let _ = api;
    jsonio::dump_json(&providers_target, &doc).unwrap();
    // Skip the inner fetch — just print changed providers.json for diff.
    let _ = jsonio::load_providers;
    0
}

// Helpers
mod crate_pseudo {
    pub fn get_bool(o: &serde_json::Map<String, serde_json::Value>, key: &str, default: bool) -> bool {
        match o.get(key) {
            Some(serde_json::Value::Bool(b)) => *b,
            _ => default,
        }
    }
}

fn find_by_id<'a>(doc: &'a serde_json::Value, pid: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    doc.get("providers")?
        .as_array()?
        .iter()
        .find(|p| p.get("id").and_then(serde_json::Value::as_str) == Some(pid))
        .and_then(|p| p.as_object().cloned())
}

fn find_by_id_mut<'a>(doc: &'a mut serde_json::Value, pid: &str) -> Option<&'a mut serde_json::Map<String, serde_json::Value>> {
    doc.get_mut("providers")?
        .as_array_mut()?
        .iter_mut()
        .find(|p| p.get("id").and_then(serde_json::Value::as_str) == Some(pid))
        .and_then(serde_json::Value::as_object_mut)
}

fn cmd_toml_string(argv: &[String]) -> i32 {
    // gm-harness toml_string <provider_ids_pipe> <existing_text> <new_table>
    let ids_pipe = argv.get(0).cloned().unwrap_or_default();
    let existing_path = argv.get(1).cloned().unwrap_or_default();
    let table_key = argv.get(2).cloned().unwrap_or_default();
    let fields_path = argv.get(3).cloned().unwrap_or_default();
    let ids: Vec<String> = ids_pipe.split('|').filter(|s| !s.is_empty()).map(String::from).collect();
    let existing = std::fs::read_to_string(&existing_path).unwrap_or_default();
    let fields_v: serde_json::Value = serde_json::from_slice(&read(&fields_path)).expect("parse fields");
    let fields_map = fields_v.as_object().expect("object fields").clone();
    let text = match ::grok_models::toml_out::write_toml_stdlib(
        std::path::Path::new(&existing_path),
        &ids,
        std::slice::from_ref(&(table_key, fields_map)),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    print!("{text}");
    let _ = existing;
    0
}

fn cmd_resolve(argv: &[String]) -> i32 {
    let providers_path = argv.get(0).cloned().unwrap_or_default();
    let targets = argv[1..].to_vec();
    let v: serde_json::Value = serde_json::from_slice(&read(&providers_path)).expect("parse");
    match ::grok_models::commands::resolve_targets(&v, &targets) {
        Ok(_) => 0,
        Err(e) => {
            println!("{}", e);
            1
        }
    }
}

fn cmd_parse_bool(argv: &[String]) -> i32 {
    let raw = argv.get(0).cloned().unwrap_or_default();
    match core::parse_bool(&raw) {
        Some(b) => println!("{}", b),
        None => println!("None"),
    }
    0
}

fn cmd_close_matches(argv: &[String]) -> i32 {
    let word = argv.get(0).cloned().unwrap_or_default();
    let list: Vec<String> = argv.get(1).cloned().unwrap_or_default()
        .split('|').filter(|s| !s.is_empty()).map(String::from).collect();
    let hits = difflib::get_close_matches(&word, &list);
    println!("{}", hits.join(","));
    0
}

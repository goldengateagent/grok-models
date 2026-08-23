use grok_models::{cli, commands, flow, sync, Res};
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            cli::print_help();
            return ExitCode::from(2);
        }
    };
    match dispatch(args) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(args: cli::Args) -> Res<i32> {
    if let Some(id) = args.add_provider {
        return commands::cmd_add_provider(&id);
    }
    if args.import_flag {
        return commands::cmd_import();
    }
    if let Some(t) = args.search {
        return commands::cmd_search(&t);
    }
    if args.config {
        return flow::cmd_config();
    }
    if args.providers {
        let doc = grok_models::jsonio::load_providers()?;
        commands::render_list_text(&doc, None, true)?;
        return Ok(0);
    }
    if let Some(p) = &args.provider {
        let doc = grok_models::jsonio::load_providers()?;
        commands::render_list_text(&doc, Some(p), false)?;
        return Ok(0);
    }
    if args.models {
        return commands::render_models_text();
    }
    if args.disable_all {
        return commands::cmd_disable_all();
    }
    if !args.enable.is_empty() || !args.disable.is_empty() {
        return commands::cmd_toggle(&args.enable, &args.disable);
    }
    commands::cmd_sync()
}

#[allow(dead_code)]
fn _sync_alias() {
    // Anchor to keep `sync` module symbol referenced from the binary entry.
    let _ = sync::MODELS_DEV_URL;
}

//! Argparse-equivalent parser: mutually exclusive group, repeatable
//! `--enable`/`--disable`, same help text and epilog.

use crate::fail;
use crate::Res;

#[derive(Debug, Default)]
pub struct Args {
    pub add_provider: Option<String>,
    pub import_flag: bool,
    pub search: Option<String>,
    pub disable_all: bool,
    pub sync: bool,
    pub disable: Vec<String>,
    pub enable: Vec<String>,
    pub models: bool,
    pub providers: bool,
    pub provider: Option<String>,
    pub codex: Option<String>,
}

const HELP: &str = "\
Manage Grok Build [model.*] tables from models.dev.

Writes [model.<provider-id>-<model-id>] into ~/.grok/config.toml (or $GROK_HOME).
Matching tables are added, updated, or deleted on sync. Give custom models
unique table names so they are not overwritten.

No arguments opens the interactive TUI (numbered menus if stdout is not a TTY).";
const EPILOG: &str = "\
quick start:
  grok-models --add-provider opencode-go
  grok-models --enable opencode-go/glm-5.3
  grok-models                              then: TUI, or just use the model

examples:
  grok-models                              interactive TUI
  grok-models --providers                  list configured providers
  grok-models --provider opencode-go       list models for a provider
  grok-models --models                     list enabled models
  grok-models --add-provider opencode-go   add a provider (models start disabled)
  grok-models --search glm                 search models.dev and add a provider
  grok-models --enable opencode-go/glm-5.3 enable a model
  grok-models --disable opencode-go/glm-5.3
  grok-models --disable-all
  grok-models --codex openrouter           write Codex config for this provider on sync (or 'disabled')
  grok-models --sync                       refresh from models.dev; rewrite config.toml
  grok-models --import                     pull [model.*] from an existing config.toml";

pub fn print_help() {
    println!("usage: grok-models [OPTION]...");
    println!();
    println!("{HELP}");
    println!();
    println!("Options:");
    println!("  --providers              List configured providers");
    println!("  --provider ID            List models for this provider");
    println!("  --models                 List enabled models");
    println!("  --add-provider ID        Add provider ID from models.dev");
    println!("  --search TERM            Search models.dev providers and add one");
    println!("  --enable TARGET          Enable provider or provider/model (repeatable)");
    println!("  --disable TARGET         Disable provider or provider/model (repeatable)");
    println!("  --disable-all            Disable every model in every provider");
    println!("  --codex PROVIDER         Write Codex config for this enabled provider on sync (or 'disabled')");
    println!("  --sync                   Refresh providers.json from models.dev; rewrite config.toml");
    println!("  --import                 Import providers/models from existing config.toml [model.*]");
    println!("  -h, --help               Show this help and exit");
    println!();
    println!("{EPILOG}");
}

pub fn parse(argv: &[String]) -> Res<Args> {
    let mut a = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let consume_value = |args: &mut Args, val: String, arg_name: &str| -> Res<()> {
            match arg_name {
                "--add-provider" => args.add_provider = Some(val),
                "--search" => args.search = Some(val),
                "--enable" => args.enable.push(val),
                "--disable" => args.disable.push(val),
                "--provider" => args.provider = Some(val),
                "--codex" => args.codex = Some(val),
                other => return fail(format!("unknown flag {other}")),
            }
            Ok(())
        };

        // --foo=bar form
        if let Some((name, val)) = arg.split_once('=') {
            match name {
                "--add-provider" | "--search" | "--enable" | "--disable" | "--provider" | "--codex" => {
                    consume_value(&mut a, val.to_string(), name)?;
                    i += 1;
                    continue;
                }
                "--import" => {
                    a.import_flag = true;
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }

        match arg.as_str() {
            "--add-provider" | "--search" | "--enable" | "--disable" | "--provider" | "--codex" => {
                let i_next = i + 1;
                if i_next >= argv.len() {
                    return fail(format!("{arg} requires a value"));
                }
                consume_value(&mut a, argv[i_next].clone(), arg)?;
                i += 2;
            }
            "--import" => {
                a.import_flag = true;
                i += 1;
            }
            "--disable-all" => {
                a.disable_all = true;
                i += 1;
            }
            "--sync" => {
                a.sync = true;
                i += 1;
            }
            "--models" => {
                a.models = true;
                i += 1;
            }
            "--providers" => {
                a.providers = true;
                i += 1;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("grok-models 1.0.0");
                std::process::exit(0);
            }
            other => return fail(format!("unknown flag {other}")),
        }
    }

    // argparse-equivalent mutually exclusive group: at most one of these.
    let mut group_flags: Vec<&str> = Vec::new();
    if a.add_provider.is_some() {
        group_flags.push("--add-provider");
    }
    if a.import_flag {
        group_flags.push("--import");
    }
    if a.search.is_some() {
        group_flags.push("--search");
    }
    if a.disable_all {
        group_flags.push("--disable-all");
    }
    if a.sync {
        group_flags.push("--sync");
    }
    if a.providers {
        group_flags.push("--providers");
    }
    if a.provider.is_some() {
        group_flags.push("--provider");
    }
    if a.models {
        group_flags.push("--models");
    }
    if a.codex.is_some() {
        group_flags.push("--codex");
    }
    if group_flags.len() > 1 {
        return fail(format!(
            "the following arguments are mutually exclusive: {}",
            group_flags.join(", ")
        ));
    }

    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn a(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_add_provider() {
        let argv = a(&["--add-provider", "opencode"]);
        let p = parse(&argv).unwrap();
        assert_eq!(p.add_provider.as_deref(), Some("opencode"));
    }

    #[test]
    fn repeated_enable_disable() {
        let argv = a(&[
            "--enable", "opencode",
            "--disable", "openrouter/x",
            "--enable=foo/bar",
        ]);
        let p = parse(&argv).unwrap();
        assert_eq!(p.enable, vec!["opencode", "foo/bar"]);
        assert_eq!(p.disable, vec!["openrouter/x"]);
    }

    #[test]
    fn parses_codex() {
        let p = parse(&a(&["--codex", "openrouter"])).unwrap();
        assert_eq!(p.codex.as_deref(), Some("openrouter"));
        let p = parse(&a(&["--codex=disabled"])).unwrap();
        assert_eq!(p.codex.as_deref(), Some("disabled"));
    }

    #[test]
    fn rejects_unknown() {
        let argv = a(&["--bogus"]);
        assert!(parse(&argv).is_err());
    }
}

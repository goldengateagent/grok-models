//! Argparse-equivalent parser: mutually exclusive group, repeatable
//! `--enable`/`--disable`, same help text and epilog.

use crate::fail;
use crate::Res;

#[derive(Debug, Default)]
pub struct Args {
    pub add_provider: Option<String>,
    pub import_flag: bool,
    pub search: Option<String>,
    pub config: bool,
    pub disable_all: bool,
    pub sync: bool,
    pub disable: Vec<String>,
    pub enable: Vec<String>,
    pub models: bool,
    pub providers: bool,
    pub provider: Option<String>,
}

const HELP: &str = "Grok Build config.toml [model.<provider-id>-<model-id>] tables will be added, \
updated or deleted by this command for any matched pattern of <provider-id>-<model-id>. \
Uniquely name your manually configured custom models to avoid modification.";
const EPILOG: &str = "\
examples:
  grok-models                              sync to config.toml
  grok-models --add-provider opencode      add OpenCode Zen provider
  grok-models --search ollama              search and add a provider
  grok-models --providers                  show configured providers
  grok-models --config                     interactively configure providers/models
  grok-models --models                     show currently enabled models
  grok-models --provider opencode          show models for a provider
  grok-models --enable opencode/hy3-free   enable model
  grok-models --disable openrouter         disable a provider
  grok-models --disable-all                disable all models
  grok-models --import                     import providers/models from config.toml";

pub fn print_help() {
    println!("usage: grok-models [OPTION]...");
    println!();
    println!("{HELP}");
    println!();
    println!("Options:");
    println!("  --add-provider ID        Add provider ID");
    println!("  --search TERM            Search providers");
    println!("  --config                 Configure a provider or its models");
    println!("  --models                 Show enabled models");
    println!("  --providers              Show configured providers");
    println!("  --provider ID            Show the models for this provider");
    println!("  --disable-all            Disable all models in every provider");
    println!("  --disable TARGET         Disable TARGET (provider or provider/model); repeatable");
    println!("  --enable TARGET          Enable TARGET (provider or provider/model); repeatable");
    println!("  --import                 Import providers/models from existing config.toml [model.*] tables");
    println!("  --sync                   Sync providers.json with models.dev and rewrite config.toml");
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
                other => return fail(format!("unknown flag {other}")),
            }
            Ok(())
        };

        // --foo=bar form
        if let Some((name, val)) = arg.split_once('=') {
            match name {
                "--add-provider" | "--search" | "--enable" | "--disable" | "--provider" => {
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
            "--add-provider" | "--search" | "--enable" | "--disable" | "--provider" => {
                let i_next = i + 1;
                if i_next >= argv.len() {
                    return fail(format!("{arg} requires a value"));
                }
                consume_value(&mut a, argv[i_next].clone(), arg)?;
                i += 2;
            }
            "--config" => {
                a.config = true;
                i += 1;
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
    if a.config {
        group_flags.push("--config");
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
    fn rejects_unknown() {
        let argv = a(&["--bogus"]);
        assert!(parse(&argv).is_err());
    }
}

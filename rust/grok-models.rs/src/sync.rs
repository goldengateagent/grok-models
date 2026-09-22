//! Sync orchestration surface: shared types, report strings, and re-exports.
//!
//! Fetching lives in [`crate::fetch`], providers.json updates in
//! [`crate::providers`], and generated config files in [`crate::config_toml`].

#[cfg(test)]
use crate::Error;
use crate::Res;
pub use crate::codex_out::*;
pub use crate::config_toml::*;
use crate::core;
pub use crate::fetch::*;
pub use crate::grok_out::*;
pub use crate::providers::*;
use serde_json::Value;

/// Something the sync noticed but did not fail on.
pub enum SyncWarning {
    /// The provider is configured but absent from models.dev.
    NotInModelsDev { provider_id: String },
    /// The live /models call failed; the catalog was used instead.
    LiveFetchFailed { message: String },
}

/// `run_sync()` — reconcile providers.json with a live API payload, then
/// rewrite config.toml from it.
pub fn run_sync() -> Res<(UpdateConfigResponse, UpdateProvidersResponse)> {
    // Phase 1: update the models in providers.json.
    let response = update_providers_json()?;

    // Phase 2: rewrite config.toml from providers.json.
    let written = update_config_toml()?;
    Ok((written, response))
}

/// The warning lines a sync produced, in order, ready to print. Used by the
/// printers and by the failure paths that attach them to an error.
pub fn sync_warning_lines(
    response: &UpdateProvidersResponse,
    written: &UpdateConfigResponse,
) -> Vec<String> {
    let mut lines = response_warning_lines(&response.warnings);
    lines.extend(config_warning_lines(&written.providers_without_base_url));
    lines
}

/// Phase 1's warning lines.
pub fn response_warning_lines(warnings: &[SyncWarning]) -> Vec<String> {
    warnings
        .iter()
        .map(|warning| match warning {
            SyncWarning::NotInModelsDev { provider_id } => format!(
                "  warning: provider '{}' not found in models.dev; skipping",
                provider_id
            ),
            // Already rendered by live_fetch_error_status.
            SyncWarning::LiveFetchFailed { message } => message.clone(),
        })
        .collect()
}

/// Phase 2's warning lines.
pub fn config_warning_lines(providers_without_base_url: &[String]) -> Vec<String> {
    providers_without_base_url
        .iter()
        .map(|pid| {
            format!(
                "  warning: provider '{}' has no base URL; \
tables will have an empty base_url",
                pid
            )
        })
        .collect()
}

/// Prints the updated path.
pub fn print_summary(path: &std::path::Path) {
    println!();
    println!("Updated: {}", path.display());
}

/// Prints the relaunch notice.
pub fn print_relaunch() {
    println!("Relaunch Grok Build for model changes");
    println!();
}

/// Prints required env vars with masked values.
pub fn print_env_requirements(providers_doc: &Value) {
    let env_vars = core::enabled_provider_env_vars(providers_doc);
    if env_vars.is_empty() {
        return;
    }
    println!();
    println!("Required environment variables:");
    for env_var in &env_vars {
        println!("  {}", crate::env::vars::env_key_masked_display(env_var));
    }
    println!();
}

/// Prints the summary plus env requirements.
pub fn print_sync_report(path: &std::path::Path, providers_doc: &Value) {
    print_summary(path);
    print_env_requirements(providers_doc);
}

/// Warnings gathered by `run_sync`'s two phases, in the order the phases
/// produced them. Printed by the CLI; the TUI renders its own status lines.
pub fn print_sync_warnings(response: &UpdateProvidersResponse, written: &UpdateConfigResponse) {
    for line in sync_warning_lines(response, written) {
        println!("{line}");
    }
}

/// The config.toml write phase's warnings.
pub fn print_config_warnings(written: &UpdateConfigResponse) {
    for line in config_warning_lines(&written.providers_without_base_url) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_lines_render_python_wording() {
        let lines = response_warning_lines(&[SyncWarning::NotInModelsDev {
            provider_id: "foo".into(),
        }]);
        assert_eq!(
            lines,
            vec!["  warning: provider 'foo' not found in models.dev; skipping"]
        );

        let lines = response_warning_lines(&[SyncWarning::LiveFetchFailed {
            message: "already rendered".into(),
        }]);
        assert_eq!(lines, vec!["already rendered"]);

        let lines = config_warning_lines(&["bar".to_string()]);
        assert_eq!(
            lines,
            vec![
                "  warning: provider 'bar' has no base URL; \
tables will have an empty base_url"
            ]
        );
    }

    #[test]
    fn error_carries_warnings_produced_before_the_failure() {
        let e = Error::new("provider 'x' has no models in models.dev")
            .with_warnings(vec!["  warning: provider 'x' not found".into()]);
        assert_eq!(e.message, "provider 'x' has no models in models.dev");
        assert_eq!(e.warnings.len(), 1);
        assert_eq!(format!("{e}"), "provider 'x' has no models in models.dev");
    }
}

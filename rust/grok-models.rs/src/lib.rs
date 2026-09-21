//! Grok models library.
//!
//! Module map:
//! - `jsonio`:    ordered JSON load/dump + atomic writes
//! - `json_utils`: generic `serde_json` value accessors (no provider meaning)
//! - `env`:       process environment — `env::paths` GROK_HOME / CODEX_HOME
//!                locations, `env::vars` WSL-aware env reads and masked
//!                key previews, `env::term` terminal probes,
//!                `env::test_support` GROK_HOME test lock
//! - `core`:      model id/table-key helpers, sorting, TOML field building
//! - `benchmarks`: model benchmark score tables
//! - `toml_out`:  `[model.*]` table emission and owned-section stripping
//! - `sync`:      provider entry writes, models.dev reconciliation,
//!                config.toml writing
//! - `cli`:       CLI surface — `cli::args` parser and help text,
//!                `cli::commands` command implementations
//! - `client`:    blocking JSON HTTP client
//! - `fallback`:  numbered (non-TTY) interactive flows
//! - `theme`:     Tokyo Nights palette, truecolor SGR, opacity compensation
//! - `tui`:       Ratatui/Crossterm screens
//! - `flow`:      interactive TUI orchestration (TUI + numbered fallback)

pub mod benchmarks;
pub mod cli;
pub mod client;
pub mod core;

pub mod env;
pub mod fallback;
pub mod flow;
pub mod json_utils;
pub mod jsonio;
pub mod sync;
pub mod theme;
pub mod toml_out;
pub mod tui;

/// Fatal error mapped to exit code 1.
///
/// `warnings` holds diagnostics the operation produced before it failed, so a
/// caller that only receives the error still has them to print. Rendered lines
/// rather than structured values: this type sits at the crate root and must not
/// depend on `sync`'s warning enum.
#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub warnings: Vec<String>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            warnings: Vec::new(),
        }
    }

    /// Attaches diagnostics produced before the failure.
    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

pub type Res<T> = Result<T, Error>;

pub fn fail<T>(message: impl Into<String>) -> Res<T> {
    Err(Error::new(message))
}

/// Provider display label; provider-specific, kept here until the provider
/// reorganization moves it next to the other provider helpers.
pub fn provider_label_from(o: &serde_json::Map<String, serde_json::Value>) -> String {
    core::provider_label(o)
}

//! Process environment and home-directory locations.
//!
//! Layout:
//! - `paths`: `GROK_HOME` / `CODEX_HOME` file locations
//! - `vars`: process env reads (WSL-aware) and masked key previews
//! - `term`: terminal capability probes (`COLORTERM`)
//! - `test_support`: serializes tests that mutate `GROK_HOME` / `CODEX_HOME`

pub mod paths;
pub mod term;
#[cfg(test)]
pub mod test_support;
pub mod vars;

//! Terminal capability probes.
//!
//! Mirrors grok-build's tokyonight.rs: 24-bit truecolor when
//! `COLORTERM` is `truecolor` or `24bit`, named-color fallback otherwise.

/// Truecolor when `COLORTERM` is `truecolor` or `24bit`.
pub fn use_truecolor() -> bool {
    matches!(
        std::env::var("COLORTERM").as_deref(),
        Ok("truecolor") | Ok("24bit")
    )
}

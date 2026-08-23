//! Tokyo Nights theme + transparency compensation, ported from
//! `_TN`/`_curses_init_colors`/`_compensated_bg`/`_emit_sgr_bg`.
//!
//! Strategy mirrors grok-build's tokyonight.rs: 24-bit truecolor when
//! `COLORTERM ∈ {truecolor, 24bit}`, named-color fallback otherwise.

use once_cell::sync::OnceCell;

pub const TN: [(u8, u8, u8); 8] = [
    (36, 40, 59),    // bg (#24283b Storm)
    (31, 35, 53),    // bg_dark (#1f2335)
    (41, 46, 66),    // bg_highlight (#292e42)
    (40, 52, 87),    // bg_visual (#283457 selection)
    (192, 202, 245), // fg primary (#c0caf5)
    (169, 177, 214), // fg_dark (#a9b1d6)
    (86, 95, 137),   // comment muted (#565f89)
    (115, 122, 162), // dark5 (#737aa2)
];
// Additional accent colors consumed directly by the SGR emitters below.
pub const ACCENT: [(u8, u8, u8); 3] = [
    (122, 162, 247), // blue (#7aa2f7)
    (125, 207, 255), // cyan (#7dcfff)
    (158, 206, 106), // green (#9ece6a)
];

/// Tokyo Nights red, used for the `[disabled]` state token (Python `P.ERROR`).
pub const RED: (u8, u8, u8) = (247, 118, 142); // #f7768e

/// Curses pair ids preserved from the Python `P` enum (1..=10, plus `Error`).
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, PartialEq)]
pub enum P {
    Text = 1,
    Muted = 2,
    Value = 3,
    Free = 4,
    Enabled = 5,
    Disabled = 6,
    Selected = 7,
    Chevron = 8,
    LegendKey = 9,
    LegendDesc = 10,
    /// Red missing/error text on theme bg (Python `P.ERROR=11`): used for an
    /// unset env var name in the `--config` models preview box.
    Error = 11,
}

#[derive(Clone, Copy, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub fn tn(palette: usize) -> Rgb {
    let c = TN[palette.min(TN.len() - 1)];
    Rgb { r: c.0, g: c.1, b: c.2 }
}

pub fn accent(i: usize) -> Rgb {
    let c = ACCENT[i.min(ACCENT.len() - 1)];
    Rgb { r: c.0, g: c.1, b: c.2 }
}

/// Truecolor + config.
pub fn use_truecolor() -> bool {
    matches!(
        std::env::var("COLORTERM").as_deref(),
        Ok("truecolor") | Ok("24bit")
    )
}

/// Compute the SGR bg to emit (compensated for translucent macOS Terminal
/// profiles) or the uncompensated bg otherwise.
pub fn bg_to_emit() -> Rgb {
    let target = tn(0); // bg
    if !use_truecolor() {
        return target;
    }
    *COMPENSATED.get_or_init(|| compute_compensated(target, target))
}

/// Cached compensated background (lazily fetched once per process).
static COMPENSATED: OnceCell<Rgb> = OnceCell::new();

fn compute_compensated(target: Rgb, fallback: Rgb) -> Rgb {
    if !(cfg!(target_os = "macos")) {
        return fallback;
    }
    let osascript = match std::process::Command::new("which")
        .arg("osascript")
        .output()
    {
        Ok(o) => o,
        Err(_) => return fallback,
    };
    if !String::from_utf8_lossy(&osascript.stdout).contains("osascript") {
        return fallback;
    }

    fn run(script: &str) -> Option<String> {
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    let profile = match run(r#"tell application "Terminal" to get name of current settings of front window"#) {
        Some(p) => p,
        None => return fallback,
    };
    let bg_script = format!(
        r#"tell application "Terminal" to get background color of settings set "{}""#,
        profile
    );
    let alpha_script = format!(
        r#"tell application "Terminal" to get transparency of settings set "{}""#,
        profile
    );
    let bg_raw = match run(&bg_script) {
        Some(b) => b,
        None => return fallback,
    };
    let alpha_raw = match run(&alpha_script) {
        Some(a) => a,
        None => return fallback,
    };
    let parts: Vec<u16> = bg_raw
        .split(", ")
        .filter_map(|s| s.trim().parse::<f64>().ok().map(|f| f as u16))
        .collect();
    if parts.len() < 3 {
        return fallback;
    }
    let pb = Rgb {
        r: (parts[0] / 257) as u8,
        g: (parts[1] / 257) as u8,
        b: (parts[2] / 257) as u8,
    };
    let alpha = 1.0_f64 - alpha_raw.parse::<f64>().unwrap_or(0.0);
    if !(0.0..1.0).contains(&alpha) {
        return fallback;
    }
    Rgb {
        r: comp(target.r, pb.r, alpha),
        g: comp(target.g, pb.g, alpha),
        b: comp(target.b, pb.b, alpha),
    }
}

fn comp(target: u8, bg: u8, alpha: f64) -> u8 {
    let t = target as f64;
    let b = bg as f64;
    let c = (t - (1.0 - alpha) * b) / alpha;
    c.round().clamp(0.0, 255.0) as u8
}

/// Build an SGR string for fg+bg truecolor paint.
pub fn sgr_paint(fg: Rgb, bg: Rgb, bold: bool) -> String {
    let bold_part = if bold { "1;" } else { "" };
    format!(
        "\x1b[{bold_part}38;2;{r};{g};{b};48;2;{br};{bg};{bb}m",
        r = fg.r,
        g = fg.g,
        b = fg.b,
        br = bg.r,
        bg = bg.g,
        bb = bg.b
    )
}

/// 24-bit background SGR (`\x1b[48;2;R;G;Bm`).
pub fn sgr_bg(r: Rgb) -> String {
    format!("\x1b[48;2;{};{};{}m", r.r, r.g, r.b)
}

/// SGR reset that preserves the theme bg (so an emitted 24-bit bg sticks
/// after non-themed text).
pub fn sgr_reset_with_bg(bg: Rgb) -> String {
    format!("\x1b[0;{}48;2;{};{};{}m", 0, bg.r, bg.g, bg.b)
}

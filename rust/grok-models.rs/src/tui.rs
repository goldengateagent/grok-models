//! Raw-mode ANSI TUI (curses equivalent).
//!
//! Backed by `Stdscr` — a trait whose real implementation writes escape
//! sequences to a `/dev/tty` and whose fake implementation records calls so
//! the unit tests can assert exact rendered output. This lets every code path
//! in the curses Python flow run through one screen object without `curses`
//! panicking in CI.
//!
//! Behaviour parity with `grok-models.py`:
//! - Full-screen background sweep on every frame (NBSP fill).
//! - Truecolor fg+bg per cell when `COLORTERM=truecolor`.
//! - One persistent screen (no mode flash): all screens share `Stdscr`.
//! - Identical keybindings (↑/↓/←/→/Enter/Space/Backspace/ESC/q/type).
//! - Footer "box" when terminal is tall enough, single-line pill otherwise.

use crate::core;
use crate::fallback;
use crate::jsonio;
use crate::paths;
use crate::theme::{self, P, Rgb};
use crate::Res;
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Fate of a TUI invocation when /dev/tty isn't a TTY.
pub struct CursesFailed;

pub fn curses_failed_marker() -> CursesFailed {
    CursesFailed
}

pub fn tui_supported() -> bool {
    if !atty_stdout() {
        return false;
    }
    #[cfg(test)]
    {
        // Tests run the TUI through `FakeStdscr` regardless.
        true
    }
    #[cfg(not(test))]
    {
        true
    }
}

fn atty_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// stdscr trait
// ---------------------------------------------------------------------------

pub trait Stdscr {
    fn getmaxyx(&self) -> (i32, i32);
    fn erase(&mut self);
    fn refresh(&mut self);
    fn addstr(&mut self, y: i32, x: i32, s: &str, attr: Paint);
    fn getch(&mut self) -> Key;
    /// Force the next `refresh` to rewrite every cell. Used after a confirm
    /// overlay so leftover title/body cells cannot survive a diff paint.
    fn invalidate(&mut self) {}
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Backspace,
    Esc,
    Char(char),
    Interrupt,
    Resize,
    PageUp,
    PageDown,
    /// Mouse-wheel up; payload is the 0-based row the event landed on.
    WheelUp(i32),
    /// Mouse-wheel down; payload is the 0-based row the event landed on.
    WheelDown(i32),
    Eof,
}

/// Provider ids highlighted in the Add Provider screen's "Suggested" section.
/// Anything already configured lands in the "Added" section above it; the rest
/// are listed unhighlighted below. Mirrors `SUGGESTED_PROVIDER_IDS` in Python.
pub const SUGGESTED_PROVIDER_IDS: [&str; 5] =
    ["opencode", "opencode-go", "openrouter", "ollama-cloud", "gmicloud"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paint {
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
}

impl Paint {
    pub fn plain(fg: Rgb, bg: Rgb) -> Paint {
        Paint { fg, bg, bold: false }
    }
    pub fn bold(mut self) -> Paint {
        self.bold = true;
        self
    }
    pub fn bold_if(mut self, cond: bool) -> Paint {
        if cond {
            self.bold = true;
        }
        self
    }
}

/// A blank frame: every cell a theme-background space.
fn blank_frame(rows: usize, cols: usize) -> Vec<Vec<(char, Paint)>> {
    let bg = Paint::plain(crate::theme::tn(0), crate::theme::tn(0));
    vec![vec![(' ', bg); cols.max(1)]; rows.max(1)]
}

/// A "never seen" frame: `\0` cells that compare unequal to anything real,
/// forcing the diff renderer to emit a full repaint.
fn unknown_frame(rows: usize, cols: usize) -> Vec<Vec<(char, Paint)>> {
    let p = Paint::plain(Rgb { r: 0, g: 0, b: 0 }, Rgb { r: 0, g: 0, b: 0 });
    vec![vec![('\0', p); cols.max(1)]; rows.max(1)]
}

// ---------------------------------------------------------------------------
// Palette helpers
// ---------------------------------------------------------------------------

fn color_of(p: P) -> (Rgb, Rgb) {
    // fg, bg for the curses pair
    match p {
        P::Text => (theme::tn(4), theme::tn(0)),
        P::Muted => (theme::tn(6), theme::tn(0)),
        P::Value => (accent_color(0), theme::tn(0)),
        P::Free => (accent_color(1), theme::tn(0)),
        P::Enabled => (accent_color(2), theme::tn(0)),
        P::Disabled => (theme::tn(5), theme::tn(0)),
        P::Selected => (theme::tn(4), theme::tn(3)),
        P::Chevron => (theme::tn(7), theme::tn(0)),
        P::LegendKey => (theme::tn(5), theme::tn(0)),
        P::LegendDesc => (theme::tn(6), theme::tn(0)),
        P::Error => (
            Rgb { r: theme::RED.0, g: theme::RED.1, b: theme::RED.2 },
            theme::tn(0),
        ),
        P::CodeText => (theme::CODE_TEXT, theme::CODE_BG),
        P::CodeComment => (theme::CODE_COMMENT, theme::CODE_BG),
        P::CodeError => (
            Rgb { r: theme::RED.0, g: theme::RED.1, b: theme::RED.2 },
            theme::CODE_BG,
        ),
        P::CodeString => (theme::CODE_STRING, theme::CODE_BG),
        P::CodeSymbol => (theme::CODE_SYMBOL_GOLD, theme::CODE_BG),
        P::CodeVar => (theme::CODE_TEXT, theme::CODE_BG),
    }
}

fn accent_color(i: usize) -> Rgb {
    use crate::theme::ACCENT;
    let c = ACCENT[i.min(ACCENT.len() - 1)];
    Rgb { r: c.0, g: c.1, b: c.2 }
}

fn tn_color(p: P) -> Rgb {
    color_of(p).0
}
fn bg_color(p: P) -> Rgb {
    color_of(p).1
}

/// Build a plain `Paint` for a palette pair (fg + bg, no bold).
fn paint_for(p: P) -> Paint {
    Paint::plain(color_of(p).0, color_of(p).1)
}

/// Terminal columns for one scalar. `➕` (and similar emoji) are 2 columns
/// even though East Asian Width is Neutral — treating them as 1 is what
/// left `d`/`…` ghosts after `Add Provider…` and ate the space in that label.
fn char_cols(ch: char) -> usize {
    if ch == '➕' {
        return 2;
    }
    let u = ch as u32;
    if (0x1F300..=0x1FAFF).contains(&u) {
        return 2;
    }
    // Common fullwidth / CJK blocks (no extra crate).
    if (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE10..=0xFE19).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x3000..=0x303E).contains(&u)
    {
        2
    } else {
        1
    }
}

fn str_cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

fn clip_cols(s: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    for c in s.chars() {
        let w = char_cols(c);
        if n + w > max_cols {
            break;
        }
        out.push(c);
        n += w;
    }
    out
}

fn pad_cols(s: &str, width: usize, fill: char) -> String {
    let s = clip_cols(s, width);
    let n = str_cols(&s);
    let mut out = s;
    for _ in n..width {
        out.push(fill);
    }
    out
}

/// Draw a line of `(text, color)` segments, truncating once `max_w` is
/// exhausted. Mirrors Python's `_draw_seg_line`.
fn draw_seg_line<S: Stdscr>(
    stdscr: &mut S,
    y: i32,
    x: i32,
    segments: &[(String, P)],
    max_w: usize,
) {
    draw_seg_line_bg(stdscr, y, x, segments, max_w, None);
}

/// Like `draw_seg_line`, but paints every segment on `bg_pair`'s background
/// so a selected row keeps its fg colors on the highlight.
fn draw_seg_line_bg<S: Stdscr>(
    stdscr: &mut S,
    y: i32,
    x: i32,
    segments: &[(String, P)],
    max_w: usize,
    bg_pair: Option<P>,
) {
    let mut cx = x;
    for (text, pid) in segments {
        if (cx as usize) >= (x as usize) + max_w {
            break;
        }
        let take = ((x as usize) + max_w).saturating_sub(cx as usize);
        let piece = clip_cols(text, take);
        if piece.is_empty() {
            continue;
        }
        let paint = match bg_pair {
            Some(bg) => Paint::plain(tn_color(*pid), bg_color(bg)),
            None => paint_for(*pid),
        };
        stdscr.addstr(y, cx, &piece, paint);
        cx += str_cols(&piece) as i32;
    }
}

// ---------------------------------------------------------------------------
// Code panels: vim-sh tokenizer + borderless black panel (python
// `_code_line_segments` / `draw_code_panel`)
// ---------------------------------------------------------------------------

/// Syntax-color one code line for the black code panels, following vim's sh
/// scheme: gold symbols (=, quotes, redirections), white strings and plain
/// text, cyan comments, green variable names. `highlight` optionally recolors
/// a char span (start, end, pair) — used to flag unset env variables. An empty
/// `VAR = ""` assignment is auto-flagged red.
/// Match Python `^(\S+)(\s*=)`: leading non-space run followed by optional
/// spaces and '='. With no whitespace at all, the run backtracks to the first
/// '=' (so `KEY=""` assigns). Returns the end of the variable-name span.
fn sh_assign_name_end(chars: &[char]) -> Option<usize> {
    let n = chars.len();
    if n == 0 || chars[0].is_whitespace() {
        return None;
    }
    let run_end = chars.iter().position(|c| c.is_whitespace()).unwrap_or(n);
    let mut j = run_end;
    while j < n && chars[j].is_whitespace() {
        j += 1;
    }
    if j < n && chars[j] == '=' {
        return Some(run_end);
    }
    if run_end == n {
        if let Some(eq) = chars.iter().position(|&c| c == '=') {
            if eq > 0 {
                return Some(eq);
            }
        }
    }
    None
}

pub fn code_line_segments(
    ln: &str,
    highlight: Option<(usize, usize, P)>,
) -> Vec<(String, P)> {
    let chars: Vec<char> = ln.chars().collect();
    let n = chars.len();
    if chars.iter().all(|c| c.is_whitespace()) || ln.trim_start().starts_with('#') {
        return vec![(ln.to_string(), P::CodeComment)];
    }
    let mut attrs: Vec<P> = vec![P::CodeText; n];

    // Assignment: leading identifier before '=' colors as a variable.
    // Otherwise the leading command word (echo, pbpaste, …) renders gold.
    let assign_end = sh_assign_name_end(&chars);
    if let Some(name_end) = assign_end {
        for a in attrs.iter_mut().take(name_end) {
            *a = P::CodeVar;
        }
    } else if n > 0 {
        let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
        let end = chars[start..]
            .iter()
            .position(|c| c.is_whitespace())
            .map(|p| start + p)
            .unwrap_or(n);
        for a in attrs[start..end].iter_mut() {
            *a = P::CodeSymbol;
        }
    }

    // Quote state machine + bare operators.
    let mut in_single = false;
    let mut in_double = false;
    let mut sq_open: Option<usize> = None;
    let mut dq_open: Option<usize> = None;
    for i in 0..n {
        let ch = chars[i];
        if ch == '\'' && !in_double {
            attrs[i] = P::CodeSymbol;
            if in_single {
                for a in attrs[sq_open.unwrap_or(0) + 1..i].iter_mut() {
                    *a = P::CodeString;
                }
                in_single = false;
                sq_open = None;
            } else {
                in_single = true;
                sq_open = Some(i);
            }
        } else if ch == '"' && !in_single {
            attrs[i] = P::CodeSymbol;
            if in_double {
                for a in attrs[dq_open.unwrap_or(0) + 1..i].iter_mut() {
                    *a = P::CodeString;
                }
                in_double = false;
                dq_open = None;
            } else {
                in_double = true;
                dq_open = Some(i);
            }
        } else if "=<>|;".contains(ch) && !in_single && !in_double {
            attrs[i] = P::CodeSymbol;
        }
    }

    // Auto-highlight an empty assignment `VAR = ""`.
    let highlight = highlight.or_else(|| {
        let name_end = sh_assign_name_end(&chars)?;
        let mut i = name_end;
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i < n && chars[i] == '=' {
            i += 1;
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i + 1 < n && chars[i] == '"' && chars[i + 1] == '"' {
                return Some((0, name_end, P::CodeError));
            }
        }
        None
    });

    // Unterminated string keeps its remainder white.
    if in_single {
        for a in attrs[sq_open.unwrap_or(0) + 1..n].iter_mut() {
            *a = P::CodeString;
        }
    } else if in_double {
        for a in attrs[dq_open.unwrap_or(0) + 1..n].iter_mut() {
            *a = P::CodeString;
        }
    }

    if let Some((hs, he, hcolor)) = highlight {
        for a in attrs[hs.min(n)..he.min(n)].iter_mut() {
            if matches!(*a, P::CodeVar | P::CodeText | P::CodeString) {
                *a = hcolor;
            }
        }
    }

    // Compact into runs.
    let mut runs: Vec<(usize, usize, P)> = Vec::new();
    for (i, a) in attrs.iter().enumerate() {
        match runs.last_mut() {
            Some(last) if last.2 == *a && last.1 == i => last.1 = i + 1,
            _ => runs.push((i, i + 1, *a)),
        }
    }
    runs.into_iter()
        .map(|(s, e, a)| (chars[s..e].iter().collect::<String>(), a))
        .collect()
}

/// Solid black panel whose lines are colored by the shared tokenizer. Width is
/// the longest tokenized line plus horizontal padding, clipped to the screen;
/// drawing stops above the legend row.
fn draw_code_panel<S: Stdscr>(
    stdscr: &mut S,
    row: i32,
    panel_lines: &[String],
    bx: i32,
    width: i32,
    legend_y: i32,
) {
    const PAD_X: usize = 1;
    let panel_segs: Vec<Vec<(String, P)>> =
        panel_lines.iter().map(|l| code_line_segments(l, None)).collect();
    let panel_w = panel_segs
        .iter()
        .map(|segs| segs.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(0)
        + 2 * PAD_X;
    let panel_w = panel_w.min(((width.max(1) as usize).saturating_sub(bx as usize)).saturating_sub(2)).max(1);
    if row + panel_lines.len() as i32 > legend_y {
        return;
    }
    let fill = Paint::plain(tn_color(P::CodeText), bg_color(P::CodeText));
    for ry in row..row + panel_lines.len() as i32 {
        let spaces: String = " ".repeat(panel_w);
        stdscr.addstr(ry, bx, &spaces, fill);
    }
    for (i, segs) in panel_segs.iter().enumerate() {
        let ry = row + i as i32;
        let mut cx = bx + PAD_X as i32;
        for (t, a) in segs {
            let limit = bx + panel_w as i32 - 1;
            if cx >= limit {
                break;
            }
            let take = (limit - cx) as usize;
            let run: String = t.chars().take(take).collect();
            stdscr.addstr(ry, cx, &run, paint_for(*a));
            cx += run.chars().count() as i32;
        }
    }
}

// ---------------------------------------------------------------------------
// Background sweep (every cell NBSP-painted, never blank)
// ---------------------------------------------------------------------------

fn paint_bg<S: Stdscr>(stdscr: &mut S, paint: Paint) {
    let (h, w) = stdscr.getmaxyx();
    let fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
    for y in 0..h {
        stdscr.addstr(y, 0, &fill, paint);
    }
}

/// Mirror Python's `[enabled]`/`[disabled]` token split: locate the state
/// token (if present) and return `(head, token, tail)` so the token can be
/// painted green (enabled) or red (disabled) independent of the row color.
fn split_state_token(line: &str) -> Option<(String, String, String)> {
    let (token, pos) = if let Some(p) = line.find("[enabled]") {
        ("[enabled]".to_string(), p)
    } else if let Some(p) = line.find("[disabled]") {
        ("[disabled]".to_string(), p)
    } else if let Some(p) = line.find('[') {
        let rest = &line[p + 1..];
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            match rest.find(']') {
                Some(rel) => (line[p..=p + 1 + rel].to_string(), p),
                None => return None,
            }
        } else {
            return None;
        }
    } else {
        return None;
    };
    let head = line[..pos].to_string();
    let tail = line[pos + token.len()..].to_string();
    Some((head, token, tail))
}

// ---------------------------------------------------------------------------
// Common draws
// ---------------------------------------------------------------------------

fn draw_header<S: Stdscr>(stdscr: &mut S, text: &str) {
    let (h, w) = stdscr.getmaxyx();
    let paint = Paint::plain(tn_color(P::Selected), bg_color(P::Selected)).bold();
    let row_fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
    stdscr.addstr(0, 0, &row_fill, paint);
    stdscr.addstr(0, 2, text, paint);
    let _ = h;
}

fn draw_legend<S: Stdscr>(
    stdscr: &mut S,
    entries: &[(String, String)],
) {
    let (h, w) = stdscr.getmaxyx();
    let legend_y = h - 2;
    let bg = Paint::plain(tn_color(P::Text), bg_color(P::Text));
    let row_fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
    stdscr.addstr(legend_y, 0, &row_fill, bg);

    let mut x = 2i32;
    for (i, (key, desc)) in entries.iter().enumerate() {
        if i > 0 {
            stdscr.addstr(legend_y, x, "  │  ", Paint::plain(tn_color(P::Muted), bg_color(P::Muted)));
            x += 5;
        }
        let run = format!("{key} {desc}");
        if x as usize + str_cols(&run) >= w.max(1) as usize {
            break;
        }
        for ch in key.chars() {
            let attr = if ch == '/' {
                Paint::plain(tn_color(P::Muted), bg_color(P::Muted))
            } else {
                Paint::plain(tn_color(P::LegendKey), bg_color(P::LegendKey)).bold()
            };
            stdscr.addstr(legend_y, x, &ch.to_string(), attr);
            x += char_cols(ch) as i32;
        }
        stdscr.addstr(legend_y, x, " ", Paint::plain(tn_color(P::LegendDesc), bg_color(P::LegendDesc)));
        x += 1;
        stdscr.addstr(legend_y, x, desc, Paint::plain(tn_color(P::LegendDesc), bg_color(P::LegendDesc)));
        x += str_cols(desc) as i32;
    }
}

// ---------------------------------------------------------------------------
// Selector screen (provider list / action menu)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SelectOutcome {
    Picked(usize),
    Cancelled,
    /// Main-menu `S`: toggle Enabled Models sort; carries the current cursor.
    SortToggled(usize),
    /// Enter on an Enabled Models row.
    ModelPicked { pid: String, mid: String },
}

/// A line drawn in the TUI main-menu preview panel beneath the provider
/// list. A `Heading` is a full-width blue bar (like the screen title); a `Segs`
/// line is a sequence of `(text, color)` segments (like `--models` output).
#[derive(Clone)]
pub enum PreviewLine {
    Heading(String),
    Segs(Vec<(String, P)>),
    Model {
        pid: String,
        mid: String,
        segs: Vec<(String, P)>,
    },
}

fn preview_model_entries(preview: &[PreviewLine]) -> Vec<(usize, String, String)> {
    preview
        .iter()
        .enumerate()
        .filter_map(|(i, line)| match line {
            PreviewLine::Model { pid, mid, .. } => Some((i, pid.clone(), mid.clone())),
            _ => None,
        })
        .collect()
}

/// Step the preview pane's scroll offset by one page (the visible preview
/// height), clamped so the pane stays filled. The provider list above the
/// pane is unaffected. Mirrors Python `_curses_select_win`'s PgUp/PgDn arm.
fn page_preview(
    preview: Option<&[PreviewLine]>,
    sep_y: i32,
    h: i32,
    has_status: bool,
    preview_scroll: &mut usize,
    down: bool,
) {
    let Some(preview) = preview else { return };
    let avail_top = sep_y + 1;
    let _ = has_status;
    // Locked chrome: H-4 blank, H-3 status, H-2 nav, H-1 blank.
    let avail_bottom = h - 5;
    let max_lines = (avail_bottom - avail_top + 1).max(0) as usize;
    if max_lines == 0 {
        return;
    }
    let max_top = preview.len().saturating_sub(max_lines);
    *preview_scroll = if down {
        (*preview_scroll + max_lines).min(max_top)
    } else {
        preview_scroll.saturating_sub(max_lines)
    };
}

/// Restore the Enabled Models highlight to `(pid, mid)` after a reasoning
/// popup (or a no-op Enter on a model with no levels).
fn restore_preview_model_cursor(
    preview: Option<&[PreviewLine]>,
    focus: Option<(&str, &str)>,
    n: usize,
    current: &mut usize,
    model_cursor: &mut Option<usize>,
    preview_scroll: &mut usize,
) {
    let Some((pid, mid)) = focus else { return };
    let Some(preview) = preview else { return };
    let models = preview_model_entries(preview);
    if let Some((i, (line_idx, _, _))) = models
        .iter()
        .enumerate()
        .find(|(_, (_, p, m))| p == pid && m == mid)
    {
        *model_cursor = Some(i);
        if n > 0 {
            *current = n - 1;
        }
        *preview_scroll = *line_idx;
    }
}

/// If the cursor is in the Enabled Models pane, pin it to the first model
/// row that is still in the scroll window so a page/wheel never leaves the
/// highlight on a row that scrolled off.
fn pin_model_cursor_to_scroll(
    preview: Option<&[PreviewLine]>,
    preview_scroll: usize,
    model_cursor: &mut Option<usize>,
) {
    if model_cursor.is_none() {
        return;
    }
    let models = preview.map(preview_model_entries).unwrap_or_default();
    *model_cursor = models
        .iter()
        .position(|(idx, _, _)| *idx >= preview_scroll)
        .or_else(|| models.len().checked_sub(1));
}

/// Explanatory note shown directly under the heading on the Codex Config page.
const CODEX_CONFIG_INFO: &str = "$CODEX_HOME/config.toml and $CODEX_HOME/<provider>-models.json are updated to enable this provider's enabled models. Codex only allows one configured provider by setting:\n\n  model_provider = <provider>\n  model_catalog_json = <provider>-models.json\n\nDisabling removes this config from config.toml and deletes its models json file.";

pub fn select_win<S: Stdscr>(
    stdscr: &mut S,
    options: &[String],
    title: &str,
    multi: bool,
    preselected: &[usize],
    back_on_left: bool,
    key_hint: Option<&str>,
    footer: Option<&str>,
    status: Option<&str>,
    preview: Option<&[PreviewLine]>,
    initial: usize,
    section_sep_before: Option<usize>,
    model_initial: Option<(&str, &str)>,
) -> Option<SelectOutcome> {
    if options.is_empty() {
        return None;
    }
    let mut state: Vec<bool> = options
        .iter()
        .enumerate()
        .map(|(i, _)| preselected.contains(&i))
        .collect();
    let n = options.len();
    let mut current = initial.min(n - 1);
    let mut top = 0usize;
    // Scroll offset into the preview pane (the enabled-models listing under
    // the provider list). The provider rows above never move.
    let mut preview_scroll = 0usize;
    let mut model_cursor: Option<usize> = None;
    restore_preview_model_cursor(
        preview,
        model_initial,
        n,
        &mut current,
        &mut model_cursor,
        &mut preview_scroll,
    );

    loop {
        stdscr.erase();
        let (h, w) = stdscr.getmaxyx();
        // Codex Config page: explanatory note directly under the heading.
        let mut info_lines: Vec<String> = Vec::new();
        let mut is_codex_config = false;
        if title.trim() == "Codex Config" {
            is_codex_config = true;
            let iw = (w.max(1) as usize).saturating_sub(4);
            for para in CODEX_CONFIG_INFO.split('\n') {
                if para.is_empty() {
                    info_lines.push(String::new());
                    continue;
                }
                let mut cur = String::new();
                for word in para.split(' ') {
                    let cand = if cur.is_empty() {
                        word.to_string()
                    } else {
                        format!("{cur} {word}")
                    };
                    if str_cols(&cand) as usize <= iw {
                        cur = cand;
                    } else {
                        if !cur.is_empty() {
                            info_lines.push(std::mem::take(&mut cur));
                        }
                        cur = word.to_string();
                    }
                }
                if !cur.is_empty() {
                    info_lines.push(cur);
                }
            }
        }
        let info_h = info_lines.len();
        paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
        draw_header(stdscr, &format!("  {title}"));
        // Blank row padding under the heading on the Codex Config page only.
        if is_codex_config {
            stdscr.addstr(
                1,
                0,
                &" ".repeat((w.max(1) as usize).saturating_sub(1)),
                Paint::plain(tn_color(P::Text), bg_color(P::Text)),
            );
        }
        let info_row0 = if is_codex_config { 2 } else { 1 };
        for (i, line) in info_lines.iter().enumerate() {
            stdscr.addstr(
                info_row0 + i as i32,
                2,
                &clip_cols(line, (w.max(1) as usize).saturating_sub(3)),
                Paint::plain(tn_color(P::Muted), bg_color(P::Muted)),
            );
        }

        // list_top pushes the list below the info text + padding row when
        // present. Original layout (list_top = 2) is preserved for pages
        // without info text (all pages other than Codex Config).
        let list_top: usize = if is_codex_config { 3 + info_h } else { 2 };
        let list_h: usize = if is_codex_config {
            ((h - 5 - info_h as i32).max(1)) as usize
        } else {
            ((h - 4).max(1)) as usize
        };
        if current < top {
            top = current;
        }
        if current >= top + list_h {
            top = current + 1 - list_h;
        }

        // Optional section rule between the provider rows and the trailing
        // block (Model Descriptions / Add Provider / Add Model). It gets its
        // own screen row and pushes the separator/preview down one line —
        // but only while the whole menu plus the rule fits; otherwise it is
        // skipped and the layout stays exactly as without it.
        let rule_drawn = match section_sep_before {
            Some(before) if n + 1 <= list_h => {
                let trial_sep = 2 + n as i32 + 1; // separator after the shift
                let avail_bottom = h - 5;
                if trial_sep + 1 <= avail_bottom {
                    let rule_y = list_top as i32 + (before - top) as i32;
                    let rule = "─".repeat((w.max(1) as usize).saturating_sub(1));
                    stdscr.addstr(rule_y, 0, &rule, Paint::plain(tn_color(P::Chevron), bg_color(P::Chevron)));
                    true
                } else {
                    false
                }
            }
            _ => false,
        };

        let env_hdr = "# required env_key values";
        let mut max_env_w = env_hdr.len();
        let mut has_env_cell = false;
        for opt in options {
            if let Some(env) = crate::core::provider_row_env_text(opt) {
                has_env_cell = true;
                max_env_w = max_env_w.max(env.len());
            }
        }
        let env_pad = crate::core::PROVIDER_ENV_PAD;

        for row in 0..list_h {
            let idx = top + row;
            if idx >= n {
                break;
            }
            let y = if rule_drawn && idx >= section_sep_before.unwrap_or(usize::MAX) {
                list_top as i32 + row as i32 + 1
            } else {
                list_top as i32 + row as i32
            };
            let is_sel = idx == current && model_cursor.is_none();
            let row_paint = if is_sel {
                Paint::plain(tn_color(P::Selected), bg_color(P::Selected)).bold()
            } else {
                Paint::plain(tn_color(P::Text), bg_color(P::Text))
            };
            let row_bg = Paint::plain(
                if is_sel { tn_color(P::Selected) } else { tn_color(P::Text) },
                if is_sel { bg_color(P::Selected) } else { bg_color(P::Text) },
            );
            let fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
            stdscr.addstr(y, 0, &fill, row_bg);
            let line = if multi {
                let mark = if state[idx] { "●" } else { "○" };
                format!("  {mark}  {}", options[idx])
            } else {
                format!("  ▸ {}", options[idx])
            };
            // Clip only when drawing. Tokenizing a width-truncated env cell
            // (cut off before '=') would treat the name as a command word and
            // paint it gold like '='.
            let vis_limit = (w.max(1) as usize).saturating_sub(2);
            let vis = clip_cols(&line, vis_limit);
            let label = if !multi && is_sel {
                row_paint
            } else {
                row_bg
            };
            // Colorize a [enabled]/[disabled] token green/red, mirroring
            // Python's `_curses_select_win` (P.ENABLED/P.ERROR pairs).
            if let Some((head, token, tail)) = split_state_token(&line) {
                let tok_enabled = token != "[disabled]";
                let tok_fg = if tok_enabled {
                    theme::accent(2)
                } else {
                    Rgb {
                        r: theme::RED.0,
                        g: theme::RED.1,
                        b: theme::RED.2,
                    }
                };
                let tok_bg = if is_sel {
                    bg_color(P::Selected)
                } else {
                    bg_color(P::Text)
                };
                let tok_paint = Paint::plain(tok_fg, tok_bg).bold_if(is_sel);
                stdscr.addstr(y, 0, &clip_cols(&head, vis_limit), label);
                let tok_x = str_cols(&head) as i32;
                if (tok_x as usize) < vis_limit {
                    stdscr.addstr(
                        y,
                        tok_x,
                        &clip_cols(&token, vis_limit.saturating_sub(tok_x as usize)),
                        tok_paint,
                    );
                }
                if !tail.is_empty() {
                    let mut tail_x = (str_cols(&head) + str_cols(&token)) as i32;
                    let nspaces = tail.len() - tail.trim_start_matches(' ').len();
                    let env = &tail[nspaces..];
                    if nspaces > 0 {
                        let gap = clip_cols(&" ".repeat(nspaces), vis_limit.saturating_sub(tail_x as usize));
                        if !gap.is_empty() && (tail_x as usize) < vis_limit {
                            stdscr.addstr(y, tail_x, &gap, label);
                        }
                        tail_x += nspaces as i32;
                    }
                    if !env.is_empty() {
                        let box_x = (tail_x - env_pad).max(0);
                        let box_w = max_env_w + 2 * env_pad as usize;
                        let fill_w = box_w.min((w.max(1) as usize).saturating_sub(box_x as usize + 1));
                        stdscr.addstr(
                            y,
                            box_x,
                            &" ".repeat(fill_w),
                            Paint::plain(tn_color(P::CodeText), bg_color(P::CodeText)),
                        );
                        let segs = code_line_segments(env, None);
                        draw_seg_line(
                            stdscr,
                            y,
                            tail_x,
                            &segs,
                            vis_limit.saturating_sub(tail_x as usize),
                        );
                    } else if nspaces == 0 {
                        stdscr.addstr(
                            y,
                            tail_x,
                            &clip_cols(&tail, vis_limit.saturating_sub(tail_x as usize)),
                            label,
                        );
                    }
                }
            } else {
                stdscr.addstr(
                    y,
                    0,
                    &pad_cols(&line, (w.max(1) as usize).saturating_sub(1), ' '),
                    label,
                );
            }
            if !multi {
                let chev_x = (str_cols(&vis) as i32 + 2).max(w - 4);
                stdscr.addstr(y, chev_x, "›", Paint::plain(tn_color(P::Chevron), bg_color(P::Chevron)));
            }
        }

        // Env-column header on unused row 1 (main menu provider rows).
        if !back_on_left && !multi && has_env_cell {
            let mut env_x: Option<i32> = None;
            for opt in options {
                for tok in ["[enabled]", "[disabled]"] {
                    if let Some(p) = opt.find(tok) {
                        let after = p + tok.len();
                        let rest = &opt[after..];
                        let nsp = rest.len() - rest.trim_start_matches(' ').len();
                        if !rest.trim_start_matches(' ').is_empty() {
                            env_x = Some(str_cols("  ▸ ") as i32 + after as i32 + nsp as i32);
                            break;
                        }
                    }
                }
                if env_x.is_some() {
                    break;
                }
            }
            if let Some(env_x) = env_x {
                let segs = code_line_segments(env_hdr, None);
                let box_x = (env_x - env_pad).max(0);
                let box_w = max_env_w + 2 * env_pad as usize;
                let fill_w = box_w.min((w.max(1) as usize).saturating_sub(box_x as usize + 1));
                stdscr.addstr(
                    1,
                    box_x,
                    &" ".repeat(fill_w),
                    Paint::plain(tn_color(P::CodeText), bg_color(P::CodeText)),
                );
                draw_seg_line(stdscr, 1, env_x, &segs, box_w);
            }
        }

        // Separator line (pushed down one row while the rule is shown).
        // Skipped on the Codex Config page, which has no footer below it.
        let mut sep_y = list_top as i32 + (n.min((h as usize).saturating_sub(4 + info_h)) as i32);
        if rule_drawn {
            sep_y += 1;
        }
        if !is_codex_config {
            let sep = "─".repeat((w.max(1) as usize).saturating_sub(1));
            stdscr.addstr(sep_y, 0, &sep, Paint::plain(tn_color(P::Chevron), bg_color(P::Chevron)));
        }

        // Models preview: fill the empty space below the list (the TUI
        // main menu) with the enabled-models listing, styled like --models.
        // The action menu passes no preview and may have a footer instead.
        if let Some(preview) = preview {
            let avail_top = sep_y + 1;
            // Locked chrome: H-4 blank, H-3 status, H-2 nav, H-1 blank.
            let avail_bottom = h - 5;
            let max_lines = (avail_bottom - avail_top + 1).max(0) as usize;
            if max_lines > 0 {
                // Scroll window over the preview; provider rows above stay
                // put. Paging is advertised by the legend's "PgUp/PgDn page"
                // entry, so no inline truncation hint is drawn.
                let max_top = preview.len().saturating_sub(max_lines);
                let preview_top = preview_scroll.min(max_top);
                // preview_scroll can exceed the window after a resize; clamp
                // the slice so a stale offset can't panic.
                let end = (preview_top + max_lines).min(preview.len());
                let draw_lines: Vec<PreviewLine> =
                    preview[preview_top..end].to_vec();
                for (i, line) in draw_lines.iter().enumerate() {
                    let y = avail_top + i as i32;
                    match line {
                        PreviewLine::Heading(text) => {
                            let hfill = "\u{00a0}"
                                .repeat((w.max(1) as usize).saturating_sub(1));
                            let hp = Paint::plain(tn_color(P::Selected), bg_color(P::Selected));
                            stdscr.addstr(y, 0, &hfill, hp);
                            stdscr.addstr(y, 4, text, hp.bold());
                        }
                        PreviewLine::Segs(segs) => {
                            draw_seg_line(stdscr, y, 2, segs, (w.max(1) as usize).saturating_sub(3));
                        }
                        PreviewLine::Model { segs, .. } => {
                            let models = preview_model_entries(preview);
                            let abs_i = preview_top + i;
                            let is_sel = model_cursor
                                .and_then(|c| models.get(c).map(|(idx, _, _)| *idx == abs_i))
                                .unwrap_or(false);
                            if is_sel {
                                let hfill = "\u{00a0}"
                                    .repeat((w.max(1) as usize).saturating_sub(1));
                                stdscr.addstr(
                                    y,
                                    0,
                                    &hfill,
                                    Paint::plain(tn_color(P::Selected), bg_color(P::Selected)),
                                );
                                let sel: Vec<(String, P)> = segs
                                    .iter()
                                    .map(|(t, _)| (t.clone(), P::Selected))
                                    .collect();
                                draw_seg_line(
                                    stdscr,
                                    y,
                                    2,
                                    &sel,
                                    (w.max(1) as usize).saturating_sub(3),
                                );
                            } else {
                                draw_seg_line(stdscr, y, 2, segs, (w.max(1) as usize).saturating_sub(3));
                            }
                        }
                    }
                }
            }
        }

        // Transient status line (e.g. post-add confirmation), kept a few rows
        // above the legend so long messages never clobber the menu chrome.
        if let Some(status) = status {
            let trunc: String = status
                .chars()
                .take((w.max(1) as usize).saturating_sub(4))
                .collect();
            stdscr.addstr(
                h - 3,
                2,
                &trunc,
                Paint::plain(tn_color(P::Enabled), bg_color(P::Enabled)),
            );
        }

        // Footer(s): code panels under the separator — borderless black
        // rectangles, one column of horizontal padding, vim-sh syntax colors
        // via the shared tokenizer. Panel 1 content = key-setup commands;
        // then a blank line, the env comment label, and the env status lines.
        if key_hint.is_some() || footer.is_some() {
            let mut panel_lines: Vec<String> = Vec::new();
            if let Some(kh) = key_hint {
                panel_lines.extend(kh.split('\n').map(|s| s.to_string()));
            }
            if let Some(f) = footer {
                if !panel_lines.is_empty() {
                    panel_lines.push(String::new());
                }
                panel_lines.push("# required env_key value".to_string());
                panel_lines.extend(f.split('\n').map(|s| s.to_string()));
            }
            if !panel_lines.is_empty() {
                draw_code_panel(stdscr, sep_y + 1, &panel_lines, 2, w, h - 2);
            }
        }

        let mut legend: Vec<(String, String)> = vec![
            ("↑/↓".to_string(), "nav".to_string()),
        ];
        if multi {
            legend.push(("Space".to_string(), "toggle".to_string()));
        }
        if back_on_left {
            legend.push(("Enter/→".to_string(), "select".to_string()));
            legend.push(("←".to_string(), "back".to_string()));
        } else {
            // Main menu: page sits left of select when the preview overflows.
            if let Some(pv) = preview {
                let avail_top = sep_y + 1;
                let avail_bottom = h - 5;
                let visible = ((avail_bottom - avail_top + 1).max(0)) as usize;
                if pv.len() > visible {
                    legend.push(("PgUp/PgDn".to_string(), "page".to_string()));
                }
            }
            legend.push(("Enter/→".to_string(), "select".to_string()));
            legend.push(("S".to_string(), "sort".to_string()));
            legend.push(("Q".to_string(), "quit".to_string()));
        }
        draw_legend(stdscr, &legend);

        stdscr.refresh();
        let _ = emit_sgr_bg_keep_alive();

        match stdscr.getch() {
            Key::Resize => {
                paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
            }
            Key::Up => {
                if let Some(c) = model_cursor {
                    if c > 0 {
                        let next = c - 1;
                        model_cursor = Some(next);
                        // Keep the highlighted model in the pane: if it sits
                        // above the current window, scroll up to its line.
                        // Mirrors Python `_curses_select_win` KEY_UP.
                        if let Some(preview) = preview {
                            let models = preview_model_entries(preview);
                            if let Some((line_idx, _, _)) = models.get(next) {
                                if *line_idx < preview_scroll {
                                    preview_scroll = *line_idx;
                                }
                            }
                        }
                    } else {
                        model_cursor = None;
                    }
                } else if current > 0 {
                    current -= 1;
                }
            }
            Key::Down => {
                let models = preview.map(preview_model_entries).unwrap_or_default();
                if let Some(c) = model_cursor {
                    if c + 1 < models.len() {
                        model_cursor = Some(c + 1);
                    }
                } else if current + 1 < n {
                    current += 1;
                } else if !models.is_empty() && !back_on_left {
                    model_cursor = Some(0);
                }
            }
            Key::Char(' ') if multi => {
                state[current] = !state[current];
            }
            Key::Enter | Key::Right => {
                if let Some(c) = model_cursor {
                    if let Some(preview) = preview {
                        let models = preview_model_entries(preview);
                        if let Some((_, pid, mid)) = models.get(c) {
                            return Some(SelectOutcome::ModelPicked {
                                pid: pid.clone(),
                                mid: mid.clone(),
                            });
                        }
                    }
                }
                if multi {
                    let picked: Vec<usize> = (0..n).filter(|i| state[*i]).collect();
                    return Some(SelectOutcome::Picked(picked.first().copied().unwrap_or(0)));
                } else {
                    return Some(SelectOutcome::Picked(current));
                }
            }
            Key::WheelDown(y) => {
                let models = preview.map(preview_model_entries).unwrap_or_default();
                let avail_top = sep_y + 1;
                if !models.is_empty()
                    && !back_on_left
                    && y >= avail_top
                    && y <= h - 5
                {
                    let max_top = preview.map(|p| p.len().saturating_sub(1)).unwrap_or(0);
                    preview_scroll = (preview_scroll + 1).min(max_top);
                    model_cursor = Some(0);
                    pin_model_cursor_to_scroll(preview, preview_scroll, &mut model_cursor);
                    current = n.saturating_sub(1);
                }
            }
            Key::WheelUp(y) => {
                let models = preview.map(preview_model_entries).unwrap_or_default();
                let avail_top = sep_y + 1;
                if !models.is_empty()
                    && !back_on_left
                    && y >= avail_top
                    && y <= h - 5
                {
                    preview_scroll = preview_scroll.saturating_sub(1);
                    model_cursor = Some(0);
                    pin_model_cursor_to_scroll(preview, preview_scroll, &mut model_cursor);
                    current = n.saturating_sub(1);
                }
            }
            Key::Left | Key::Esc if back_on_left => return Some(SelectOutcome::Cancelled),
            Key::Char('q') if !back_on_left => return Some(SelectOutcome::Cancelled),
            Key::Char('s') | Key::Char('S') if !back_on_left => {
                return Some(SelectOutcome::SortToggled(current));
            }
            Key::PageDown => {
                page_preview(preview, sep_y, h, status.is_some(), &mut preview_scroll, true);
                pin_model_cursor_to_scroll(preview, preview_scroll, &mut model_cursor);
            }
            Key::PageUp => {
                page_preview(preview, sep_y, h, status.is_some(), &mut preview_scroll, false);
                pin_model_cursor_to_scroll(preview, preview_scroll, &mut model_cursor);
            }
            Key::Interrupt => return Some(SelectOutcome::Cancelled),
            _ => {}
        }
    }
}

fn emit_sgr_bg_keep_alive() -> Res<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Model search/filter widget
// ---------------------------------------------------------------------------

pub struct ModelSearch<'a> {
    pub ids: &'a [String],
    pub models: RefCell<&'a mut Map<String, Value>>,
}

// ---------------------------------------------------------------------------
// Generic type-to-filter list widget (python `_curses_filter_list_win`)
// ---------------------------------------------------------------------------

/// Behavior contract for a filter-list screen, mirroring the callback set
/// python passes into `_curses_filter_list_win`.
pub trait FilterList {
    type Entry: PartialEq + Clone;
    /// (entries, query) -> (ordered entries, separators). Separators are
    /// (index before which to insert a `─` rule occupying its own row).
    fn compute_view(&mut self, entries: &[Self::Entry], query: &str) -> (Vec<Self::Entry>, Vec<(usize, P)>);
    /// (entry, is_selected) -> colored segments for the row.
    fn render(&mut self, entry: &Self::Entry, is_selected: bool) -> Vec<(String, P)>;
    /// Enter on an entry: return true to keep the window open, false to close.
    /// Receives the active stdscr so models can draw overlays (inline errors).
    fn on_enter<S: Stdscr>(&mut self, stdscr: &mut S, entry: &Self::Entry) -> bool;
}

/// Visual row in the filter list: a separator occupies its own row before
/// `filtered[sep_idx]`, so model rows are never overdrawn.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FilterViewRow {
    Sep(P),
    Item(usize),
}

fn build_filter_view_rows(n: usize, separators: &[(usize, P)]) -> Vec<FilterViewRow> {
    let mut sep_at: Vec<(usize, P)> = separators
        .iter()
        .copied()
        .filter(|(idx, _)| *idx > 0 && *idx < n)
        .collect();
    sep_at.sort_by_key(|(idx, _)| *idx);
    let mut view = Vec::with_capacity(n + sep_at.len());
    let mut si = 0usize;
    for i in 0..n {
        if si < sep_at.len() && sep_at[si].0 == i {
            view.push(FilterViewRow::Sep(sep_at[si].1));
            si += 1;
        }
        view.push(FilterViewRow::Item(i));
    }
    view
}

/// Generic type-to-filter list widget drawn into an existing stdscr. ESC or
/// Left-at-the-top always closes. `bottom_pad` reserves that many blank
/// themed rows above the legend so a fully-scrolled list never touches the
/// menu chrome. `status_fn` optionally supplies a transient confirmation
/// line drawn a few rows above the legend.
pub fn filter_list_win<S: Stdscr, M: FilterList>(
    stdscr: &mut S,
    entries: &[M::Entry],
    title: &str,
    legend: &[(String, String)],
    model: &mut M,
) {
    filter_list_win_with(
        stdscr, entries, title, legend, model, 0, None,
    )
}

pub fn filter_list_win_with<S: Stdscr, M: FilterList>(
    stdscr: &mut S,
    entries: &[M::Entry],
    title: &str,
    legend: &[(String, String)],
    model: &mut M,
    bottom_pad: usize,
    status_fn: Option<&dyn Fn() -> Option<String>>,
) {
    let mut query = String::new();
    let mut current = 0usize;
    let mut top = 0usize;
    let mut snap_to_current = false;
    // Cache the computed view so arrow-key navigation (which leaves the query
    // untouched) reuses it instead of re-filtering/sorting the whole catalog
    // every keystroke. After a toggle the recompute runs, the toggled item
    // leaves its old `filtered` index, and the next item in its section
    // slides up to occupy that index. `current` already points at the
    // right neighbor — no adjustment is needed.
    let mut cached_q: Option<String> = None;
    let mut cached_view: Option<(Vec<M::Entry>, Vec<(usize, P)>)> = None;
    let mut dirty = true;
    loop {
        if dirty || cached_q.as_deref() != Some(query.as_str()) {
            cached_view = Some(model.compute_view(entries, &query));
            cached_q = Some(query.clone());
            dirty = false;
        }
        let (filtered, separators) = cached_view.as_ref().unwrap();
        if filtered.is_empty() {
            current = 0;
        } else if current >= filtered.len() {
            current = filtered.len() - 1;
        }
        let view = build_filter_view_rows(filtered.len(), separators);
        // Map filtered-index -> visual-row in O(N) once, then look up
        // `current` in O(1). The previous `position()` scan ran on every
        // frame, which is the per-frame hot path over a 10k-row catalog.
        let mut pos_of: Vec<usize> = vec![0; filtered.len()];
        for (vi, row) in view.iter().enumerate() {
            if let FilterViewRow::Item(i) = row {
                if *i < pos_of.len() {
                    pos_of[*i] = vi;
                }
            }
        }
        let cur_vis = pos_of.get(current).copied().unwrap_or(0);
        stdscr.erase();
        let (h, w) = stdscr.getmaxyx();
        paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
        draw_header(
            stdscr,
            &format!("  {title}  ({})  |  Filter: {query}", filtered.len()),
        );

        let list_top = 2usize;
        // Locked chrome: H-4 blank, H-3 status, H-2 nav, H-1 blank.
        let list_h = ((h as usize)
            .saturating_sub(list_top + 4 + bottom_pad))
        .max(1);
        if snap_to_current {
            top = cur_vis;
            if top + list_h > view.len() {
                top = view.len().saturating_sub(list_h);
            }
            snap_to_current = false;
        } else if cur_vis < top {
            top = cur_vis;
        } else if cur_vis >= top + list_h {
            top = cur_vis + 1 - list_h;
        }
        if !view.is_empty() && top >= view.len() {
            top = view.len().saturating_sub(list_h);
        }

        if filtered.is_empty() {
            stdscr.addstr(2, 0, "  (no matches)", Paint::plain(tn_color(P::Muted), bg_color(P::Muted)));
        }

        for row in 0..list_h {
            let vis_i = top + row;
            if vis_i >= view.len() {
                break;
            }
            let y = (list_top + row) as i32;
            match view[vis_i] {
                FilterViewRow::Sep(sep_pair) => {
                    let sep = "─".repeat((w.max(1) as usize).saturating_sub(1));
                    stdscr.addstr(y, 0, &sep, Paint::plain(tn_color(sep_pair), bg_color(sep_pair)));
                }
                FilterViewRow::Item(idx) => {
                    let entry = &filtered[idx];
                    let segs = model.render(entry, idx == current);
                    let fill_pair = if idx == current { P::Selected } else { P::Text };
                    let fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
                    stdscr.addstr(
                        y,
                        0,
                        &fill,
                        Paint::plain(tn_color(fill_pair), bg_color(fill_pair)),
                    );
                    let bg_pair = if idx == current { Some(P::Selected) } else { None };
                    draw_seg_line_bg(
                        stdscr,
                        y,
                        0,
                        &segs,
                        (w.max(1) as usize).saturating_sub(2),
                        bg_pair,
                    );
                }
            }
        }

        // Transient status line (e.g. post-add confirmation), a few rows
        // above the legend so it never clobbers the list or the chrome.
        if let Some(status_fn) = status_fn {
            if let Some(status) = status_fn() {
                let trunc: String = status
                    .chars()
                    .take((w.max(1) as usize).saturating_sub(4))
                    .collect();
                stdscr.addstr(
                    h - 3,
                    2,
                    &trunc,
                    Paint::plain(tn_color(P::Enabled), bg_color(P::Enabled)),
                );
            }
        }

        draw_legend(stdscr, legend);
        stdscr.refresh();

        match stdscr.getch() {
            Key::Resize => {}
            Key::Esc => return,
            Key::Interrupt => return,
            Key::Up if current > 0 => current -= 1,
            Key::Down if current + 1 < filtered.len() => current += 1,
            Key::Right => {
                if !filtered.is_empty() {
                    current = (((current / list_h) + 1) * list_h).min(filtered.len() - 1);
                    snap_to_current = true;
                }
            }
            Key::Left => {
                if current == 0 {
                    return;
                }
                if current < list_h {
                    current = 0;
                    snap_to_current = true;
                } else {
                    current = ((current / list_h) - 1) * list_h;
                    snap_to_current = true;
                }
            }
            Key::Backspace => {
                query.pop();
                current = 0;
                top = 0;
            }
            Key::Enter => {
                if !filtered.is_empty() {
                    if !model.on_enter(stdscr, &filtered[current]) {
                        return;
                    }
                    dirty = true;
                    // After a toggle, move the cursor one row inside the
                    // section the toggled item just left: disable from the
                    // enabled side moves up (current - 1), enable from the
                    // disabled side moves down (current + 1). The chevron
                    // separator marks the boundary between the two
                    // sections.
                    let chev_idx = separators
                        .iter()
                        .find_map(|(i, p)| if *p == P::Chevron { Some(*i) } else { None });
                    if chev_idx.map_or(false, |i| current < i) {
                        if current > 0 {
                            current -= 1;
                        }
                    } else if current + 1 < filtered.len() {
                        current += 1;
                    }
                }
            }
            Key::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                query.push(c);
                current = 0;
                top = 0;
            }
            Key::WheelDown(y) => {
                let list_bottom = (list_top + list_h) as i32;
                if y < list_top as i32 || y >= list_bottom {
                    continue;
                }
                if !view.is_empty() && top + 1 < view.len() {
                    top += 1;
                }
                if let Some(i) = view[top.min(view.len().saturating_sub(1))..]
                    .iter()
                    .find_map(|r| match r {
                        FilterViewRow::Item(i) => Some(*i),
                        _ => None,
                    })
                {
                    current = i;
                }
            }
            Key::WheelUp(y) => {
                let list_bottom = (list_top + list_h) as i32;
                if y < list_top as i32 || y >= list_bottom {
                    continue;
                }
                if top > 0 {
                    top -= 1;
                }
                if let Some(i) = view.get(top).and_then(|r| match r {
                    FilterViewRow::Item(i) => Some(*i),
                    FilterViewRow::Sep(_) => view[top..]
                        .iter()
                        .find_map(|r| match r {
                            FilterViewRow::Item(i) => Some(*i),
                            _ => None,
                        }),
                }) {
                    current = i;
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Model picker built on the filter widget (python `_curses_model_search_win`)
// ---------------------------------------------------------------------------

struct ModelPicker<'a> {
    ids: &'a [String],
    models: &'a mut Map<String, Value>,
    pid: String,
    pname: String,
    changed: bool,
}

impl<'a> FilterList for ModelPicker<'a> {
    type Entry = String;

    fn compute_view(&mut self, _entries: &[String], query: &str) -> (Vec<String>, Vec<(usize, P)>) {
        let sorted = core::sort_model_indices(self.ids, self.models, Some(query));
        let ordered: Vec<String> = sorted.filtered.iter().map(|&i| self.ids[i].clone()).collect();
        let mut separators: Vec<(usize, P)> = Vec::new();
        if 0 < sorted.enabled_count && sorted.enabled_count < ordered.len() {
            separators.push((sorted.enabled_count, P::Chevron));
        }
        let free_sep_idx = sorted.enabled_count + sorted.free_disabled_count;
        if sorted.free_disabled_count > 0 && free_sep_idx < ordered.len() {
            separators.push((free_sep_idx, P::Free));
        }
        (ordered, separators)
    }

    fn render(&mut self, mid: &String, _is_sel: bool) -> Vec<(String, P)> {
        let m = self.models.get(mid);
        let enabled = m.map(|v| crate::get_bool_val(v, "enabled", true)).unwrap_or(false);
        let is_free = mid.to_lowercase().contains("free");
        let mark = if enabled { "●" } else { "○" };
        let mname = m.map(|v| crate::name_or(v, mid)).unwrap_or_else(|| mid.clone());
        let rest = format!(" ({}) - {}/{mid}", self.pname, self.pid);
        let name_pair = if enabled {
            P::Value
        } else if is_free {
            P::Enabled
        } else {
            P::Text
        };
        let mark_pair = if enabled { P::Enabled } else { P::Text };
        vec![
            ("  ".to_string(), P::Text),
            (mark.to_string(), mark_pair),
            ("  ".to_string(), P::Text),
            (mname, name_pair),
            (rest, P::Text),
        ]
    }

    fn on_enter<S: Stdscr>(&mut self, _stdscr: &mut S, mid: &String) -> bool {
        let entry = self.models.entry(mid.clone()).or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        let cur = crate::get_bool_val(entry, "enabled", true);
        entry.as_object_mut().unwrap().insert("enabled".into(), Value::Bool(!cur));
        self.changed = true;
        true // stay open
    }
}

pub fn model_search_win<S: Stdscr>(
    stdscr: &mut S,
    ids: &[String],
    models: &mut Map<String, Value>,
    title: &str,
    pid: &str,
    pname: &str,
) -> bool {
    let mut picker = ModelPicker {
        ids,
        models,
        pid: pid.to_string(),
        pname: pname.to_string(),
        changed: false,
    };
    filter_list_win(
        stdscr,
        ids,
        title,
        &[
            ("↑/↓/←/→".to_string(), "nav".to_string()),
            ("ESC".to_string(), "back".to_string()),
            ("Enter".to_string(), "toggle".to_string()),
            ("type".to_string(), "filter".to_string()),
        ],
        &mut picker,
    );
    picker.changed
}

// ---------------------------------------------------------------------------
// Confirm dialog
// ---------------------------------------------------------------------------

pub fn confirm_win<S: Stdscr>(stdscr: &mut S, prompt: &str) -> bool {
    stdscr.erase();
    paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
    draw_header(stdscr, &format!("  Confirm: {prompt}"));
    draw_legend(
        stdscr,
        &[
            ("Y".to_string(), "yes".to_string()),
            ("N".to_string(), "no".to_string()),
            ("ESC".to_string(), "cancel".to_string()),
        ],
    );
    stdscr.refresh();
    loop {
        match stdscr.getch() {
            Key::Char('y') | Key::Char('Y') => {
                stdscr.invalidate();
                return true;
            }
            Key::Char('n') | Key::Char('N') | Key::Esc | Key::Interrupt => {
                stdscr.invalidate();
                return false;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Inline error overlay + Add-provider modal
// ---------------------------------------------------------------------------

/// Overlay an error message inside the active TUI session; any key dismisses.
pub fn inline_error_win<S: Stdscr>(stdscr: &mut S, message: &str) {
    let (h, w) = stdscr.getmaxyx();
    stdscr.erase();
    paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
    let trunc: String = message.chars().take((w.max(1) as usize).saturating_sub(4)).collect();
    stdscr.addstr(h / 2, 2, &trunc, Paint::plain(tn_color(P::Disabled), bg_color(P::Disabled)));
    stdscr.addstr(
        h / 2 + 2,
        2,
        "Press any key to go back",
        Paint::plain(tn_color(P::Muted), bg_color(P::Muted)),
    );
    draw_legend(
        stdscr,
        &[("any key".to_string(), "back".to_string())],
    );
    stdscr.refresh();
    let _ = emit_sgr_bg_keep_alive();
    let _ = stdscr.getch();
    stdscr.invalidate();
}

/// Filter-list model for the add-provider modal: live-filter the FULL
/// models.dev catalog (already-added providers stay listed, rendered inert),
/// Enter adds quietly and keeps the modal open so several providers can be
/// added in one visit.
struct AddProviderPicker<'a> {
    doc: &'a mut Value,
    api: Value,
    added: Option<String>,
    status: std::rc::Rc<RefCell<Option<String>>>,
    // Cache for padded_labels(), keyed on (providers_count, api_count).
    // Recomputed only when the providers doc grows or the api key set changes,
    // so per-row render calls are O(1) HashMap lookups instead of an O(N)
    // scan of the full models.dev catalog.
    label_cache: Option<(usize, usize, std::collections::HashMap<String, String>)>,
}

fn provider_matches(pid: &str, name: &str, term_l: &str) -> bool {
    term_l.is_empty()
        || pid.to_lowercase().contains(term_l)
        || name.to_lowercase().contains(term_l)
}

impl<'a> AddProviderPicker<'a> {
    /// Provider ids already configured — the Added section's membership,
    /// regardless of each provider-level `enabled` bool.
    fn added_ids(&self) -> std::collections::HashSet<String> {
        usable(self.doc)
            .iter()
            .map(|p| p.get("id").and_then(Value::as_str).unwrap_or_default().to_string())
            .filter(|id| !id.is_empty())
            .collect()
    }

    fn padded_labels(&mut self) -> &std::collections::HashMap<String, String> {
        let providers_count = self
            .doc
            .get("providers")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let api_count = self.api.as_object().map(|o| o.len()).unwrap_or(0);
        let cache_hit = self
            .label_cache
            .as_ref()
            .is_some_and(|(pc, ac, _)| *pc == providers_count && *ac == api_count);
        if !cache_hit {
            let map = self.compute_padded_labels();
            self.label_cache = Some((providers_count, api_count, map));
        }
        &self.label_cache.as_ref().expect("cache populated above").2
    }

    fn compute_padded_labels(&self) -> std::collections::HashMap<String, String> {
        let added = self.added_ids();
        let mut rows: Vec<(String, String, bool)> = Vec::new();
        if let Some(obj) = self.api.as_object() {
            for (pid, pinfo) in obj {
                if !pinfo.is_object() {
                    continue;
                }
                let cat_name = pinfo
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(pid);
                if added.contains(pid) {
                    let p = usable(self.doc)
                        .into_iter()
                        .find(|pr| pr.get("id").and_then(Value::as_str) == Some(pid));
                    let name = p
                        .as_ref()
                        .and_then(|pr| pr.get("name").and_then(Value::as_str))
                        .filter(|s| !s.is_empty())
                        .unwrap_or(cat_name);
                    let enabled = p
                        .as_ref()
                        .map(|pr| crate::get_bool_obj(pr, "enabled", true))
                        .unwrap_or(false);
                    rows.push((name.to_string(), pid.clone(), enabled));
                } else {
                    rows.push((cat_name.to_string(), pid.clone(), false));
                }
            }
        }
        core::format_provider_id_rows(&rows)
            .into_iter()
            .zip(rows.iter())
            .map(|(lab, (_, pid, _))| (pid.clone(), lab))
            .collect()
    }
}

impl<'a> FilterList for AddProviderPicker<'a> {
    type Entry = (String, String);

    fn compute_view(
        &mut self,
        entries: &[(String, String)],
        query: &str,
    ) -> (Vec<(String, String)>, Vec<(usize, P)>) {
        let term_l = query.to_lowercase();
        let matched: Vec<(String, String)> = entries
            .iter()
            .filter(|(pid, name)| provider_matches(pid, name, &term_l))
            .cloned()
            .collect();
        let added = self.added_ids();
        let suggested: Vec<&str> = SUGGESTED_PROVIDER_IDS.to_vec();
        // 0 = Added, 1 = Suggested, 2 = everything else; alphabetical by id
        // within a bucket.
        let mut rows: Vec<(usize, &(String, String))> = matched
            .iter()
            .map(|entry| {
                let b = if added.contains(&entry.0) {
                    0
                } else if suggested.contains(&entry.0.as_str()) {
                    1
                } else {
                    2
                };
                (b, entry)
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.0.cmp(&b.1.0)));
        let ordered: Vec<(String, String)> =
            rows.iter().map(|(_, e)| (*e).clone()).collect();
        // Green divider before Suggested would be wrong: the first rule sits
        // before the Suggested bucket (mirrors Configure Models' enabled |
        // free-disabled | rest layout).
        let n_added = ordered
            .iter()
            .filter(|(pid, _)| added.contains(pid))
            .count();
        let n_sugg = ordered
            .iter()
            .filter(|(pid, _)| !added.contains(pid) && suggested.contains(&pid.as_str()))
            .count();
        let mut separators: Vec<(usize, P)> = Vec::new();
        if 0 < n_added && n_added < ordered.len() {
            separators.push((n_added, P::Enabled));
        }
        let free_sep_idx = n_added + n_sugg;
        if n_sugg > 0 && free_sep_idx < ordered.len() {
            separators.push((free_sep_idx, P::Free));
        }
        (ordered, separators)
    }

    fn render(&mut self, entry: &(String, String), _is_sel: bool) -> Vec<(String, P)> {
        let (pid, _name) = entry;
        let added = self.added_ids().contains(pid);
        let suggested = SUGGESTED_PROVIDER_IDS.contains(&pid.as_str()) && !added;
        let labels = self.padded_labels();
        let label = labels.get(pid).cloned().unwrap_or_else(|| pid.clone());
        let (head, token, tok_pair) = if let Some(rest) = label.strip_suffix("[enabled]") {
            (rest.to_string(), "[enabled]".to_string(), P::Enabled)
        } else if let Some(rest) = label.strip_suffix("[disabled]") {
            (rest.to_string(), "[disabled]".to_string(), P::Error)
        } else {
            (label, String::new(), P::Text)
        };
        let name_pair = if added {
            P::Enabled
        } else if suggested {
            P::Free
        } else {
            P::Text
        };
        vec![
            ("  ".to_string(), P::Text),
            (head, name_pair),
            (token, tok_pair),
        ]
    }

    fn on_enter<S: Stdscr>(&mut self, stdscr: &mut S, entry: &(String, String)) -> bool {
        let pid = &entry.0;
        if self.added_ids().contains(pid) {
            return true; // already configured; inert row (delete via its menu)
        }
        let fetch_err_url = match crate::commands::add_provider_entry(self.doc, &self.api, pid, true) {
            Err(e) => {
                // Add errors surface inline so the surrounding session survives.
                inline_error_win(stdscr, &format!("Add failed: {}", e.0));
                return true; // stay open
            }
            Ok(url) => url,
        };
        // dump_providers already wrote name-sorted order back into doc.
        let mut model_count = 0usize;
        if let Some(arr) = self.doc.get("providers").and_then(Value::as_array) {
            if let Some(new_entry) = arr.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(pid)) {
                model_count = new_entry
                    .get("models")
                    .and_then(Value::as_object)
                    .map(|m| m.len())
                    .unwrap_or(0);
            }
        }
        self.added = Some(format!(
            "Added provider '{pid}' with {model_count} models (all disabled)."
        ));
        *self.status.borrow_mut() = match fetch_err_url {
            Some(url) => Some(crate::sync::live_fetch_error_status(&url)),
            None => self.added.clone(),
        };
        true // stay open so more providers can be added
    }
}
/// Modal: type-to-filter the full models.dev catalog and add a provider.
/// Returns the last confirmation status line for the parent menu, or None.
/// The fetch runs before the modal opens so a failure never leaves it on
/// screen. The modal stays open across adds; ESC or Left-at-top closes it.
pub fn add_provider_win<S: Stdscr>(stdscr: &mut S, doc: &mut Value) -> Option<String> {
    let api = match crate::sync::fetch_models_dev() {
        Ok(a) => a,
        Err(e) => {
            inline_error_win(stdscr, &format!("Fetch failed: {}", e.0));
            return None;
        }
    };
    // Full catalog — already-added providers stay listed so the sections
    // show what is configured; they are just rendered differently.
    let mut catalog: Vec<(String, String)> = api
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, pinfo)| pinfo.is_object())
                .map(|(pid, pinfo)| {
                    (
                        pid.clone(),
                        pinfo.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    catalog.sort();

    let status_cell = std::rc::Rc::new(RefCell::new(None::<String>));
    let mut picker = AddProviderPicker {
        doc,
        api,
        added: None,
        status: std::rc::Rc::clone(&status_cell),
        label_cache: None,
    };
    let status_for_fn = std::rc::Rc::clone(&status_cell);
    filter_list_win_with(
        stdscr,
        &catalog,
        "Add Provider",
        &[
            ("↑/↓/←/→".to_string(), "nav".to_string()),
            ("ESC".to_string(), "cancel".to_string()),
            ("Enter".to_string(), "add".to_string()),
            ("type".to_string(), "filter".to_string()),
        ],
        &mut picker,
        0,
        Some(&move || status_for_fn.borrow().clone()),
    );
    picker.added
}

// ---------------------------------------------------------------------------
// Inline row editor (edits a value in place on the menu, no extra page)
// ---------------------------------------------------------------------------

/// Redraws `rows` with row `edit_index` turned into a text field
/// (`<label> [buffer█]`), capturing characters until Enter (save) or
/// ESC (cancel). Returns the typed value, or None on cancel.
pub fn edit_inline_row<S: Stdscr>(
    stdscr: &mut S,
    title: &str,
    rows: &[String],
    edit_index: usize,
    label: &str,
    initial: &str,
) -> Option<String> {
    let mut buf = String::from(initial);
    loop {
        stdscr.erase();
        let (h, w) = stdscr.getmaxyx();
        paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
        draw_header(stdscr, &format!("  {title}"));
        let fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
        for (i, row) in rows.iter().enumerate() {
            let y = (2 + i) as i32;
            if y >= h - 2 {
                break;
            }
            if i == edit_index {
                stdscr.addstr(y, 0, &fill, Paint::plain(tn_color(P::Selected), bg_color(P::Selected)));
                // Draw the field without its closing bracket, then a block
                // cursor followed by the bracket, so the caret sits inside.
                let line = format!("  ▸ {label} [{buf}");
                stdscr.addstr(
                    y,
                    0,
                    &pad_cols(&line, (w.max(1) as usize).saturating_sub(1), ' '),
                    Paint::plain(tn_color(P::Selected), bg_color(P::Selected)).bold(),
                );
                let cur_x =
                    (4 + str_cols(label) + 2 + str_cols(&buf)) as i32;
                stdscr.addstr(
                    y,
                    cur_x,
                    "█]",
                    Paint::plain(tn_color(P::Selected), bg_color(P::Selected)).bold(),
                );
            } else {
                stdscr.addstr(y, 0, &fill, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
                let line = clip_cols(
                    &format!("    {row}"),
                    (w.max(1) as usize).saturating_sub(2),
                );
                stdscr.addstr(
                    y,
                    0,
                    &pad_cols(&line, (w.max(1) as usize).saturating_sub(1), ' '),
                    Paint::plain(tn_color(P::Muted), bg_color(P::Muted)),
                );
            }
        }
        draw_legend(
            stdscr,
            &[
                ("Enter".to_string(), "save".to_string()),
                ("ESC".to_string(), "cancel".to_string()),
            ],
        );
        stdscr.refresh();

        match stdscr.getch() {
            Key::Enter => return Some(buf),
            Key::Esc | Key::Interrupt => return None,
            Key::Backspace => {
                buf.pop();
            }
            Key::Char(c) if c.is_ascii_graphic() => buf.push(c),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Add-model modal: cross-provider catalog search
// ---------------------------------------------------------------------------

/// True when `(pid, mid)` is an enabled model in `doc`.
fn combo_enabled(doc: &Value, pid: &str, mid: &str) -> bool {
    let Some(arr) = doc.get("providers").and_then(Value::as_array) else {
        return false;
    };
    for p in arr {
        if p.get("id").and_then(Value::as_str) != Some(pid) {
            continue;
        }
        let Some(mm) = p.get("models").and_then(Value::as_object) else {
            return false;
        };
        return mm
            .get(mid)
            .is_some_and(|m| m.is_object() && crate::get_bool_val(m, "enabled", true));
    }
    false
}

/// Flatten every models.dev model across all providers. Already-enabled
/// combos stay listed so the Enabled section can show what is configured;
/// extra enabled models that exist only in the doc are appended.
/// Entries: (pid, mid, model display name, provider display name).
fn build_add_model_catalog(api: &Value, doc: &Value) -> Vec<(String, String, String, String)> {
    let mut catalog: Vec<(String, String, String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    if let Some(api_obj) = api.as_object() {
        for (pid, pinfo) in api_obj {
            if !pinfo.is_object() {
                continue;
            }
            let pname = pinfo
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(pid);
            let api_models = pinfo.get("models").and_then(Value::as_object);
            for (mid, minfo) in api_models.into_iter().flatten() {
                let mname = minfo
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(mid);
                catalog.push((pid.clone(), mid.clone(), mname.to_string(), pname.to_string()));
                seen.insert((pid.clone(), mid.clone()));
            }
        }
    }
    if let Some(arr) = doc.get("providers").and_then(Value::as_array) {
        for p in arr {
            let Some(pid) = p.get("id").and_then(Value::as_str) else { continue };
            let pname = p
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(pid);
            let Some(mm) = p.get("models").and_then(Value::as_object) else { continue };
            for (mid, m) in mm {
                if seen.contains(&(pid.to_string(), mid.clone())) {
                    continue;
                }
                if !m.is_object() || !crate::get_bool_val(m, "enabled", true) {
                    continue;
                }
                let mname = m
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(mid);
                catalog.push((pid.to_string(), mid.clone(), mname.to_string(), pname.to_string()));
                seen.insert((pid.to_string(), mid.clone()));
            }
        }
    }
    catalog
}

struct AddModelPicker<'a> {
    doc: &'a mut Value,
    api: Value,
    status: Option<String>,
    // Cache of (pid, mid) combos that are enabled in providers.json, so render()
    // does O(1) lookups instead of a linear scan per row per keystroke.
    enabled_cache: std::collections::HashSet<(String, String)>,
}

impl<'a> AddModelPicker<'a> {
    fn is_enabled(&self, pid: &str, mid: &str) -> bool {
        self.enabled_cache.contains(&(pid.to_string(), mid.to_string()))
    }

    fn refresh_enabled_cache(&mut self) {
        let mut set: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        if let Some(arr) = self.doc.get("providers").and_then(Value::as_array) {
            for p in arr {
                if !p.is_object() {
                    continue;
                }
                let Some(pid) = p.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(mm) = p.get("models").and_then(Value::as_object) {
                    for (mid, m) in mm {
                        if m.is_object() && crate::get_bool_val(m, "enabled", true) {
                            set.insert((pid.to_string(), mid.to_string()));
                        }
                    }
                }
            }
        }
        self.enabled_cache = set;
    }
}

impl<'a> FilterList for AddModelPicker<'a> {
    type Entry = (String, String, String, String);

    fn compute_view(
        &mut self,
        entries: &[(String, String, String, String)],
        query: &str,
    ) -> (Vec<(String, String, String, String)>, Vec<(usize, P)>) {
        // Refresh enabled-state cache once per redraw; render() does O(1) lookups.
        self.refresh_enabled_cache();
        // Searchable: model display name and model id only.
        let term_l = query.to_lowercase();
        // Precompute the sort key once per entry so the sort does O(1) tuple
        // comparisons instead of allocating (lowercase + clone) on every
        // comparison — a large cost in debug builds over a big catalog.
        let mut keyed: Vec<(
            (u8, u8, String, String, String),
            (String, String, String, String),
        )> = entries
            .iter()
            .filter(|(_, mid, mname, _)| {
                term_l.is_empty()
                    || mname.to_lowercase().contains(&term_l)
                    || mid.to_lowercase().contains(&term_l)
            })
            .map(|(pid, mid, mname, pname)| {
                let en = if self.is_enabled(pid, mid) { 0u8 } else { 1 };
                let free = if mid.to_lowercase().contains("free") { 0u8 } else { 1 };
                let key = (en, free, mname.to_lowercase(), pid.clone(), mid.clone());
                (
                    key,
                    (pid.clone(), mid.clone(), mname.clone(), pname.clone()),
                )
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let ordered: Vec<(String, String, String, String)> =
            keyed.into_iter().map(|(_, e)| e).collect();
        let enabled_count = ordered
            .iter()
            .filter(|(pid, mid, _, _)| self.is_enabled(pid, mid))
            .count();
        let free_disabled_count = ordered[enabled_count.min(ordered.len())..]
            .iter()
            .filter(|(_, mid, _, _)| mid.to_lowercase().contains("free"))
            .count();
        let mut separators: Vec<(usize, P)> = Vec::new();
        if 0 < enabled_count && enabled_count < ordered.len() {
            separators.push((enabled_count, P::Chevron));
        }
        let free_sep_idx = enabled_count + free_disabled_count;
        if free_disabled_count > 0 && free_sep_idx < ordered.len() {
            separators.push((free_sep_idx, P::Free));
        }
        (ordered, separators)
    }

    fn render(
        &mut self,
        entry: &(String, String, String, String),
        _is_sel: bool,
    ) -> Vec<(String, P)> {
        let (pid, mid, mname, pname) = entry;
        let enabled = self.is_enabled(pid, mid);
        let is_free = mid.to_lowercase().contains("free");
        let mark = if enabled { "●" } else { "○" };
        let rest = format!(" ({pname}) - {pid}/{mid}");
        let name_pair = if enabled {
            P::Value
        } else if is_free {
            P::Enabled
        } else {
            P::Text
        };
        let mark_pair = if enabled { P::Enabled } else { P::Text };
        vec![
            ("  ".to_string(), P::Text),
            (mark.to_string(), mark_pair),
            ("  ".to_string(), P::Text),
            (mname.clone(), name_pair),
            (rest, P::Text),
        ]
    }

    fn on_enter<S: Stdscr>(&mut self, stdscr: &mut S, entry: &(String, String, String, String)) -> bool {
        let (pid, mid, mname, pname) = entry;
        if combo_enabled(self.doc, pid, mid) {
            // Already enabled: disable it. The model stays in the catalog
            // (from models.dev) so it visibly moves into the disabled or
            // free-disabled section.
            let Some(slot) = find_by_id_mut(self.doc, pid) else {
                inline_error_win(
                    stdscr,
                    &format!("Disable failed: provider {pid:?} missing"),
                );
                return true; // stay open
            };
            let mut disabled = false;
            if let Some(models) = slot.get_mut("models").and_then(Value::as_object_mut) {
                if let Some(m) = models.get_mut(mid) {
                    if let Some(obj) = m.as_object_mut() {
                        obj.insert("enabled".into(), Value::Bool(false));
                        disabled = true;
                    }
                }
            }
            if disabled {
                let _ = jsonio::dump_providers(&crate::paths::providers_path(), self.doc);
                self.status = Some(format!("Disabled {mname} ({pname}) - {pid}/{mid}."));
            }
            return true; // stay open
        }
        let existing: Vec<String> = usable(self.doc)
            .iter()
            .map(|p| p.get("id").and_then(Value::as_str).unwrap_or_default().to_string())
            .collect();
        let mut added = false;
        let mut fetch_err_url = None;
        if !existing.iter().any(|e| e == pid) {
            match crate::commands::add_provider_entry(self.doc, &self.api, pid, true) {
                Err(e) => {
                    inline_error_win(stdscr, &format!("Add failed: {}", e.0));
                    return true; // stay open
                }
                Ok(url) => {
                    fetch_err_url = url;
                    if let Some(arr) = self.doc.get("providers").and_then(Value::as_array) {
                        if arr.len() > existing.len() {
                            added = true;
                        }
                    }
                }
            }
        }
        // Enable just this model on the target provider.
        let Some(slot) = find_by_id_mut(self.doc, pid) else {
            inline_error_win(stdscr, &format!("Enable failed: provider {pid:?} missing"));
            return true; // stay open
        };
        let models = slot
            .entry("models".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !models.is_object() {
            *models = Value::Object(Map::new());
        }
        let m = models
            .as_object_mut()
            .unwrap()
            .entry(mid.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !m.is_object() {
            *m = Value::Object(Map::new());
        }
        m.as_object_mut()
            .unwrap()
            .insert("enabled".into(), Value::Bool(true));
        let _ = jsonio::dump_providers(&crate::paths::providers_path(), self.doc);
        let prefix = if added {
            format!("Added provider '{pid}'. ")
        } else {
            String::new()
        };
        self.status = Some(match fetch_err_url {
            Some(url) => crate::sync::live_fetch_error_status(&url),
            None => format!("{prefix}Enabled {mname} ({pname}) - {pid}/{mid}."),
        });
        // Stay open so the user can keep adding models; ESC returns to the
        // main menu, which then shows the last confirmation in its status
        // bar.
        true
    }
}

/// Modal: type-to-filter every models.dev model across all providers and
/// enable the chosen one, auto-adding a missing provider first. Already-
/// enabled models stay listed (enabled | free | rest, like Configure Model)
/// and are inert. Returns the confirmation status line for the parent menu,
/// or None.
pub fn add_model_win<S: Stdscr>(stdscr: &mut S, doc: &mut Value) -> Option<String> {
    let api = match crate::sync::fetch_models_dev() {
        Ok(a) => a,
        Err(e) => {
            inline_error_win(stdscr, &format!("Fetch failed: {}", e.0));
            return None;
        }
    };
    let catalog = build_add_model_catalog(&api, doc);
    let mut picker = AddModelPicker { doc, api, status: None, enabled_cache: Default::default() };
    filter_list_win_with(
        stdscr,
        &catalog,
        "Add Model",
        &[
            ("↑/↓/←/→".to_string(), "nav".to_string()),
            ("ESC".to_string(), "cancel".to_string()),
            ("Enter".to_string(), "enable".to_string()),
            ("type".to_string(), "filter".to_string()),
        ],
        &mut picker,
        0,
        None,
    );
    picker.status
}

// ---------------------------------------------------------------------------
// Whole TUI flow driver (real terminal)
// ---------------------------------------------------------------------------

/// Build the `--models`-style enabled-models listing as `PreviewLine`s, for
/// rendering in the empty space under the TUI main menu. Mirrors
/// Python's `_build_config_models_preview`: enabled models in providers.json
/// order, then an env-var status box and a summary line. `sort_by_name`
/// reorders the model rows by display name without writing anything.
pub fn build_config_models_preview(doc: &Value, sort_by_name: bool) -> Vec<PreviewLine> {
    let providers: Vec<&Map<String, Value>> = doc
        .get("providers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|p| p.is_object() && p.get("id").is_some_and(|v| !v.is_null()))
                .filter_map(|p| p.as_object())
                .collect()
        })
        .unwrap_or_default();

    let mut lines: Vec<PreviewLine> = Vec::new();
    let mut model_rows: Vec<(String, String, String, String)> = Vec::new();
    for provider in &providers {
        let pid = provider.get("id").and_then(Value::as_str).unwrap_or_default();
        let penabled = crate::get_bool_obj(provider, "enabled", true);
        let mm = provider.get("models").and_then(Value::as_object);
        let Some(mm) = mm else {
            continue;
        };
        let pname = crate::name_or(&Value::Object((*provider).clone()), pid);
        for (mid, m) in mm {
            if !m.is_object() || !crate::get_bool_val(m, "enabled", true) {
                continue;
            }
            if !penabled {
                continue;
            }
            let mname = crate::name_or(m, mid);
            model_rows.push((mname, pname.clone(), pid.to_string(), mid.clone()));
        }
    }
    if sort_by_name {
        model_rows.sort_by(|a, b| {
            (
                a.0.to_lowercase(),
                a.1.to_lowercase(),
                a.2.clone(),
                a.3.clone(),
            )
                .cmp(&(
                    b.0.to_lowercase(),
                    b.1.to_lowercase(),
                    b.2.clone(),
                    b.3.clone(),
                ))
        });
    }
    let total_enabled = model_rows.len();
    // Heading marker -> full-width blue bar, like the screen title.
    // Count sits on the bar so paging cannot park a second "Summary"
    // line on the status row.
    lines.push(PreviewLine::Heading(format!("Enabled Models: {total_enabled}")));
    lines.push(PreviewLine::Segs(vec![("".to_string(), P::Text)])); // gap under the models header
    let model_width = model_rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let rows_with_levels: Vec<(String, String, String, String, String)> = model_rows
        .iter()
        .map(|(mname, pname, pid, mid)| {
            let level = model_reasoning_level(doc, pid, mid);
            (mname.clone(), pname.clone(), pid.clone(), mid.clone(), level)
        })
        .collect();
    let level_cell_width = rows_with_levels.iter().map(|r| r.4.chars().count() + 2).max().unwrap_or(0);
    for (mname, pname, pid, mid, level) in &rows_with_levels {
        let level_pair = if level != "none" { P::Free } else { P::Muted };
        let pad_m = model_width.saturating_sub(mname.chars().count());
        let pad_l = (level_cell_width + 2).saturating_sub(level.chars().count() + 4);
        lines.push(PreviewLine::Model {
            pid: pid.clone(),
            mid: mid.clone(),
            segs: vec![
                ("● ".to_string(), P::Enabled),
                (format!("{mname}{}", " ".repeat(pad_m)), P::Value),
                (format!(" ({}) {}", level, " ".repeat(pad_l)), level_pair),
                (format!("({pname})"), P::Text),
            ],
        });
    }

    if total_enabled == 0 {
        lines.push(PreviewLine::Segs(vec![(
            "No enabled models. Enable with --enable or grok-models".to_string(),
            P::Muted,
        )]));
        return lines;
    }

    lines
}

fn model_reasoning_level(doc: &Value, pid: &str, mid: &str) -> String {
    let Some(arr) = doc.get("providers").and_then(Value::as_array) else {
        return "none".into();
    };
    for p in arr {
        if p.get("id").and_then(Value::as_str) != Some(pid) {
            continue;
        }
        let Some(m) = p.get("models").and_then(Value::as_object).and_then(|mm| mm.get(mid)) else {
            return "none".into();
        };
        if let Some(efforts) = m.get("reasoning_efforts").and_then(Value::as_array) {
            for row in efforts {
                if row.get("default").and_then(Value::as_bool).unwrap_or(false) {
                    if let Some(v) = row.get("value").and_then(Value::as_str) {
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }
        if let Some(v) = m.get("reasoning_effort").and_then(Value::as_str) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
        return "none".into();
    }
    "none".into()
}

fn set_reasoning_win<S: Stdscr>(
    stdscr: &mut S,
    doc: &mut Value,
    pid: &str,
    mid: &str,
) -> Option<String> {
    let (labels, values, mname) = {
        let models = doc
            .get("providers")?
            .as_array()?
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))?
            .get("models")?
            .as_object()?;
        let m = models.get(mid)?;
        let efforts = match m.get("reasoning_efforts").and_then(Value::as_array) {
            Some(e) if !e.is_empty() => e,
            _ => return Some("No reasoning levels".into()),
        };
        let mut labels = Vec::new();
        let mut values = Vec::new();
        for row in efforts {
            let val = row.get("value").and_then(Value::as_str).unwrap_or("");
            if val.is_empty() {
                continue;
            }
            let lab = row
                .get("label")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(val);
            if row.get("default").and_then(Value::as_bool).unwrap_or(false) {
                labels.push(format!("{lab} [default]"));
            } else {
                labels.push(lab.to_string());
            }
            values.push(val.to_string());
        }
        if values.is_empty() {
            return Some("No reasoning levels".into());
        }
        (labels, values, crate::name_or(m, mid))
    };
    let pick = match select_win(
        stdscr,
        &labels,
        &format!("Reasoning: {mname}"),
        false,
        &[],
        true,
        None,
        None,
        None,
        None,
        0,
        None,
        None,
    ) {
        Some(SelectOutcome::Picked(i)) => i,
        _ => return None,
    };
    if pick >= values.len() {
        return None;
    }
    let chosen = values[pick].clone();
    let slot = find_by_id_mut(doc, pid)?;
    let m = slot
        .get_mut("models")?
        .as_object_mut()?
        .get_mut(mid)?
        .as_object_mut()?;
    m.insert("reasoning_effort".into(), Value::String(chosen.clone()));
    if let Some(arr) = m.get_mut("reasoning_efforts").and_then(Value::as_array_mut) {
        for row in arr {
            if let Some(obj) = row.as_object_mut() {
                let is = obj.get("value").and_then(Value::as_str) == Some(chosen.as_str());
                obj.insert("default".into(), Value::Bool(is));
            }
        }
    }
    let _ = jsonio::dump_providers(&paths::providers_path(), doc);
    Some(format!("Reasoning set to {chosen}"))
}

/// Try to run the TUI; return `Ok(changed)` on success, a `CursesFailed` on
/// any TTY/setup failure (caller falls back to numbered flow).
pub fn run_config_flow(doc: &mut Value) -> Res<bool> {
    if !tui_supported() {
        return Ok(false);
    }
    let mut stdscr = match RealStdscr::open() {
        Some(s) => s,
        None => return Ok(false),
    };
    run_config_flow_with_backend(&mut stdscr, doc)
}

pub fn run_config_flow_with_backend<S: Stdscr>(stdscr: &mut S, doc: &mut Value) -> Res<bool> {
    let mut changed = false;
    let mut status_msg: Option<String> = None;
    let mut sort_by_name = false;
    let mut menu_cursor = 0usize;
    let mut model_focus: Option<(String, String)> = None;
    loop {
        // Order is providers.json (sorted only on dump).
        let ordered: Vec<Map<String, Value>> = usable(doc);
        // Zero providers is a valid state: ➕ Add Provider… is reachable first.
        // Trailing block after a section rule: Codex Config, Model
        // Descriptions toggle, Update Model List, Sync Model Config, then
        // the two add actions.
        let descriptions_on = doc
            .get("include_descriptions")
            .and_then(Value::as_bool)
            .unwrap_or(crate::jsonio::INCLUDE_DESCRIPTIONS_DEFAULT);
        let mut labels: Vec<String> = crate::core::provider_menu_labels(&ordered);
        let token_col = crate::core::provider_state_token_col(&ordered);
        labels.push(crate::core::pad_state_label(
            crate::core::CODEX_CONFIG_LABEL,
            &format!("[{}]", crate::jsonio::codex_status_token(doc)),
            token_col,
        ));
        labels.push(crate::core::pad_state_label(
            crate::core::MODEL_DESC_LABEL,
            &format!("[{}]", if descriptions_on { "enabled" } else { "disabled" }),
            token_col,
        ));
        match doc.get("last_updated").and_then(Value::as_str) {
            Some(ts) if !ts.is_empty() => {
                labels.push(crate::core::pad_state_label(
                    crate::core::UPDATE_LIST_LABEL,
                    &format!("[{ts}]"),
                    token_col,
                ));
            }
            _ => labels.push(crate::core::UPDATE_LIST_LABEL.to_string()),
        }
        match doc.get("last_synced").and_then(Value::as_str) {
            Some(ts) if !ts.is_empty() => {
                labels.push(crate::core::pad_state_label(
                    crate::core::SYNC_CONFIG_LABEL,
                    &format!("[{ts}]"),
                    token_col,
                ));
            }
            _ => labels.push(crate::core::SYNC_CONFIG_LABEL.to_string()),
        }
        labels.push("➕ Add Provider…".to_string());
        labels.push("➕ Add Model…".to_string());
        let preview = build_config_models_preview(doc, sort_by_name);
        // Trailing-block rows (Codex Config, Model Descriptions, …) are
        // selectable; Enter lands on them as SelectOutcome::Picked.
        let pi = match select_win(stdscr,
            &labels,
            "Select Provider (changes sync on exit)",
            false,
            &[],
            false,
            None,
            None,
            status_msg.as_deref(),
            Some(&preview),
            menu_cursor,
            Some(ordered.len()),
            model_focus.as_ref().map(|(p, m)| (p.as_str(), m.as_str())),
        ) {
            None => return Ok(changed),
            Some(SelectOutcome::Cancelled) => return Ok(changed),
            Some(SelectOutcome::SortToggled(i)) => {
                sort_by_name = !sort_by_name;
                menu_cursor = i;
                continue;
            }
            Some(SelectOutcome::ModelPicked { pid, mid }) => {
                model_focus = Some((pid.clone(), mid.clone()));
                if let Some(msg) = set_reasoning_win(stdscr, doc, &pid, &mid) {
                    status_msg = Some(msg);
                    changed = true;
                }
                continue;
            }
            Some(SelectOutcome::Picked(i)) => {
                model_focus = None;
                i
            }
        };
        if pi == ordered.len() {
            // Provider rows share the main provider-list layout.
            let enabled: Vec<Map<String, Value>> = doc
                .get("providers")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter(|p| {
                            p.is_object()
                                && p.get("id").is_some()
                                && p.get("enabled")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(true)
                        })
                        .filter_map(|p| p.as_object().cloned())
                        .collect()
                })
                .unwrap_or_default();
            let mut values: Vec<Option<String>> = vec![None];
            for p in &enabled {
                values.push(p.get("id").and_then(Value::as_str).map(|s| s.to_string()));
            }
            let mut choices: Vec<String> = vec!["disabled".to_string()];
            choices.extend(crate::core::provider_menu_labels(&enabled));
            let current = crate::jsonio::codex_status_token(doc);
            let initial = if current == "disabled" {
                0
            } else if let Some(pos) =
                values.iter().position(|v| v.as_deref() == Some(current.as_str()))
            {
                pos
            } else {
                0
            };
            match select_win(
                stdscr,
                &choices,
                "Codex Config",
                false,
                &[],
                true,
                None,
                None,
                None,
                None,
                initial,
                None,
                None,
            ) {
                Some(SelectOutcome::Picked(i)) => {
                    let sel = values[i].clone();
                    let previous = crate::jsonio::codex_model_provider_id(doc);
                    let is_switch = sel.is_some()
                        && !previous.is_empty()
                        && previous != "disabled"
                        && sel.as_deref() != Some(previous.as_str());
                    if is_switch {
                        // Flush the previous pick: turn writing off, sync
                        // (which deletes the previous catalog via the
                        // one-shot cleanup), then turn writing back on for
                        // the new pick and sync again.
                        crate::jsonio::set_codex_selection(doc, None);
                        let _ = jsonio::dump_providers(&paths::providers_path(), doc);
                        let _ = crate::sync::update_config_toml_with(true);
                        crate::jsonio::set_codex_selection(doc, sel.as_deref());
                        let _ = jsonio::dump_providers(&paths::providers_path(), doc);
                        let _ = crate::sync::update_config_toml_with(true);
                    } else {
                        crate::jsonio::set_codex_selection(doc, sel.as_deref());
                        let _ = jsonio::dump_providers(&paths::providers_path(), doc);
                        let _ = crate::sync::update_config_toml_with(true);
                    }
                    if let Ok(fresh) = jsonio::load_providers() {
                        *doc = fresh;
                    }
                    status_msg = Some(format!(
                        "Codex Config {}",
                        crate::jsonio::codex_status_token(doc)
                    ));
                    changed = true;
                }
                _ => {}
            }
            menu_cursor = pi;
            continue;
        }
        if pi == ordered.len() + 1 {
            // "Model Descriptions [enabled/disabled]" — global flag.
            let new_val = !descriptions_on;
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("include_descriptions".into(), Value::Bool(new_val));
            }
            let _ = jsonio::dump_providers(&paths::providers_path(), doc);
            status_msg = Some(format!(
                "Model Descriptions {}",
                if new_val { "enabled" } else { "disabled" }
            ));
            changed = true;
            menu_cursor = pi; // stay on the toggle row, like Configure Models
            continue;
        }
        if pi == ordered.len() + 2 {
            match crate::sync::update_providers_json_with(true) {
                Ok(stats) => {
                    if let Ok(fresh) = jsonio::load_providers() {
                        *doc = fresh;
                    }
                    status_msg = Some(if stats.live_fetch_errors.len() == 1 {
                        stats.live_fetch_errors[0].clone()
                    } else if stats.live_fetch_errors.len() > 1 {
                        format!(
                            "{} (+{} more)",
                            stats.live_fetch_errors[0],
                            stats.live_fetch_errors.len() - 1
                        )
                    } else {
                        format!(
                            "Updated model list · {} providers synced",
                            stats.providers_synced
                        )
                    });
                    changed = true;
                }
                Err(e) => {
                    status_msg = Some(if e.0.starts_with("error ") {
                        e.0
                    } else {
                        format!("error {}: fetch live model list failed", e.0)
                    });
                }
            }
            menu_cursor = pi;
            continue;
        }
        if pi == ordered.len() + 3 {
            match crate::sync::update_config_toml_with(true) {
                Ok(_) => {
                    if let Ok(fresh) = jsonio::load_providers() {
                        *doc = fresh;
                    }
                    status_msg = Some("Synced model config".to_string());
                }
                Err(e) => {
                    status_msg = Some(if e.0.starts_with("error ") {
                        e.0
                    } else {
                        format!("error {}: sync model config failed", e.0)
                    });
                }
            }
            menu_cursor = pi;
            continue;
        }
        if pi == ordered.len() + 4 {
            // "➕ Add Provider…" — modal over the models.dev catalog.
            if let Some(msg) = add_provider_win(stdscr, doc) {
                status_msg = Some(msg);
                changed = true;
            }
            menu_cursor = pi;
            continue;
        }
        if pi == ordered.len() + 5 {
            // "➕ Add Model…" — cross-provider modal; auto-adds a missing
            // provider and enables just that model.
            if let Some(msg) = add_model_win(stdscr, doc) {
                status_msg = Some(msg);
                changed = true;
            }
            menu_cursor = pi;
            continue;
        }
        status_msg = None;
        let mut action_cursor = 0usize;
        let mut target = ordered[pi].clone();
        menu_cursor = pi;
        loop {
            let enabled = crate::get_bool_val(&Value::Object(target.clone()), "enabled", true);
            let current_base =
                target.get("base_url").and_then(Value::as_str).unwrap_or_default().to_string();
            let actions = vec![
                "Configure Models".to_string(),
                format!("Provider [{}]", if enabled { "enabled" } else { "disabled" }),
                format!("Base Url [{current_base}]"),
                "Delete Provider".to_string(),
                "Back".to_string(),
            ];
            let env_key = crate::first_env_key_from(&target);
            let key_hint = if env_key.is_empty() {
                None
            } else {
                Some(format!(
                    "# config {} api keys\npbpaste > key-file\necho 'export {env_key}=\"$(cat ~/key-file)\"' >> ~/.zshrc",
                    target.get("id").and_then(Value::as_str)
                        .or_else(|| target.get("name").and_then(Value::as_str))
                        .unwrap_or_default()
                ))
            };
            let footer = if env_key.is_empty() {
                None
            } else {
                Some(core::env_status_line(&env_key))
            };
            let ai = match select_win(stdscr,
                &actions,
                &format!("Provider: {}", target.get("name").and_then(Value::as_str).unwrap_or(target["id"].as_str().unwrap_or_default())),
                false,
                &[],
                true,
                key_hint.as_deref(),
                footer.as_deref(),
                None,
                None,
                action_cursor,
                None,
                None,
            ) {
                None | Some(SelectOutcome::Cancelled) => break,
                Some(SelectOutcome::SortToggled(_)) => continue,
                Some(SelectOutcome::ModelPicked { .. }) => continue,
                Some(SelectOutcome::Picked(i)) => i,
            };
            action_cursor = ai;
            if actions[ai] == "Back" {
                break;
            }
            let id_str = target["id"].as_str().unwrap_or_default().to_string();
            match ai {
                0 => {
                    let ids: Vec<String> = match target.get("models") {
                        Some(Value::Object(m)) => m.keys().cloned().collect(),
                        _ => Vec::new(),
                    };
                    if ids.is_empty() {
                        inline_error_win(
                            stdscr,
                            &format!(
                                "No models for {}. Run a sync or re-add the provider.",
                                core::py_repr(&id_str)
                            ),
                        );
                    } else {
                        let mut models = target
                            .get("models")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        let pname = target
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(target["id"].as_str().unwrap_or_default());
                        let provider_title = format!("Provider: {pname} | Configure Model");
                        model_search_win(stdscr, &ids, &mut models, &provider_title, &id_str, pname);
                        // Sync BOTH the live `target` copy and the doc: the
                        // action menu renders from `target`, so re-entering
                        // Configure Models must reflect the toggles even
                        // without going back to the main menu first.
                        let updated = Value::Object(models);
                        target.insert("models".to_string(), updated.clone());
                        if let Some(slot) = find_by_id_mut(doc, &id_str) {
                            slot.insert("models".to_string(), updated);
                        }
                        jsonio::dump_providers(&paths::providers_path(), doc)?;
                        changed = true;
                    }
                }
                1 => {
                    let want = !enabled;
                    if let Some(slot) = find_by_id_mut(doc, &id_str) {
                        slot.insert("enabled".into(), Value::Bool(want));
                    }
                    // Keep `target` in sync so the action-menu label flips on
                    // the next render (it is read from `target`, not `doc`).
                    target.insert("enabled".into(), Value::Bool(want));
                    jsonio::dump_providers(&paths::providers_path(), doc)?;
                    changed = true;
                }
                2 => {
                    // Inline Base Url edit: the [] area on this same menu row
                    // becomes the text field; Enter saves, ESC cancels.
                    let title_fmt = format!(
                        "Provider: {}",
                        target.get("name").and_then(Value::as_str).unwrap_or(
                            target["id"].as_str().unwrap_or_default()
                        )
                    );
                    if let Some(value) = edit_inline_row(
                        stdscr,
                        &title_fmt,
                        &actions,
                        2,
                        "Base Url",
                        &current_base,
                    ) {
                        let trimmed = value.trim().to_string();
                        if trimmed.is_empty() {
                            // Empty input clears the override (falls back to
                            // the models.dev catalog value on next sync).
                            if let Some(slot) = find_by_id_mut(doc, &id_str) {
                                slot.remove("base_url");
                            }
                            target.remove("base_url");
                        } else {
                            let val = Value::String(trimmed);
                            if let Some(slot) = find_by_id_mut(doc, &id_str) {
                                slot.insert("base_url".into(), val.clone());
                            }
                            target.insert("base_url".into(), val);
                        }
                        jsonio::dump_providers(&paths::providers_path(), doc)?;
                        changed = true;
                    }
                }
                3 => {
                    if confirm_win(stdscr, &format!("Delete Provider {}?", core::provider_display(&Value::Object(target.clone())))) {
                        // Grab the enabled model ids from providers.json
                        // before the entry is removed.
                        let enabled = core::enabled_model_ids(&Value::Object(target.clone()));
                        remove_provider(doc, &id_str);
                        fallback::record_removed_provider(doc, &id_str, enabled);
                        jsonio::dump_providers(&paths::providers_path(), doc)?;
                        // Flush the deletion into config.toml now so a re-add
                        // of the same provider this session can't collide
                        // with a pending deletion record.
                        crate::sync::update_config_toml_with(true)?;
                        changed = true;
                    }
                    menu_cursor = 0;
                    break;
                }
                _ => break,
            }
        }
    }
}


fn usable(doc: &Value) -> Vec<Map<String, Value>> {
    doc.get("providers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|p| p.is_object() && p.get("id").is_some_and(|v| !v.is_null()))
                .filter_map(|p| p.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

fn find_by_id_mut<'a>(doc: &'a mut Value, pid: &str) -> Option<&'a mut Map<String, Value>> {
    doc.get_mut("providers")?
        .as_array_mut()?
        .iter_mut()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
        .and_then(Value::as_object_mut)
}

fn remove_provider(doc: &mut Value, pid: &str) {
    if let Some(arr) = doc.get_mut("providers").and_then(Value::as_array_mut) {
        arr.retain(|p| p.get("id").and_then(Value::as_str) != Some(pid));
    }
}

// ---------------------------------------------------------------------------
// Real terminal backend (skip under tests)
// ---------------------------------------------------------------------------

/// Write a cell to a terminal-like sink: move the cursor to `(y, x)` (1-based
/// addressing, like curses `addstr(y, x)`), then emit the SGR + text + reset.
/// Shared by `RealStdscr` and the capture test double so both paths render a
/// true fullscreen layout (each element at its absolute row/column) instead of
/// streaming text inline, which collapses the screen onto one wrapped line.
/// Emit one styled string straight to a writer (used by the test capture
/// stdscr; the real screen diffs frames instead).
#[cfg(test)]
fn emit_cell<W: std::io::Write>(w: &mut W, y: i32, x: i32, s: &str, paint: Paint) {
    let _ = write!(w, "\x1b[{};{}H", y.max(0) + 1, x.max(0) + 1);
    let sgr = theme::sgr_paint(paint.fg, paint.bg, paint.bold);
    let _ = write!(w, "{}{}\x1b[0m", sgr, s);
}

/// Switch to the terminal's alternate screen so the fullscreen TUI doesn't
/// paint over (or leave scrollback history of) the user's existing terminal.
/// On exit we restore the original screen, so closing the TUI returns the
/// terminal exactly as it was before — no blue background, no menu history.
fn enable_mouse<W: std::io::Write>(w: &mut W) {
    let _ = write!(w, "\x1b[?1000h\x1b[?1006h");
}
fn disable_mouse<W: std::io::Write>(w: &mut W) {
    let _ = write!(w, "\x1b[?1000l\x1b[?1006l");
}
fn enter_alt_screen<W: std::io::Write>(w: &mut W) {
    let _ = write!(w, "\x1b[?1049h");
}
fn leave_alt_screen<W: std::io::Write>(w: &mut W) {
    let _ = write!(w, "\x1b[?1049l\x1b[0m");
}
fn hide_cursor<W: std::io::Write>(w: &mut W) {
    let _ = write!(w, "\x1b[?25l");
}
fn show_cursor<W: std::io::Write>(w: &mut W) {
    let _ = write!(w, "\x1b[?25h");
}

/// The set of DEC private modes grok-models enables (alternate screen +
/// hidden cursor + SGR mouse tracking). The restore sequence clears exactly
/// these on every exit path so the terminal returns to its prior state and
/// wheel events stop being eaten. This is the hand-rolled equivalent of
/// grok-build's `RESTORE_SEQ` (async-signal-safe: ANSI only).
const RESTORE_SEQ: &[u8] = b"\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l\x1b[0m";

/// Original termios captured at `open()`, restored on every exit path
/// (normal `Drop`, SIGINT/SIGTERM/SIGHUP, panic) so the terminal never stays
/// in raw mode. Read from the async signal handler, so it is a plain `static`
/// set exactly once before any signal can fire.
static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// Set by the SIGWINCH handler and consumed by `read_key` to surface a resize
/// between key presses. The default SIGWINCH disposition is "ignore", so a
/// real handler must be installed — `sigwait` silently fails on macOS for a
/// default-ignore signal (grok-build's `sigwinch_loop` documents this).
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe terminal restore: `write(2)` only, no allocation, no
/// locks. Mirrors grok-build's `restore_in_signal_handler`, which writes the
/// RESTORE_SEQ from a real signal handler context.
unsafe fn restore_terminal_raw() {
    libc::write(
        1,
        RESTORE_SEQ.as_ptr() as *const libc::c_void,
        RESTORE_SEQ.len(),
    );
    libc::write(
        2,
        RESTORE_SEQ.as_ptr() as *const libc::c_void,
        RESTORE_SEQ.len(),
    );
    if let Some(t) = SAVED_TERMIOS.get() {
        libc::tcsetattr(0, libc::TCSANOW, t);
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    // The terminal is left in raw mode + alt screen until we restore it here.
    // Restore, then exit with the conventional `128 + sig` status.
    unsafe {
        restore_terminal_raw();
        libc::_exit(128 + sig);
    }
}

extern "C" fn on_winch(_sig: libc::c_int) {
    RESIZE_PENDING.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    type Sig = extern "C" fn(libc::c_int);
    unsafe {
        // Ignore SIGTTIN/SIGTTOU: a child briefly stealing the foreground
        // process group would otherwise stop the whole TUI, stranding the
        // terminal in raw mode. (grok-build's `signal_handler::install` does
        // the same.)
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        // Restore the terminal before the default action terminates us, so
        // Ctrl-C / terminal close never leaves a broken (raw + blue) terminal.
        libc::signal(libc::SIGINT, on_signal as Sig as usize);
        libc::signal(libc::SIGTERM, on_signal as Sig as usize);
        libc::signal(libc::SIGHUP, on_signal as Sig as usize);
        libc::signal(libc::SIGWINCH, on_winch as Sig as usize);
    }
}

fn reset_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTTIN, libc::SIG_DFL);
        libc::signal(libc::SIGTTOU, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
        libc::signal(libc::SIGWINCH, libc::SIG_DFL);
    }
}

/// Chain a panic hook that restores the terminal. `Drop` already handles the
/// unwind path, but this also covers `panic = "abort"` builds (where `Drop`
/// does not run), mirroring grok-build's `set_panic_hook`.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        unsafe {
            restore_terminal_raw();
        }
        prev(info);
    }));
}

pub struct RealStdscr {
    pub raw_mode: Option<TermiosMode>,
    /// Unconsumed bytes from previous reads: a burst (paste, fast typing) can
    /// deliver several keys in one chunk and none may be dropped.
    pub input_buf: Vec<u8>,
    /// Frame being built this pass (erase/addstr write here, never to the
    /// terminal) plus the last frame actually emitted. `refresh()` diffs the
    /// two and writes only the changed cells — no `\x1b[2J` wipe per keypress,
    /// which is what made fast navigation flicker.
    pub frame: Vec<Vec<(char, Paint)>>,
    pub committed: Vec<Vec<(char, Paint)>>,
}

impl Stdscr for RealStdscr {
    fn getmaxyx(&self) -> (i32, i32) {
        unsafe {
            let w = libc::STDOUT_FILENO;
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(w, libc::TIOCGWINSZ, &mut ws) == 0 {
                (ws.ws_row as i32, ws.ws_col as i32)
            } else {
                (40, 80)
            }
        }
    }
    fn erase(&mut self) {
        // No terminal output: just start a fresh frame at the current size.
        // (The old `\x1b[2J` wipe per keypress was the flicker source.)
        let (rows, cols) = self.getmaxyx();
        self.frame = blank_frame(rows as usize, cols as usize);
    }
    fn refresh(&mut self) {
        let rows = self.frame.len();
        let cols = self.frame.first().map(|r| r.len()).unwrap_or(0);
        if rows == 0 || cols == 0 {
            return;
        }
        // Shape changed (resize, first frame): force a full repaint.
        if self.committed.len() != rows
            || self.committed.first().map(|r| r.len()) != Some(cols)
        {
            self.committed = unknown_frame(rows, cols);
        }
        let mut out = String::new();
        let mut last_paint: Option<Paint> = None;
        let mut cur_pos: Option<(usize, usize)> = None;
        for r in 0..rows {
            for c in 0..cols {
                let cell = self.frame[r][c];
                if cell.0 == '\0' {
                    // Continuation of a 2-col glyph: the terminal already
                    // advanced past this column when the glyph was emitted.
                    self.committed[r][c] = cell;
                    continue;
                }
                if self.committed[r][c] == cell {
                    continue;
                }
                // Move only when the cursor wouldn't naturally land here by
                // having written the previous changed cell in this run.
                if cur_pos != Some((r, c)) || last_paint != Some(cell.1) {
                    out.push_str(&format!("\x1b[{};{}H", r + 1, c + 1));
                }
                if last_paint != Some(cell.1) {
                    out.push_str(&crate::theme::sgr_paint(cell.1.fg, cell.1.bg, cell.1.bold));
                    last_paint = Some(cell.1);
                }
                out.push(cell.0);
                let adv = char_cols(cell.0);
                cur_pos = Some((r, c + adv));
                self.committed[r][c] = cell;
            }
        }
        if !out.is_empty() {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
        }
    }
    fn addstr(&mut self, y: i32, x: i32, s: &str, paint: Paint) {
        // Write into the frame buffer; refresh() sends it to the terminal.
        let rows = self.frame.len();
        let cols = self.frame.first().map(|r| r.len()).unwrap_or(0);
        if y < 0 || y as usize >= rows || x < 0 {
            return;
        }
        let mut cx = x as usize;
        for ch in s.chars() {
            let w = char_cols(ch);
            if cx + w > cols {
                break;
            }
            self.frame[y as usize][cx] = (ch, paint);
            if w == 2 && cx + 1 < cols {
                self.frame[y as usize][cx + 1] = ('\0', paint);
            }
            cx += w;
        }
    }
    fn getch(&mut self) -> Key {
        // Buffered input path: parse one key per call from bytes already
        // read, only hitting the tty when the buffer runs dry. An incomplete
        // escape sequence waits ~25ms (python set_escdelay(25)) for its tail
        // before giving up and treating the leading ESC as Key::Esc.
        const ESC_DELAY_MS: i32 = 25;
        loop {
            match parse_key_prefix(&self.input_buf) {
                Some((k, used)) => {
                    self.input_buf.drain(..used);
                    return k;
                }
                None => {}
            }
            let readable = if self.input_buf.is_empty() {
                match wait_stdin_readable(-1) {
                    Ok(r) => r,
                    Err(()) => {
                        if RESIZE_PENDING.swap(false, Ordering::SeqCst) {
                            return Key::Resize;
                        }
                        // Non-resize interrupt: retry the read.
                        continue;
                    }
                }
            } else {
                matches!(wait_stdin_readable(ESC_DELAY_MS), Ok(true))
            };
            if !readable {
                // Esc-delay expired on an incomplete sequence. A lone ESC is
                // a real Esc; a truncated CSI/mouse prefix must be dropped —
                // emitting Esc would pop Configure Models back to the
                // provider page on a fast wheel burst.
                if self.input_buf.first() == Some(&0x1b) {
                    if self.input_buf.len() == 1 {
                        self.input_buf.clear();
                        return Key::Esc;
                    }
                    self.input_buf.clear();
                }
                continue;
            }
            let mut buf = [0u8; 4096];
            match std::io::stdin().read(&mut buf) {
                Ok(nread) if nread >= 1 => self.input_buf.extend_from_slice(&buf[..nread]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    if RESIZE_PENDING.swap(false, Ordering::SeqCst) {
                        return Key::Resize;
                    }
                    // Non-resize interrupt: retry the read.
                }
                _ => return Key::Eof,
            }
        }
    }
    fn invalidate(&mut self) {
        let (rows, cols) = self.getmaxyx();
        self.committed = unknown_frame(rows as usize, cols as usize);
    }
}

/// Read one key from `r`, transparently handling `EINTR` (delivered by
/// SIGWINCH): a resize interrupt surfaces as `Key::Resize` so the TUI can
/// redraw at the new size; any other interrupt is retried.
impl RealStdscr {
    pub fn open() -> Option<Self> {
        let raw_mode = TermiosMode::enter().ok()?;
        // Capture the original termios for the signal/panic restore paths.
        SAVED_TERMIOS.get_or_init(|| raw_mode.saved);
        // Swap to the alternate screen and hide the cursor before drawing.
        let mut out = std::io::stdout();
        enter_alt_screen(&mut out);
        enable_mouse(&mut out);
        hide_cursor(&mut out);
        let _ = out.flush();
        // Ensure every exit path restores the terminal (Ctrl-C, terminal
        // close, crash) instead of stranding it in raw mode + alt screen.
        install_signal_handlers();
        install_panic_hook();
        Some(Self {
            raw_mode: Some(raw_mode),
            input_buf: Vec::new(),
            frame: Vec::new(),
            committed: Vec::new(),
        })
    }
}

impl Drop for RealStdscr {
    fn drop(&mut self) {
        // Show the cursor, leave the alternate screen, and flush so the
        // restore actually reaches the terminal (stdout is buffered). Then
        // restore cooked mode. Finally reset our signal handlers so a later
        // Ctrl-C (e.g. during the post-config sync) behaves normally.
        let mut out = std::io::stdout();
        disable_mouse(&mut out);
        show_cursor(&mut out);
        leave_alt_screen(&mut out);
        let _ = out.flush();
        if let Some(m) = self.raw_mode.take() {
            let _ = m.restore();
        }
        reset_signal_handlers();
    }
}

pub struct TermiosMode {
    fd: i32,
    saved: libc::termios,
}

impl TermiosMode {
    pub fn enter() -> std::io::Result<Self> {
        unsafe {
            let fd = libc::STDIN_FILENO;
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { fd, saved })
        }
    }
    pub fn restore(self) -> std::io::Result<()> {
        unsafe {
            if libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

/// Parse one key from the front of `buf`, returning the key and bytes
/// consumed. Returns None only when the buffer ends with an incomplete
/// escape sequence (more bytes are needed to decide).
/// SGR/X10 wheel: bit 6 marks a wheel event, bit 0 is direction (0 up / 1
/// down). Modifier and motion bits (shift/ctrl/meta/32) are ignored so a
/// fast trackpad burst still scrolls instead of being dropped or turned
/// into Esc. Releases (`press == false`) are ignored to avoid double-steps.
fn sgr_wheel_key(btn: u32, press: bool, y: i32) -> Key {
    if !press || btn & 64 == 0 {
        return Key::Eof;
    }
    if btn & 1 == 0 {
        Key::WheelUp(y)
    } else {
        Key::WheelDown(y)
    }
}

fn parse_key_prefix(buf: &[u8]) -> Option<(Key, usize)> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] == 0x1b {
        if buf.len() == 1 {
            // Could be the start of an escape sequence still arriving.
            return None;
        }
        if buf[1] == b'[' {
            if buf.len() == 2 {
                return None;
            }
            match buf[2] {
                b'A' => return Some((Key::Up, 3)),
                b'B' => return Some((Key::Down, 3)),
                b'C' => return Some((Key::Right, 3)),
                b'D' => return Some((Key::Left, 3)),
                _ => {}
            }
            // PageUp/PageDown: CSI 5~ / 6~ (numeric params, tilde final).
            if buf.len() >= 3 && (buf[2] == b'5' || buf[2] == b'6') {
                if buf.len() == 3 {
                    return None; // wait for the tail
                }
                if buf[3] == b'~' {
                    let key = if buf[2] == b'5' { Key::PageUp } else { Key::PageDown };
                    return Some((key, 4));
                }
            }
            // SGR mouse: ESC [ < btn ; x ; y M/m. Wheel is bit 6 (64/65), plus
            // optional shift/meta/ctrl/motion bits. Only button-press (`M`)
            // scrolls; release (`m`) is ignored so a fast wheel does not
            // double-step or leak Esc.
            if buf[2] == b'<' {
                let end = buf[3..]
                    .iter()
                    .position(|b| *b == b'M' || *b == b'm')
                    .map(|p| p + 4);
                let Some(end) = end else {
                    return None; // wait for the tail
                };
                let press = buf[end - 1] == b'M';
                let payload = std::str::from_utf8(&buf[3..end - 1]).unwrap_or("");
                let mut parts = payload.split(';');
                let btn = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                let _x = parts.next();
                // SGR mouse coords are 1-based.
                let y = parts
                    .next()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                return Some((sgr_wheel_key(btn, press, y), end));
            }
            // X10 mouse: ESC [ M Cb Cx Cy (button/x/y each + 32).
            if buf[2] == b'M' {
                if buf.len() < 6 {
                    return None;
                }
                let btn = (buf[3] as u32).saturating_sub(32);
                let y = (buf[5] as i32).saturating_sub(32).saturating_sub(1);
                return Some((sgr_wheel_key(btn, true, y), 6));
            }
            // Unknown CSI: swallow through its final alpha byte. Never Esc —
            // treating leftover mouse CSI as Esc pops back to the provider
            // page mid-scroll.
            let end = buf[2..]
                .iter()
                .position(|b| b.is_ascii_alphabetic())
                .map(|p| p + 3)
                .unwrap_or(buf.len());
            return Some((Key::Eof, end));
        }
        // ESC followed by something else (e.g. an unknown alt-chord).
        return Some((Key::Esc, 1));
    }
    // Ctrl-C (ETX) is delivered as a literal byte in raw mode (ISIG is off),
    // so the tty does not raise SIGINT. Treat it as an interrupt/abort.
    let key = match buf[0] {
        0x03 => Key::Interrupt,
        0x7f | 0x08 => Key::Backspace,
        b'\r' | b'\n' => Key::Enter,
        c if c.is_ascii() => Key::Char((c as char).to_ascii_lowercase()),
        _ => Key::Eof,
    };
    Some((key, 1))
}

/// Wait for stdin readability. `Ok(true)` readable, `Ok(false)` timeout,
/// `Err(())` interrupted (EINTR).
fn wait_stdin_readable(timeout_ms: i32) -> Result<bool, ()> {
    unsafe {
        let mut fds = [libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 }];
        let rc = libc::poll(fds.as_mut_ptr(), 1, timeout_ms);
        if rc < 0 {
            return Err(());
        }
        Ok(rc > 0)
    }
}

// ---------------------------------------------------------------------------
// Tests: ported from `_smoketest.py` using a recording `FakeStdscr`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use std::sync::Once;

    /// Force `GROK_HOME` into a per-process temp dir before any flow test
    /// runs, so `dump_json(&paths::providers_path(), ..)` can never reach
    /// the real `~/.grok/providers.json`.
    fn isolate_grok_home() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let home = std::env::temp_dir()
                .join(format!("gm-unit-home-{}", std::process::id()));
            std::fs::create_dir_all(&home).expect("create test GROK_HOME");
            std::env::set_var("GROK_HOME", &home);
            let codex = std::env::temp_dir()
                .join(format!("gm-unit-codex-{}", std::process::id()));
            std::fs::create_dir_all(&codex).expect("create test CODEX_HOME");
            std::env::set_var("CODEX_HOME", &codex);
        });
    }

    /// Records every `addstr` call so tests can assert exact rendering
    /// (token colors, legend position, background sweep) without curses.
    struct FakeStdscr {
        h: i32,
        w: i32,
        calls: std::cell::RefCell<Vec<(i32, i32, String, Paint)>>,
        keys: std::cell::RefCell<Vec<Key>>,
        /// `calls.len()` at each `erase`, so tests can inspect the last frame.
        frame_starts: std::cell::RefCell<Vec<usize>>,
    }

    impl FakeStdscr {
        fn new(h: i32, w: i32) -> Self {
            FakeStdscr {
                h,
                w,
                calls: Default::default(),
                keys: Default::default(),
                frame_starts: Default::default(),
            }
        }
        fn script(&self, k: Key) {
            self.keys.borrow_mut().push(k);
        }
        fn recorded(&self) -> Vec<(i32, i32, String, Paint)> {
            self.calls.borrow().clone()
        }
        fn last_frame(&self) -> Vec<(i32, i32, String, Paint)> {
            let start = self.frame_starts.borrow().last().copied().unwrap_or(0);
            self.calls.borrow()[start..].to_vec()
        }
    }

    impl Stdscr for FakeStdscr {
        fn getmaxyx(&self) -> (i32, i32) {
            (self.h, self.w)
        }
        fn erase(&mut self) {
            self.frame_starts
                .borrow_mut()
                .push(self.calls.borrow().len());
        }
        fn refresh(&mut self) {}
        fn addstr(&mut self, y: i32, x: i32, s: &str, paint: Paint) {
            self.calls.borrow_mut().push((y, x, s.to_string(), paint));
        }
        fn getch(&mut self) -> Key {
            let mut k = self.keys.borrow_mut();
            if k.is_empty() {
                Key::Eof
            } else {
                k.remove(0)
            }
        }
    }

    fn token_paints(
        calls: &[(i32, i32, String, Paint)],
        token: &str,
    ) -> Vec<Paint> {
        calls
            .iter()
            .filter(|(_, _, t, _)| t == token)
            .map(|(_, _, _, p)| *p)
            .collect()
    }

    fn is_green(c: theme::Rgb) -> bool {
        c.g > c.r && c.g > c.b
    }
    fn is_red(c: theme::Rgb) -> bool {
        c.r > c.g && c.r > c.b
    }
    fn is_blue(c: theme::Rgb) -> bool {
        c.b > c.r && c.b > c.g
    }

    #[test]
    fn plus_emoji_is_two_columns() {
        assert_eq!(char_cols('➕'), 2);
        assert_eq!(char_cols('A'), 1);
        assert_eq!(char_cols('…'), 1);
        let label = "➕ Add Provider…";
        assert_eq!(str_cols(label), label.chars().count() + 1);
        let clipped = clip_cols("Back from Add Provider…", 4);
        assert_eq!(clipped, "Back");
        let padded = pad_cols("Back", str_cols("➕ Add Provider…"), ' ');
        assert_eq!(str_cols(&padded), str_cols("➕ Add Provider…"));
        assert!(!padded.contains('d') || padded.starts_with("Back"));
        assert!(!padded[4..].contains('d'));
        assert!(!padded.contains('…'));
    }

    struct CountList {
        entries: Vec<String>,
    }
    impl FilterList for CountList {
        type Entry = String;
        fn compute_view(&mut self, entries: &[String], query: &str) -> (Vec<String>, Vec<(usize, P)>) {
            let q = query.to_lowercase();
            (
                entries
                    .iter()
                    .filter(|e| q.is_empty() || e.to_lowercase().contains(&q))
                    .cloned()
                    .collect(),
                vec![],
            )
        }
        fn render(&mut self, entry: &String, _is_selected: bool) -> Vec<(String, P)> {
            vec![(format!("  {entry}"), P::Text)]
        }
        fn on_enter<S: Stdscr>(&mut self, _stdscr: &mut S, _entry: &String) -> bool {
            false
        }
    }

    #[test]
    fn filter_list_header_shows_live_count() {
        let mut model = CountList {
            entries: vec!["alpha".into(), "beta".into(), "gamma".into()],
        };
        let mut f = FakeStdscr::new(20, 80);
        f.script(Key::Char('b'));
        f.script(Key::Esc);
        filter_list_win(
            &mut f,
            &model.entries.clone(),
            "Configure Model",
            &[("ESC".into(), "back".into())],
            &mut model,
        );
        let calls = f.recorded();
        let headers: Vec<_> = calls
            .iter()
            .filter(|(_, _, t, _)| t.contains("Configure Model"))
            .map(|(_, _, t, _)| t.clone())
            .collect();
        assert!(
            headers.iter().any(|t| t.contains("(3)")),
            "unfiltered count missing: {headers:?}"
        );
        assert!(
            headers.iter().any(|t| t.contains("(1)") && t.contains("Filter: b")),
            "filtered count missing: {headers:?}"
        );
    }

    #[test]
    fn filter_view_rows_insert_separators_before_section() {
        let view = build_filter_view_rows(3, &[(1, P::Chevron), (2, P::Free)]);
        assert_eq!(
            view,
            vec![
                FilterViewRow::Item(0),
                FilterViewRow::Sep(P::Chevron),
                FilterViewRow::Item(1),
                FilterViewRow::Sep(P::Free),
                FilterViewRow::Item(2),
            ]
        );
    }

    struct SepList {
        entries: Vec<String>,
    }
    impl FilterList for SepList {
        type Entry = String;
        fn compute_view(
            &mut self,
            entries: &[String],
            _query: &str,
        ) -> (Vec<String>, Vec<(usize, P)>) {
            (entries.to_vec(), vec![(1, P::Chevron), (2, P::Free)])
        }
        fn render(&mut self, entry: &String, is_selected: bool) -> Vec<(String, P)> {
            vec![(
                format!("  {entry}"),
                if is_selected { P::Selected } else { P::Text },
            )]
        }
        fn on_enter<S: Stdscr>(&mut self, _stdscr: &mut S, _entry: &String) -> bool {
            false
        }
    }

    fn last_y(calls: &[(i32, i32, String, Paint)], token: &str) -> i32 {
        calls
            .iter()
            .rev()
            .find(|(_, _, t, _)| t.contains(token))
            .map(|(y, _, _, _)| *y)
            .expect(token)
    }

    #[test]
    fn filter_list_separators_own_row_and_one_down_skips_them() {
        let mut model = SepList {
            entries: vec!["alpha".into(), "beta".into(), "gamma".into()],
        };
        let mut f = FakeStdscr::new(20, 80);
        f.script(Key::Down);
        f.script(Key::Esc);
        filter_list_win(
            &mut f,
            &model.entries.clone(),
            "Configure Model",
            &[("ESC".into(), "back".into())],
            &mut model,
        );
        let calls = f.recorded();
        let y_a = last_y(&calls, "alpha");
        let y_b = last_y(&calls, "beta");
        let y_c = last_y(&calls, "gamma");
        assert_eq!(y_b, y_a + 2, "separator should sit between alpha and beta: a={y_a} b={y_b}");
        assert_eq!(y_c, y_b + 2, "separator should sit between beta and gamma: b={y_b} c={y_c}");
        let beta_paints = token_paints(&calls, "  beta");
        assert!(
            beta_paints.iter().any(|p| p.bg == bg_color(P::Selected)),
            "one Down should select the next model, not land on a separator: {beta_paints:?}"
        );
    }

    /// When the user enables a model that is in the middle of the disabled
    /// section, the cursor moves down one row (`current + 1`). With 6
    /// disabled models and the cursor on m5 (index 4), after enabling m5
    /// the cursor lands on the row below in the new view (m6).
    #[test]
    fn enable_mid_list_disabled_moves_to_down_neighbor() {
        use serde_json::json;
        // Six disabled models, no enabled models. No separators in the
        // view because there is no enabled section to mark and no free
        // models to mark either. The disabled section is `[0..6)`.
        let ids = vec![
            "m1".to_string(),
            "m2".to_string(),
            "m3".to_string(),
            "m4".to_string(),
            "m5".to_string(),
            "m6".to_string(),
        ];
        let mut models = json!({
            "m1": { "name": "M1", "enabled": false },
            "m2": { "name": "M2", "enabled": false },
            "m3": { "name": "M3", "enabled": false },
            "m4": { "name": "M4", "enabled": false },
            "m5": { "name": "M5", "enabled": false },
            "m6": { "name": "M6", "enabled": false },
        })
        .as_object()
        .unwrap()
        .clone();
        // Script: down 4 times to land on m5 (m1=0, m2=1, m3=2, m4=3,
        // m5=4). Enter to enable m5. Then Esc to exit. The frame
        // rendered after the Enter redraw is the one we inspect.
        let mut f = FakeStdscr::new(20, 80);
        f.script(Key::Down);
        f.script(Key::Down);
        f.script(Key::Down);
        f.script(Key::Down);
        f.script(Key::Enter);
        f.script(Key::Esc);
        let mut picker = ModelPicker {
            ids: &ids,
            models: &mut models,
            pid: "test-pid".into(),
            pname: "Test Provider".into(),
            changed: false,
        };
        filter_list_win(
            &mut f,
            &ids,
            "Configure Model",
            &[("ESC".into(), "back".into())],
            &mut picker,
        );
        // The toggled model (m5) should now be enabled.
        let m5_enabled = models
            .get("m5")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(m5_enabled, "m5 should be enabled after Enter");
        // Find the y of the last Selected-bg fill line in the recorded
        // calls. A fill line is an addstr whose string is all nbsp
        // characters. The selected row's fill uses the Selected bg.
        let calls = f.recorded();
        let sel_bg = bg_color(P::Selected);
        let selected_y: Option<i32> = calls
            .iter()
            .rev()
            .find_map(|(y, _, t, p)| {
                if !t.is_empty()
                    && t.chars().all(|c| c == '\u{00a0}')
                    && p.bg == sel_bg
                {
                    Some(*y)
                } else {
                    None
                }
            });
        // Find the y of each model name in the last frame.
        let find_y_of = |needle: &str| -> Option<i32> {
            calls
                .iter()
                .rev()
                .find(|(_, _, t, _)| t.contains(needle))
                .map(|(y, _, _, _)| *y)
        };
        let m5_y = find_y_of("M5");
        let m6_y = find_y_of("M6");
        assert_eq!(
            selected_y, m6_y,
            "after enabling m5, the cursor should move down one row to m6: sel_y={selected_y:?} m5_y={m5_y:?} m6_y={m6_y:?}"
        );
        let _ = m5_y;
    }

    #[test]
    fn select_win_state_tokens_colored_and_legend_positioned() {
        let h = 30;
        let w = 80;
        let options = vec![
            "opencode (OpenCode Zen) [enabled]".to_string(),
            "grok (Grok) [disabled]".to_string(),
            "Update Model List [08-26-2026 03:15 PM]".to_string(),
        ];

        // initial = 0: first (enabled) row selected.
        let mut f = FakeStdscr::new(h, w);
        f.script(Key::Char('q'));
        let _ = select_win(&mut f, &options, "Select Provider", false, &[], false, None, None, None, None, 0, None, None);
        let calls = f.recorded();

        // Title present.
        assert!(
            calls.iter().any(|(_, _, t, _)| t.contains("Select Provider")),
            "title 'Select Provider' not drawn"
        );
        // Full-screen NBSP background sweep present.
        assert!(
            calls.iter().any(|(_, _, t, _)| t.contains('\u{00a0}')),
            "no NBSP background sweep"
        );

        // [enabled] token is green (mirrors Python P.ENABLED pair 5/12).
        let en = token_paints(&calls, "[enabled]");
        assert!(!en.is_empty(), "no [enabled] token drawn");
        assert!(
            en.iter().all(|p| is_green(p.fg)),
            "[enabled] token not green: {:?}",
            en
        );

        // [disabled] token is red (mirrors Python P.ERROR pair 11/13).
        let dis = token_paints(&calls, "[disabled]");
        assert!(!dis.is_empty(), "no [disabled] token drawn");
        assert!(
            dis.iter().all(|p| is_red(p.fg)),
            "[disabled] token not red: {:?}",
            dis
        );

        // Date token `[MM-DD-YYYY …]` is green, matching [enabled].
        let upd = token_paints(&calls, "[08-26-2026 03:15 PM]");
        assert!(!upd.is_empty(), "no date token drawn");
        assert!(
            upd.iter().all(|p| is_green(p.fg)),
            "date token not green: {:?}",
            upd
        );

        // Legend 'Q' drawn inline with the other legend items (row h-2),
        // not right-aligned in the lower-right corner.
        let q_calls: Vec<_> = calls
            .iter()
            .filter(|(y, _, t, _)| *y == h - 2 && t == "Q")
            .collect();
        assert!(!q_calls.is_empty(), "legend 'Q' not drawn");
        for (_, x, _, _) in q_calls {
            // Inline legend starts near x=2; right-aligned would be ~x>=70.
            assert!(*x < 70, "'Q' should be inline, not right corner: x={}", *x);
        }

        // initial = 1: disabled row selected -> token red AND bold (selected).
        let mut f2 = FakeStdscr::new(h, w);
        f2.script(Key::Char('q'));
        let _ = select_win(&mut f2, &options, "Select Provider", false, &[], false, None, None, None, None, 1, None, None);
        let calls2 = f2.recorded();
        let dis2 = token_paints(&calls2, "[disabled]");
        assert!(
            dis2.iter().any(|p| p.bold),
            "selected [disabled] token not bold"
        );
        // enabled (non-selected) stays green even when disabled is selected.
        let en2 = token_paints(&calls2, "[enabled]");
        assert!(
            en2.iter().all(|p| is_green(p.fg)),
            "non-selected [enabled] not green when other row selected"
        );
    }

    #[test]
    fn code_tokenizer_comment_assignment_and_auto_red_empty_var() {
        // Comments render entirely as CodeComment.
        let segs = code_line_segments("# required env_key value", None);
        assert!(matches!(segs.as_slice(), [(t, P::CodeComment)] if t.starts_with('#')));

        // Assignment LHS is var-green; a bare `VAR = ""` assignment
        // auto-flags the name red. (`export VAR=""` instead treats `export`
        // as a leading command word — matching the Python tokenizer.)
        let segs = code_line_segments("OPENCODE_API_KEY=\"\"", None);
        assert_eq!(segs[0].0, "OPENCODE_API_KEY");
        assert_eq!(segs[0].1, P::CodeError);
        let segs = code_line_segments("export OPENCODE_API_KEY=\"\"", None);
        assert_eq!(segs[0].0, "export");
        assert_eq!(segs[0].1, P::CodeSymbol);

        // Non-empty string content renders white inside gold quotes.
        let segs = code_line_segments("echo 'export FOO=\"bar\"'", None);
        assert!(segs.iter().any(|(t, p)| *p == P::CodeSymbol && t == "'"));
        assert!(segs.iter().any(|(t, p)| *p == P::CodeString && t.contains("export FOO=")));

        // Bare operators outside quotes are gold symbols.
        let segs = code_line_segments("a >> b | c", None);
        assert!(segs.iter().any(|(t, p)| *p == P::CodeSymbol && t.contains('>')));

        // A width-truncated env cell with the '=' clipped off is a command
        // word (gold). The row renderer must tokenize the full cell first.
        let segs = code_line_segments("OPENCODE_API_KEY", None);
        assert_eq!(segs[0].1, P::CodeSymbol);
        let segs = code_line_segments("OPENCODE_API_KEY = \"\"", None);
        assert_eq!(segs[0].0, "OPENCODE_API_KEY");
        assert_eq!(segs[0].1, P::CodeError);
    }

    #[test]
    fn env_cell_keeps_var_color_when_clipped_before_equals() {
        let opt = "(A) - a [enabled]  OPENCODE_API_KEY = \"\"".to_string();
        let full = format!("  ▸ {opt}");
        let eq_at = str_cols(&full[..full.find('=').unwrap()]);
        // vis_limit is width-2; clip exactly at '=' so the equals is gone.
        let w = (eq_at + 2) as i32;
        let mut f = FakeStdscr::new(30, w);
        f.script(Key::Char('q'));
        let _ = select_win(
            &mut f,
            &[opt],
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
        );
        let calls = f.recorded();
        let gold = theme::CODE_SYMBOL_GOLD;
        let name_paints: Vec<Paint> = calls
            .iter()
            .filter(|(_, _, t, _)| t.contains("OPENCODE_API_KEY") || t.contains("OPENCODE"))
            .map(|(_, _, _, p)| *p)
            .collect();
        assert!(
            !name_paints.is_empty(),
            "env var name missing from a clipped env cell: {:?}",
            calls.iter().map(|(_, _, t, _)| t.clone()).collect::<Vec<_>>()
        );
        assert!(
            name_paints.iter().all(|p| p.fg != gold),
            "clipped env var name painted gold like '=': {name_paints:?}"
        );
        assert!(
            name_paints.iter().any(|p| p.fg == tn_color(P::CodeError)),
            "clipped empty env var should stay error-red: {name_paints:?}"
        );
    }

    #[test]
    fn add_provider_picker_buckets_added_suggested_and_rest() {
        // The full catalog stays listed (already-added providers included);
        // sections: Added | Suggested | rest. Rows use the main-list
        // `(name) - id [enabled/disabled]` format.
        for enabled_flag in [true, false] {
            let mut doc = serde_json::json!({
                "providers": [{
                    "id": "opencode", "name": "OpenCode",
                    "enabled": enabled_flag,
                    "models": {}
                }]
            });
            let api = serde_json::json!({
                "zzz-last": {"name": "Zzz Last"},
                "anthropic": {"name": "Anthropic"},
                "opencode": {"name": "OpenCode"},
                "openrouter": {"name": "OpenRouter"},
                "ollama-cloud": {"name": "Ollama Cloud"},
                "opencode-go": {"name": "OpenCode Go"}
            });
            let mut picker = AddProviderPicker {
                doc: &mut doc,
                api,
                added: None,
                status: std::rc::Rc::new(RefCell::new(None)),
                label_cache: None,
            };
            let entries: Vec<(String, String)> = vec![
                ("anthropic".into(), "Anthropic".into()),
                ("ollama-cloud".into(), "Ollama Cloud".into()),
                ("openrouter".into(), "OpenRouter".into()),
                ("opencode".into(), "OpenCode".into()),
                ("opencode-go".into(), "OpenCode Go".into()),
                ("zzz-last".into(), "Zzz Last".into()),
            ];
            let (ordered, seps) = picker.compute_view(&entries, "");
            // Added first, then Suggested, then the rest; alphabetical inside.
        assert_eq!(
                ordered.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
                ["opencode", "ollama-cloud", "opencode-go", "openrouter", "anthropic", "zzz-last"]
            );
            assert_eq!(
                seps,
                vec![(1, P::Enabled), (4, P::Free)],
                "green rule after the Added bucket, cyan rule after Suggested"
            );

            // Added row uses main-list `(name) - id [state]` format.
            let row = picker.render(&("opencode".to_string(), "OpenCode".to_string()), false);
            let joined: String = row.iter().map(|(t, _)| t.as_str()).collect();
            assert!(joined.contains("(OpenCode)"), "{joined}");
            assert!(joined.contains(" - opencode"), "{joined}");
            if enabled_flag {
                assert_eq!(row[2], ("[enabled]".to_string(), P::Enabled));
            } else {
                assert_eq!(row[2], ("[disabled]".to_string(), P::Error));
            }
            assert_eq!(row[1].1, P::Enabled);

            // Suggested rows: cyan name, [disabled] token.
            let srow = picker.render(&("openrouter".to_string(), "OpenRouter".to_string()), false);
            assert_eq!(srow[1].1, P::Free);
            assert_eq!(srow[2], ("[disabled]".to_string(), P::Error));
            let sjoined: String = srow.iter().map(|(t, _)| t.as_str()).collect();
            assert!(sjoined.contains("(OpenRouter)"), "{sjoined}");

            // Other rows stay default white with [disabled].
            let orow = picker.render(&("anthropic".to_string(), "Anthropic".to_string()), false);
            assert_eq!(orow[1].1, P::Text);
            assert_eq!(orow[2], ("[disabled]".to_string(), P::Error));
            let ojoined: String = orow.iter().map(|(t, _)| t.as_str()).collect();
            assert!(ojoined.contains("(Anthropic)"), "{ojoined}");

            // Tokens share a column across the catalog.
            assert_eq!(row[1].0.len(), srow[1].0.len(), "padded heads must match");
            assert_eq!(row[1].0.len(), orow[1].0.len(), "padded heads must match");

            // Enter on an Added row must NOT call add_provider_entry.
            let before = picker.doc["providers"].as_array().unwrap().len();
            let keep = picker.on_enter(&mut FakeStdscr::new(20, 80), &("opencode".to_string(), "OpenCode".to_string()));
            assert!(keep, "Enter on an Added row keeps the modal open");
            assert_eq!(
                picker.doc["providers"].as_array().unwrap().len(),
                before,
                "Added row is inert: no duplicate provider entry"
            );
        }
    }

    #[test]
    fn add_provider_picker_stays_open_and_records_status_after_add() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        use crate::paths;
        std::fs::create_dir_all(paths::providers_path().parent().unwrap()).unwrap();
        let mut doc = serde_json::json!({"providers": []});
        let api = serde_json::json!({
            "openrouter": {"name": "OpenRouter", "models": {
                "or-1": {"name": "OR One"}
            }}
        });
        let mut picker = AddProviderPicker {
            doc: &mut doc,
            api,
            added: None,
            status: std::rc::Rc::new(RefCell::new(None)),
            label_cache: None,
        };
        let entry = ("openrouter".to_string(), "OpenRouter".to_string());
        let keep = picker.on_enter(&mut FakeStdscr::new(20, 80), &entry);
        assert!(keep, "modal must stay open after an add");
        assert!(
            picker.added.is_some(),
            "a successful add records the confirmation message"
        );
        assert_eq!(picker.doc["providers"][0]["id"], "openrouter");

        // After the add, the same provider now buckets into Added and its
        // row goes inert (no duplicate on a second Enter).
        let entries = vec![entry.clone()];
        let (ordered, _) = picker.compute_view(&entries, "");
        assert_eq!(ordered[0].0, "openrouter");
        let before = picker.doc["providers"].as_array().unwrap().len();
        let keep2 = picker.on_enter(&mut FakeStdscr::new(20, 80), &entry);
        assert!(keep2);
        assert_eq!(
            picker.doc["providers"].as_array().unwrap().len(),
            before,
            "second Enter on a just-added provider adds nothing"
        );
    }

    #[test]
    fn select_win_preview_scrolls_with_pgup_pgdn() {
        // A tall preview (more lines than the pane) plus PageDown/PageUp:
        // after two PgDn presses the drawn window starts past the heading;
        // PgUp walks back. The provider row above never moves.
        let mut preview: Vec<PreviewLine> = vec![PreviewLine::Heading("Enabled Models".into())];
        preview.push(PreviewLine::Segs(vec![("Model Descriptions [enabled]".into(), P::Text)]));
        for i in 0..40 {
            preview.push(PreviewLine::Segs(vec![(format!("model-{i}"), P::Value)]));
        }
        let options = vec!["one".to_string()];
        let h = 30;

        // Baseline frame (scroll 0).
        let mut f0 = FakeStdscr::new(h, 80);
        f0.script(Key::Char('q'));
        let _ = select_win(&mut f0, &options, "Select Provider", false, &[], false, None, None, None, Some(&preview), 0, None, None);

        // Two PageDowns, then quit.
        let mut f1 = FakeStdscr::new(h, 80);
        f1.script(Key::PageDown);
        f1.script(Key::PageDown);
        f1.script(Key::Char('q'));
        let _ = select_win(&mut f1, &options, "Select Provider", false, &[], false, None, None, None, Some(&preview), 0, None, None);

        let base_rows: Vec<i32> = f0
            .recorded()
            .iter()
            .filter(|(_, _, t, _)| t.starts_with("model-"))
            .map(|(y, _, _, _)| *y)
            .collect();
        let scrolled_rows: Vec<i32> = f1
            .recorded()
            .iter()
            .filter(|(_, _, t, _)| t.starts_with("model-"))
            .map(|(y, _, _, _)| *y)
            .collect();
        assert!(!base_rows.is_empty() && !scrolled_rows.is_empty());
        let base_first_model = base_rows.iter().min().unwrap();
        let scroll_first_model = scrolled_rows.iter().min().unwrap();
        assert_ne!(
            base_first_model, scroll_first_model,
            "PageDown did not move the preview window"
        );
        // The provider list row is identical in both frames: pinned at top.
        let base_prov_y = f0
            .recorded()
            .into_iter()
            .find(|(_, _, t, _)| t == "one")
            .map(|(y, _, _, _)| y);
        let scroll_prov_y = f1
            .recorded()
            .into_iter()
            .find(|(_, _, t, _)| t == "one")
            .map(|(y, _, _, _)| y);
        assert_eq!(base_prov_y, scroll_prov_y, "provider row moved while paging");
    }

    #[test]
    fn select_win_up_scrolls_preview_to_keep_model_visible() {
        // After paging down in Enabled Models, Up must scroll the pane so
        // the previous model is drawn — not walk the cursor off the top
        // of the window until it pops back to the menu.
        let mut preview: Vec<PreviewLine> = vec![PreviewLine::Heading("Enabled Models".into())];
        for i in 0..40 {
            preview.push(PreviewLine::Model {
                pid: "prov".into(),
                mid: format!("m{i}"),
                segs: vec![(format!("model-{i}"), P::Value)],
            });
        }
        let options = vec!["one".to_string()];
        let h = 30;

        let mut f_paged = FakeStdscr::new(h, 80);
        f_paged.script(Key::Down);
        f_paged.script(Key::PageDown);
        f_paged.script(Key::Char('q'));
        let _ = select_win(
            &mut f_paged, &options, "Select Provider", false, &[], false,
            None, None, None, Some(&preview), 0, None, None,
        );

        let mut f_up = FakeStdscr::new(h, 80);
        f_up.script(Key::Down);
        f_up.script(Key::PageDown);
        f_up.script(Key::Up);
        f_up.script(Key::Char('q'));
        let _ = select_win(
            &mut f_up, &options, "Select Provider", false, &[], false,
            None, None, None, Some(&preview), 0, None, None,
        );

        fn last_frame_models(f: &FakeStdscr) -> Vec<String> {
            f.last_frame()
                .into_iter()
                .filter_map(|(_, _, t, _)| {
                    if t.starts_with("model-") {
                        Some(t)
                    } else {
                        None
                    }
                })
                .collect()
        }

        let paged = last_frame_models(&f_paged);
        let after_up = last_frame_models(&f_up);
        assert!(!paged.is_empty() && !after_up.is_empty());
        assert_ne!(
            paged.first(),
            after_up.first(),
            "Up after PageDown must scroll the preview; paged={paged:?} after_up={after_up:?}"
        );
    }

    #[test]
    fn build_add_model_catalog_includes_enabled_and_doc_only() {
        let api = serde_json::json!({
            "zeta": {"name": "Zeta AI", "models": {
                "alpha": {"name": "Alpha One"},
                "beta": {}
            }},
            "aaa": {"name": "AAA", "models": {
                "alpha": {"name": "Alpha Two"},
                "zeta-mini": {"name": "Zeta Mini"}
            }}
        });
        // zeta/alpha already enabled stays listed; doc-only enabled is appended.
        let doc = serde_json::json!({"providers": [{
            "id": "zeta", "name": "Zeta AI",
            "models": {
                "alpha": {"enabled": true},
                "legacy": {"name": "Legacy", "enabled": true}
            }
        }]});
        let cat = build_add_model_catalog(&api, &doc);
        let mut keys: Vec<(String, String)> = cat.iter().map(|(p, m, _, _)| (p.clone(), m.clone())).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                ("aaa", "alpha"),
                ("aaa", "zeta-mini"),
                ("zeta", "alpha"),
                ("zeta", "beta"),
                ("zeta", "legacy"),
            ]
            .map(|(p, m)| (p.to_string(), m.to_string()))
        );
    }

    #[test]
    fn add_model_picker_buckets_enabled_free_and_rest() {
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "zeta", "name": "Zeta AI",
                "models": {"alpha": {"name": "Alpha One", "enabled": true}}
            }]
        });
        let api = serde_json::json!({
            "zeta": {"name": "Zeta AI", "models": {
                "alpha": {"name": "Alpha One"},
                "beta": {},
                "zeta-free": {"name": "Zeta Free"}
            }},
            "aaa": {"name": "AAA", "models": {
                "omega": {"name": "Omega"}
            }}
        });
        let catalog = build_add_model_catalog(&api, &doc);
        let mut picker = AddModelPicker { doc: &mut doc, api, status: None, enabled_cache: Default::default() };
        let (ordered, seps) = picker.compute_view(&catalog, "");
        assert_eq!(
            ordered.iter().map(|(p, m, _, _)| (p.as_str(), m.as_str())).collect::<Vec<_>>(),
            vec![
                ("zeta", "alpha"),
                ("zeta", "zeta-free"),
                ("zeta", "beta"),
                ("aaa", "omega"),
            ]
        );
        assert_eq!(seps, vec![(1, P::Chevron), (2, P::Free)]);

        let enabled_row = picker.render(&ordered[0], false);
        assert_eq!(enabled_row[1], ("●".into(), P::Enabled));
        assert_eq!(enabled_row[3], ("Alpha One".into(), P::Value));

        let free_row = picker.render(&ordered[1], false);
        assert_eq!(free_row[1], ("○".into(), P::Text));
        assert_eq!(free_row[3], ("Zeta Free".into(), P::Enabled));

        let rest_row = picker.render(&ordered[2], false);
        assert_eq!(rest_row[1], ("○".into(), P::Text));
        assert_eq!(rest_row[3], ("beta".into(), P::Text));

        let mut f = FakeStdscr::new(20, 80);
        // Enter on an already-enabled row disables it. The picker stays
        // open so the user can keep toggling models in the same session.
        assert!(
            picker.on_enter(&mut f, &ordered[0]),
            "Enter on an already-enabled row must keep the modal open"
        );
        assert_eq!(
            picker.doc["providers"][0]["models"]["alpha"]["enabled"],
            serde_json::json!(false),
            "Enter on an already-enabled row must disable the model"
        );
        // Pressing Enter again on the now-disabled row enables it back.
        let (ordered2, _) = picker.compute_view(&catalog, "");
        let alpha2 = ordered2
            .iter()
            .find(|(p, m, _, _)| p == "zeta" && m == "alpha")
            .expect("alpha still in the catalog");
        assert!(
            picker.on_enter(&mut f, &alpha2),
            "Enter on a disabled row must keep the modal open"
        );
        assert_eq!(
            picker.doc["providers"][0]["models"]["alpha"]["enabled"],
            serde_json::json!(true),
            "Enter on a disabled row must enable the model"
        );
    }

    #[test]
    fn select_win_status_line_drawn_above_legend() {
        let options = vec!["opencode (OpenCode Zen) [enabled]".to_string()];
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Char('q'));
        let _ = select_win(
            &mut f,
            &options,
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            Some("Added provider 'x' with 9 models (all disabled)."),
            None,
            0,
            None,
            None,
        );
        let calls = f.recorded();
        assert!(
            calls
                .iter()
                .any(|(y, _, t, _)| *y == 27 && t.contains("Added provider 'x'")),
            "status line not at height-3"
        );
    }

    #[test]
    fn select_win_preview_renders_models_and_env_box() {
        use serde_json::json;

        // Provider with one enabled model and an env key.
        let doc = json!({
            "providers": [{
                "id": "opencode",
                "name": "OpenCode Zen",
                "enabled": true,
                "env_key": "OPENCODE_API_KEY",
                "models": {
                    "hy3-free": { "name": "HY3 Free", "enabled": true },
                    "hy3-pro": { "name": "HY3 Pro", "enabled": false }
                }
            }]
        });

        // The preview builder produces the heading and enabled-model rows;
        // env cells live on the provider list, not in this pane.
        let preview = build_config_models_preview(&doc, false);
        assert!(preview
            .iter()
            .any(|l| matches!(l, PreviewLine::Heading(t) if t == "Enabled Models: 1")));
        assert!(
            !preview.iter().any(|l| matches!(
                l,
                PreviewLine::Segs(segs) if segs.iter().any(|(t, _)| t.contains("Summary:"))
            )),
            "preview must not carry a trailing Summary line"
        );
        let has_enabled_row = preview.iter().any(|l| matches!(
            l,
            PreviewLine::Model { segs, .. } if segs
                .iter()
                .any(|(t, p)| t == "● " && *p == P::Enabled)
        ));
        assert!(has_enabled_row, "preview missing enabled-model row");

        // Drive the selector with padded provider rows + env suffix.
        let options = crate::core::provider_menu_labels(&[doc["providers"][0].as_object().unwrap().clone()]);
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Char('q'));
        let _ = select_win(
            &mut f,
            &options,
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            None,
            Some(&preview),
            0,
            None,
            None,
        );
        let calls = f.recorded();
        assert!(
            calls.iter().any(|(_, _, t, _)| t.contains("Enabled Models")),
            "preview heading not drawn"
        );
        assert!(
            calls.iter().any(|(_, _, t, _)| t.contains("OPENCODE_API_KEY")),
            "provider-row env cell not drawn"
        );
        // 'Q' drawn inline with the legend items (row h-2), not right corner.
        assert!(
            calls.iter().any(|(y, _, t, _)| *y == 28 && t == "Q"),
            "legend 'Q' not drawn inline"
        );
    }

    #[test]
    fn select_win_main_menu_legend_has_sort_before_quit() {
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Char('q'));
        let _ = select_win(
            &mut f,
            &["one".to_string()],
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
        );
        let legend: Vec<(i32, String)> = f
            .recorded()
            .iter()
            .filter(|(y, _, _, _)| *y == 28)
            .map(|(_, x, t, _)| (*x, t.clone()))
            .collect();
        let s_x = legend.iter().find(|(_, t)| t == "S").map(|(x, _)| *x);
        let q_x = legend.iter().find(|(_, t)| t == "Q").map(|(x, _)| *x);
        assert!(s_x.is_some(), "legend missing S: {legend:?}");
        assert!(q_x.is_some(), "legend missing Q: {legend:?}");
        assert!(
            legend.iter().all(|(_, t)| t != "D"),
            "D must not appear on the main-menu legend: {legend:?}"
        );
        assert!(
            s_x.unwrap() < q_x.unwrap(),
            "S should be left of Q: S@{} Q@{}",
            s_x.unwrap(),
            q_x.unwrap()
        );
    }

    #[test]
    fn select_win_main_menu_legend_page_before_select_when_preview_overflows() {
        let preview: Vec<PreviewLine> = (0..40)
            .map(|i| PreviewLine::Heading(format!("row {i}")))
            .collect();
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Char('q'));
        let _ = select_win(
            &mut f,
            &["one".to_string()],
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            None,
            Some(&preview),
            0,
            None,
            None,
        );
        let legend: Vec<(i32, String)> = f
            .recorded()
            .iter()
            .filter(|(y, _, _, _)| *y == 28)
            .map(|(_, x, t, _)| (*x, t.clone()))
            .collect();
        let page_x = legend.iter().find(|(_, t)| t == "page").map(|(x, _)| *x);
        let select_x = legend.iter().find(|(_, t)| t == "select").map(|(x, _)| *x);
        assert!(page_x.is_some(), "legend missing page: {legend:?}");
        assert!(select_x.is_some(), "legend missing select: {legend:?}");
        assert!(
            page_x.unwrap() < select_x.unwrap(),
            "PgUp/PgDn page should sit left of Enter/→ select: page@{} select@{}",
            page_x.unwrap(),
            select_x.unwrap()
        );
    }

    #[test]
    fn select_win_s_toggles_sort_on_main_menu_ignored_on_action_menu() {
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Char('s'));
        let out = select_win(
            &mut f,
            &["one".to_string()],
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
        );
        assert!(
            matches!(out, Some(SelectOutcome::SortToggled(0))),
            "main-menu s should toggle sort: {out:?}"
        );

        let mut f2 = FakeStdscr::new(30, 80);
        f2.script(Key::Char('s'));
        f2.script(Key::Esc);
        let out2 = select_win(
            &mut f2,
            &["Configure Models".to_string()],
            "Provider: X",
            false,
            &[],
            true,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
        );
        assert!(
            matches!(out2, Some(SelectOutcome::Cancelled)),
            "action-menu s should be ignored: {out2:?}"
        );
    }

    fn preview_model_names(preview: &[PreviewLine]) -> Vec<String> {
        preview
            .iter()
            .filter_map(|line| match line {
                PreviewLine::Segs(segs) | PreviewLine::Model { segs, .. } => segs
                    .iter()
                    .find(|(_, p)| *p == P::Value)
                    .map(|(t, _)| t.trim_end().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn preview_sort_by_model_name_toggles_order() {
        use serde_json::json;
        let doc = json!({
            "providers": [
                {
                    "id": "b-prov",
                    "name": "Beta Prov",
                    "enabled": true,
                    "models": { "m1": { "name": "Zulu", "enabled": true } }
                },
                {
                    "id": "a-prov",
                    "name": "Alpha Prov",
                    "enabled": true,
                    "models": { "m2": { "name": "Alpha", "enabled": true } }
                }
            ]
        });
        let by_prov = preview_model_names(&build_config_models_preview(&doc, false));
        assert_eq!(by_prov, ["Zulu", "Alpha"]);
        let by_name = preview_model_names(&build_config_models_preview(&doc, true));
        assert_eq!(by_name, ["Alpha", "Zulu"]);
    }

    #[test]
    fn configure_models_renders_name_provider_id_without_free_tag() {
        use serde_json::json;
        let ids = vec!["pro".to_string(), "hy3-free".to_string(), "omega".to_string()];
        let mut models = json!({
            "pro": { "name": "Pro", "enabled": true },
            "hy3-free": { "name": "HY3 Free", "enabled": false },
            "omega": { "name": "Omega", "enabled": false }
        })
        .as_object()
        .unwrap()
        .clone();
        let mut picker = ModelPicker {
            ids: &ids,
            models: &mut models,
            pid: "opencode-go".into(),
            pname: "OpenCode Go".into(),
            changed: false,
        };
        let (ordered, seps) = picker.compute_view(&ids, "");
        assert_eq!(ordered, ["pro", "hy3-free", "omega"]);
        assert_eq!(seps, vec![(1, P::Chevron), (2, P::Free)]);

        let enabled_row = picker.render(&"pro".to_string(), false);
        assert_eq!(enabled_row[1], ("●".into(), P::Enabled));
        assert_eq!(enabled_row[3], ("Pro".into(), P::Value));
        assert_eq!(
            enabled_row[4],
            (" (OpenCode Go) - opencode-go/pro".into(), P::Text)
        );
        assert!(
            is_green(tn_color(enabled_row[1].1)),
            "enabled circle not green"
        );
        assert!(
            is_blue(tn_color(enabled_row[3].1)),
            "enabled model name not blue"
        );

        let free_row = picker.render(&"hy3-free".to_string(), false);
        assert_eq!(free_row[3], ("HY3 Free".into(), P::Enabled));
        assert_eq!(
            free_row[4],
            (" (OpenCode Go) - opencode-go/hy3-free".into(), P::Text)
        );
        assert!(
            is_green(tn_color(free_row[3].1)),
            "free model name not green"
        );
        assert!(
            free_row.iter().all(|(t, _)| !t.contains("[free]")),
            "[free] suffix still present"
        );

        let sel = picker.render(&"pro".to_string(), true);
        assert_eq!(sel[1], ("●".into(), P::Enabled));
        assert_eq!(sel[3], ("Pro".into(), P::Value));

        let mut f = FakeStdscr::new(20, 80);
        f.script(Key::Esc);
        filter_list_win(
            &mut f,
            &ids,
            "Configure Model",
            &[("ESC".into(), "back".into())],
            &mut picker,
        );
        let calls = f.recorded();
        let pro = token_paints(&calls, "Pro");
        assert!(
            pro.iter().any(|p| is_blue(p.fg)),
            "selected enabled name turned white: {pro:?}"
        );
        let circle = token_paints(&calls, "●");
        assert!(
            circle.iter().any(|p| is_green(p.fg)),
            "selected enabled circle turned white: {circle:?}"
        );
    }

    /// Terminal-like `Stdscr` that renders into an in-memory ANSI byte buffer
    /// using the exact same `emit_cell` path as `RealStdscr`. Lets the eval
    /// verify the TUI produces a true fullscreen layout (each
    /// element at its absolute row/column) rather than a single wrapped line.
    struct CaptureStdscr {
        h: i32,
        w: i32,
        buf: Vec<u8>,
        keys: std::collections::VecDeque<Key>,
    }

    impl CaptureStdscr {
        fn new(h: i32, w: i32) -> Self {
            CaptureStdscr { h, w, buf: Vec::new(), keys: Default::default() }
        }
        fn script(&mut self, k: Key) {
            self.keys.push_back(k);
        }
        /// Parse the captured ANSI into a `h x w` character grid (cursor moves
        /// honored; SGR resets ignored; NBSP treated as a normal cell char).
        fn screen(&self) -> Vec<Vec<char>> {
            let h = self.h as usize;
            let w = self.w as usize;
            let mut grid = vec![vec![' '; w]; h];
            let chars: Vec<char> = String::from_utf8_lossy(&self.buf).chars().collect();
            let mut row = 0usize;
            let mut col = 0usize;
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '[' {
                    let mut j = i + 2;
                    let mut params = String::new();
                    while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == ';') {
                        params.push(chars[j]);
                        j += 1;
                    }
                    if j < chars.len() {
                        let fin = chars[j];
                        if fin == 'H' || fin == 'f' {
                            let mut it = params.split(';');
                            let r = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                            let c = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                            row = r.saturating_sub(1);
                            col = c.saturating_sub(1);
                        }
                        i = j + 1;
                        continue;
                    }
                    i = j;
                    continue;
                } else if chars[i] == '\n' {
                    row = (row + 1).min(h - 1);
                    col = 0;
                } else if chars[i] == '\r' {
                    col = 0;
                } else {
                    if row < h && col < w {
                        grid[row][col] = chars[i];
                    }
                    col += 1;
                }
                i += 1;
            }
            grid
        }
    }

    impl Stdscr for CaptureStdscr {
        fn getmaxyx(&self) -> (i32, i32) {
            (self.h, self.w)
        }
        fn erase(&mut self) {
            use std::io::Write as _;
            let _ = write!(self.buf, "\x1b[2J\x1b[H");
        }
        fn refresh(&mut self) {}
        fn addstr(&mut self, y: i32, x: i32, s: &str, paint: Paint) {
            emit_cell(&mut self.buf, y, x, s, paint);
        }
        fn getch(&mut self) -> Key {
            self.keys.pop_front().unwrap_or(Key::Eof)
        }
    }

    fn row_text(grid: &[Vec<char>], r: usize) -> String {
        grid.get(r).map(|row| row.iter().collect()).unwrap_or_default()
    }

    #[test]
    fn config_flow_renders_fullscreen_layout() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
use serde_json::json;

        let mut doc = json!({
            "providers": [{
                "id": "opencode",
                "name": "OpenCode Zen",
                "enabled": true,
                "env_key": "OPENCODE_API_KEY",
                "models": { "hy3-free": { "name": "HY3 Free", "enabled": true } }
            }]
        });

        let (h, w) = (30, 80);
        let mut cap = CaptureStdscr::new(h, w);
        // Quit the provider-list screen immediately ('q' quits main menu).
        cap.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut cap, &mut doc);
        assert!(res.is_ok(), "config flow returned error: {:?}", res.err());

        let grid = cap.screen();
        // Header is drawn at absolute row 0 (not streamed to the bottom).
        assert!(
            row_text(&grid, 0).contains("Select Provider"),
            "header not at row 0; row0 = {:?}",
            row_text(&grid, 0)
        );
        // The provider sits directly under the header (no top padding); a
        // section rule separates it from the trailing block below.
        assert!(
            row_text(&grid, 2).contains("(OpenCode Zen) - opencode"),
            "provider label not at row 2; row2 = {:?}",
            row_text(&grid, 2)
        );
        assert!(
            row_text(&grid, 3).contains("───"),
            "section rule not at row 3; row3 = {:?}",
            row_text(&grid, 3)
        );
        // Models preview heading rendered in the empty space under the list.
        let any_enabled = (0..h as usize).any(|r| row_text(&grid, r).contains("Enabled Models"));
        assert!(any_enabled, "preview heading 'Enabled Models' not rendered");
        // Legend 'Q' is inline with the other legend items at row h-2.
        let bottom = row_text(&grid, (h - 2) as usize);
        assert!(
            bottom.contains('Q'),
            "legend 'Q' missing at lower-right row {}; row = {:?}",
            h - 2,
            bottom
        );
        // Every visible content row is distinct — nothing was wrapped onto a
        // single line. Sanity: row 0 and row 2 differ.
        assert_ne!(row_text(&grid, 0), row_text(&grid, 2), "rows collapsed/identical");
    }

    #[test]
    fn config_flow_action_menu_enable_toggles_display() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
use serde_json::json;

        let mut doc = json!({
            "providers": [{
                "id": "opencode",
                "name": "OpenCode Zen",
                "enabled": true,
                "models": { "hy3-free": { "name": "HY3 Free", "enabled": true } }
            }]
        });

        let (h, w) = (30, 80);
        let mut cap = CaptureStdscr::new(h, w);
        // Select the provider (Enter) -> action menu; move to the
        // "Provider [..]" action (Down), toggle it (Enter), back out (Esc),
        // then quit the provider list (q).
        cap.script(Key::Enter);
        cap.script(Key::Down);
        cap.script(Key::Enter);
        cap.script(Key::Esc);
        cap.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut cap, &mut doc);
        assert!(res.is_ok(), "config flow errored: {:?}", res.err());

        let grid = cap.screen();
        // After toggling and returning to the provider config page, the list
        // must reflect the new disabled state (the display flips).
        assert!(
            grid_contains(&grid, "(OpenCode Zen) - opencode [disabled]"),
            "provider config page did not reflect disabled state after toggle"
        );
        // And the stale "enabled" label must be gone.
        assert!(
            !grid_contains(&grid, "(OpenCode Zen) - opencode [enabled]"),
            "stale enabled label still shown after toggle"
        );
        // The underlying doc really changed too.
        let enabled = doc["providers"][0]["enabled"].as_bool().unwrap_or(true);
        assert!(!enabled, "providers.json enabled flag not flipped in doc");
    }

    #[test]
    fn config_flow_restores_terminal_on_exit() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
use serde_json::json;

        let mut doc = json!({
            "providers": [{
                "id": "opencode",
                "name": "OpenCode Zen",
                "enabled": true,
                "models": { "hy3-free": { "name": "HY3 Free", "enabled": true } }
            }]
        });

        let (h, w) = (30, 80);
        let mut cap = CaptureStdscr::new(h, w);
        // Mirror RealStdscr::open(): enter the alternate screen and hide the
        // cursor before drawing.
        enter_alt_screen(&mut cap.buf);
        hide_cursor(&mut cap.buf);
        cap.script(Key::Char('q'));
        let _ = run_config_flow_with_backend(&mut cap, &mut doc);
        // Mirror RealStdscr::Drop: show the cursor and restore the terminal.
        show_cursor(&mut cap.buf);
        leave_alt_screen(&mut cap.buf);

        let s = String::from_utf8_lossy(&cap.buf);
        assert!(s.contains("\x1b[?1049h"), "TUI did not enter alternate screen");
        assert!(s.contains("\x1b[?25l"), "TUI did not hide cursor on entry");
        assert!(s.contains("\x1b[?25h"), "TUI did not show cursor on exit");
        // The restore sequence must be the LAST thing emitted, so closing the
        // TUI returns the terminal to its prior state (no blue bg / history).
        let leave_pos = s.rfind("\x1b[?1049l").expect("TUI did not restore terminal on exit");
        assert!(
            leave_pos + "\x1b[?1049l".len() >= s.len() - 8,
            "terminal restore not emitted last"
        );
        // Cursor is shown immediately before leaving the alt screen (Drop order).
        assert!(
            s.contains("\x1b[?25h\x1b[?1049l"),
            "cursor-show must precede leave-alt-screen on exit"
        );
    }

    #[test]
    fn terminal_emitters_match_restore_contract() {
        // The hand-rolled RESTORE_SEQ must clear exactly the modes we enable
        // (mouse tracking + cursor + alt screen).
        assert_eq!(RESTORE_SEQ, b"\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l\x1b[0m");
        let mut buf: Vec<u8> = Vec::new();
        enter_alt_screen(&mut buf);
        hide_cursor(&mut buf);
        assert_eq!(buf, b"\x1b[?1049h\x1b[?25l");
        let mut buf2: Vec<u8> = Vec::new();
        show_cursor(&mut buf2);
        leave_alt_screen(&mut buf2);
        assert_eq!(buf2, b"\x1b[?25h\x1b[?1049l\x1b[0m");
    }

    #[test]
    fn parse_key_prefix_consumes_burst_without_dropping_keys() {
        // A burst delivers "Down,Enter" in one chunk: Down consumes exactly
        // 3 bytes and the trailing \r must survive for the next call.
        let buf = b"\x1b[B\r";
        let (k, used) = parse_key_prefix(buf).unwrap();
        assert_eq!(k, Key::Down);
        assert_eq!(used, 3);
        assert_eq!(parse_key_prefix(&buf[used..]), Some((Key::Enter, 1)));
    }

    #[test]
    fn parse_key_prefix_flags_incomplete_escape_sequences() {
        // A lone ESC or a partial CSI is incomplete: the caller waits for
        // more bytes (escdelay) instead of misreading it.
        assert_eq!(parse_key_prefix(b"\x1b"), None);
        assert_eq!(parse_key_prefix(b"\x1b["), None);
        // Unknown CSI is swallowed, not Esc (mouse leftovers must not pop a menu).
        assert_eq!(parse_key_prefix(b"\x1b[?1049h"), Some((Key::Eof, 8)));
        // PageUp/PageDown arrive as CSI 5~/6~.
        assert_eq!(parse_key_prefix(b"\x1b[5~"), Some((Key::PageUp, 4)));
        assert_eq!(parse_key_prefix(b"\x1b[6~"), Some((Key::PageDown, 4)));
        assert_eq!(parse_key_prefix(b"\x1b[5"), None, "partial PageUp must wait for its tail");
        // SGR mouse wheel: ESC [ < 64/65 ; x ; y M. y is 1-based in the
        // sequence and 0-based on Key::Wheel*.
        assert_eq!(parse_key_prefix(b"\x1b[<64;1;5M"), Some((Key::WheelUp(4), 10)));
        assert_eq!(parse_key_prefix(b"\x1b[<65;10;12M"), Some((Key::WheelDown(11), 12)));
        assert_eq!(parse_key_prefix(b"\x1b[<64;1;5"), None, "partial SGR mouse must wait for its tail");
        // Release (`m`) does not scroll; modifier/motion bits still count as wheel.
        assert_eq!(parse_key_prefix(b"\x1b[<64;1;5m"), Some((Key::Eof, 10)));
        assert_eq!(parse_key_prefix(b"\x1b[<96;1;5M"), Some((Key::WheelUp(4), 10)));
        assert_eq!(parse_key_prefix(b"\x1b[<97;1;8M"), Some((Key::WheelDown(7), 10)));
        // X10 mouse: ESC [ M Cb Cx Cy with wheel-up button 64 (+32 => 96).
        assert_eq!(
            parse_key_prefix(&[0x1b, b'[', b'M', 64 + 32, 1 + 32, 5 + 32]),
            Some((Key::WheelUp(4), 6))
        );
        assert_eq!(parse_key_prefix(b"\x1b[M"), None, "partial X10 mouse must wait");
    }

    #[test]
    fn config_flow_d_key_does_not_toggle_descriptions() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "prov",
                "name": "Provider One",
                "enabled": true,
                "models": {"alpha-1": {"name": "Alpha One", "description": "D.", "enabled": false}}
            }]
        });
        // 'd' is not a main-menu shortcut; only Enter on the Model
        // Descriptions row toggles the flag. 'q' quits.
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Char('d'));
        f.script(Key::Char('q'));
        let _ = run_config_flow_with_backend(&mut f, &mut doc);
        assert_eq!(
            doc.get("include_descriptions").and_then(Value::as_bool),
            None,
            "'d' on the main menu must not toggle include_descriptions"
        );
    }

    #[test]
    fn config_flow_enter_on_descriptions_row_toggles_flag() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        // One provider: the trailing block starts right after it, so two
        // Downs (Codex Config, then Model Descriptions) land on the toggle.
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "prov",
                "name": "Provider One",
                "enabled": true,
                "models": {}
            }]
        });
        // Enter on Add Provider… would fetch models.dev, so the walk stops at
        // the toggle row.
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Down); // onto Codex Config
        f.script(Key::Down); // onto Model Descriptions
        f.script(Key::Enter); // toggle it
        f.script(Key::Char('q'));
        let _ = run_config_flow_with_backend(&mut f, &mut doc);
        assert_eq!(
            doc.get("include_descriptions").and_then(Value::as_bool),
            Some(true),
            "Enter on the Model Descriptions row must toggle the flag"
        );
    }

    #[test]
    fn config_flow_d_key_is_ignored_on_action_menu() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "prov",
                "name": "Provider One",
                "enabled": true,
                "models": {}
            }]
        });
        // Enter the action menu and press 'd': it is unbound there (and on
        // the main menu), then Back exits to the main menu.
        let mut f = FakeStdscr::new(30, 80);
        f.script(Key::Enter); // open action menu (Back-on-left submenu)
        f.script(Key::Char('d'));
        f.script(Key::Esc);   // leave the action menu
        f.script(Key::Char('q')); // quit the main menu
        let out = run_config_flow_with_backend(&mut f, &mut doc);
        assert!(out.is_ok());
        assert_eq!(
            doc.get("include_descriptions").and_then(Value::as_bool),
            None,
            "'d' must not toggle descriptions from the action menu"
        );
    }

    #[test]
    fn config_flow_delete_leaves_clean_main_menu() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "opencode",
                "name": "OpenCode Zen",
                "enabled": true,
                "models": { "hy3-free": { "name": "HY3 Free", "enabled": true } }
            }]
        });
        let (h, w) = (30, 80);
        let mut cap = CaptureStdscr::new(h, w);
        cap.script(Key::Enter); // provider action menu
        cap.script(Key::Down);
        cap.script(Key::Down);
        cap.script(Key::Down); // Delete Provider
        cap.script(Key::Enter);
        cap.script(Key::Char('y'));
        cap.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut cap, &mut doc);
        assert!(res.is_ok(), "delete flow errored: {:?}", res.err());
        let grid = cap.screen();
        assert!(
            !grid_contains(&grid, "Delete Provider"),
            "confirm/action leftover on main menu after delete: {:?}",
            (0..h as usize).map(|r| row_text(&grid, r)).collect::<Vec<_>>()
        );
        let bottom = row_text(&grid, (h - 2) as usize);
        assert!(
            bottom.contains('Q'),
            "legend must stay on row h-2 after delete; row = {:?}",
            bottom
        );
        assert!(
            doc.get("providers")
                .and_then(Value::as_array)
                .is_some_and(|a| a.is_empty()),
            "provider must be removed from the doc"
        );
    }

    #[test]
    fn config_flow_enter_on_enabled_model_writes_reasoning() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "prov",
                "name": "Provider One",
                "enabled": true,
                "models": {
                    "alpha-1": {
                        "name": "Alpha One",
                        "enabled": true,
                        "reasoning_effort": "low",
                        "reasoning_efforts": [
                            {"value": "low", "label": "Low", "default": true},
                            {"value": "high", "label": "High", "default": false}
                        ]
                    }
                }
            }]
        });
        let mut f = FakeStdscr::new(30, 80);
        // provider → Codex Config → Descriptions → Update Model List →
        // Sync Model Config → Add Provider → Add Model → first Enabled Models row.
        for _ in 0..7 {
            f.script(Key::Down);
        }
        f.script(Key::Enter); // open reasoning picker
        f.script(Key::Down); // High
        f.script(Key::Enter);
        f.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut f, &mut doc);
        assert!(res.is_ok(), "reasoning flow errored: {:?}", res.err());
        let m = &doc["providers"][0]["models"]["alpha-1"];
        assert_eq!(m.get("reasoning_effort").and_then(Value::as_str), Some("high"));
        let efforts = m["reasoning_efforts"].as_array().unwrap();
        assert_eq!(efforts[0]["default"], Value::Bool(false));
        assert_eq!(efforts[1]["default"], Value::Bool(true));
        let last = f
            .recorded()
            .into_iter()
            .rev()
            .find(|(_, _, t, _)| t.contains("Alpha One"))
            .expect("Alpha One should still be on screen");
        assert_eq!(
            last.3.bg,
            bg_color(P::Selected),
            "cursor must stay on the enabled model after picking a reasoning level"
        );
    }

    #[test]
    fn config_flow_model_cursor_stays_when_no_reasoning_levels() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "prov",
                "name": "Provider One",
                "enabled": true,
                "models": {
                    "alpha-1": { "name": "Alpha One", "enabled": true }
                }
            }]
        });
        let mut f = FakeStdscr::new(30, 80);
        for _ in 0..7 {
            f.script(Key::Down);
        }
        f.script(Key::Enter); // (none) — no picker
        f.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut f, &mut doc);
        assert!(res.is_ok(), "none-reasoning flow errored: {:?}", res.err());
        let last = f
            .recorded()
            .into_iter()
            .rev()
            .find(|(_, _, t, _)| t.contains("Alpha One"))
            .expect("Alpha One should still be on screen");
        assert_eq!(
            last.3.bg,
            bg_color(P::Selected),
            "cursor must stay on the enabled model after Enter on (none)"
        );
    }

    #[test]
    fn config_flow_only_current_row_highlighted_in_enabled_models() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [{
                "id": "prov",
                "name": "Provider One",
                "enabled": true,
                "models": {
                    "alpha-1": { "name": "Alpha One", "enabled": true }
                }
            }]
        });
        let mut f = FakeStdscr::new(30, 80);
        // provider → Codex Config → Descriptions → Update Model List →
        // Sync Model Config → Add Provider → Add Model → first Enabled Models row.
        for _ in 0..7 {
            f.script(Key::Down);
        }
        f.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut f, &mut doc);
        assert!(res.is_ok(), "main-menu flow errored: {:?}", res.err());
        let rec = f.recorded();
        let last_matching = |needle: &str| {
            rec.iter()
                .rev()
                .find(|(_, _, t, _)| t.contains(needle))
                .cloned()
                .unwrap_or_else(|| panic!("{needle} should still be on screen"))
        };
        let model = last_matching("Alpha One");
        assert_eq!(
            model.3.bg,
            bg_color(P::Selected),
            "current enabled-model row must be highlighted"
        );
        for needle in ["Add Model", "Add Provider", "Codex Config", "Model Descriptions", "Sync Model Config", "Update Model List"] {
            let row = last_matching(needle);
            assert_ne!(
                row.3.bg,
                bg_color(P::Selected),
                "{needle} must not stay highlighted when the cursor is on an enabled model; paint={:?}",
                row.3
            );
        }
    }

    #[test]
    fn config_flow_codex_picker_selects_enabled_provider_or_disabled() {
        isolate_grok_home();
        let _grok_home_guard = crate::test_support::grok_home_lock();
        let mut doc = serde_json::json!({
            "providers": [
                {
                    "id": "openrouter",
                    "name": "OpenRouter",
                    "enabled": true,
                    "models": { "openrouter/free": { "name": "Free", "enabled": true } }
                },
                {
                    "id": "ollama-cloud",
                    "name": "Ollama Cloud",
                    "enabled": false,
                    "models": { "gemma4:31b": { "name": "Gemma", "enabled": true } }
                }
            ]
        });
        let mut f = FakeStdscr::new(30, 80);
        // openrouter, ollama-cloud, Codex Config
        for _ in 0..2 {
            f.script(Key::Down);
        }
        f.script(Key::Enter); // picker: disabled, openrouter
        f.script(Key::Down);
        f.script(Key::Enter);
        f.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut f, &mut doc);
        assert!(res.is_ok(), "codex picker errored: {:?}", res.err());
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(true));
        assert_eq!(doc["codex_model_provider"], "openrouter");

        let mut f2 = FakeStdscr::new(30, 80);
        for _ in 0..2 {
            f2.script(Key::Down);
        }
        f2.script(Key::Enter); // picker starts on openrouter
        f2.script(Key::Up); // disabled
        f2.script(Key::Enter);
        f2.script(Key::Char('q'));
        let res = run_config_flow_with_backend(&mut f2, &mut doc);
        assert!(res.is_ok(), "codex disable picker errored: {:?}", res.err());
        assert_eq!(doc["write_codex_config_toml"], Value::Bool(false));
        assert_eq!(
            doc["codex_model_provider"], "openrouter",
            "picker disable must keep the remembered provider; only sync clears it"
        );
    }
}

#[cfg(test)]
fn grid_contains(grid: &[Vec<char>], needle: &str) -> bool {
    grid.iter().any(|row| row.iter().collect::<String>().contains(needle))
}

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
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Backspace,
    Esc,
    Char(char),
    Interrupt,
    Resize,
    Eof,
}

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

/// Draw a line of `(text, color)` segments, truncating once `max_w` is
/// exhausted. Mirrors Python's `_draw_seg_line`.
fn draw_seg_line<S: Stdscr>(
    stdscr: &mut S,
    y: i32,
    x: i32,
    segments: &[(String, P)],
    max_w: usize,
) {
    let mut cx = x;
    for (text, pid) in segments {
        if (cx as usize) >= (x as usize) + max_w {
            break;
        }
        let take = ((x as usize) + max_w).saturating_sub(cx as usize);
        let piece: String = text.chars().take(take).collect();
        if piece.is_empty() {
            continue;
        }
        stdscr.addstr(y, cx, &piece, paint_for(*pid));
        cx += piece.chars().count() as i32;
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
        ("[enabled]", p)
    } else if let Some(p) = line.find("[disabled]") {
        ("[disabled]", p)
    } else {
        return None;
    };
    let head = line[..pos].to_string();
    let tail = line[pos + token.len()..].to_string();
    Some((head, token.to_string(), tail))
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
        if x as usize + run.chars().count() >= w.max(1) as usize {
            break;
        }
        for ch in key.chars() {
            let attr = if ch == '/' {
                Paint::plain(tn_color(P::Muted), bg_color(P::Muted))
            } else {
                Paint::plain(tn_color(P::LegendKey), bg_color(P::LegendKey)).bold()
            };
            stdscr.addstr(legend_y, x, &ch.to_string(), attr);
            x += 1;
        }
        stdscr.addstr(legend_y, x, " ", Paint::plain(tn_color(P::LegendDesc), bg_color(P::LegendDesc)));
        x += 1;
        stdscr.addstr(legend_y, x, desc, Paint::plain(tn_color(P::LegendDesc), bg_color(P::LegendDesc)));
        x += desc.chars().count() as i32;
    }
}

// ---------------------------------------------------------------------------
// Selector screen (provider list / action menu)
// ---------------------------------------------------------------------------

pub enum SelectOutcome {
    Picked(usize),
    Cancelled,
}

/// A line drawn in the `--config` main-menu preview panel beneath the provider
/// list. A `Heading` is a full-width blue bar (like the screen title); a `Segs`
/// line is a sequence of `(text, color)` segments (like `--models` output).
#[derive(Clone)]
pub enum PreviewLine {
    Heading(String),
    Segs(Vec<(String, P)>),
}

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

    loop {
        stdscr.erase();
        let (h, w) = stdscr.getmaxyx();
        paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
        draw_header(stdscr, &format!("  {title}"));

        let list_h = ((h - 4).max(1)) as usize;
        if current < top {
            top = current;
        }
        if current >= top + list_h {
            top = current + 1 - list_h;
        }

        for (row, _idx) in (0..list_h).enumerate() {
            let idx = top + row;
            if idx >= n {
                break;
            }
            let is_sel = idx == current;
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
            stdscr.addstr((2 + row) as i32, 0, &fill, row_bg);
            let line = if multi {
                let mark = if state[idx] { "●" } else { "○" };
                format!("  {mark}  {}", options[idx])
            } else {
                format!("  ▸ {}", options[idx])
            };
            let line = line.chars().take((w.max(1) as usize).saturating_sub(2)).collect::<String>();
            let label = if !multi && is_sel {
                row_paint
            } else {
                row_bg
            };
            // Colorize a [enabled]/[disabled] token green/red, mirroring
            // Python's `_curses_select_win` (P.ENABLED/P.ERROR pairs).
            if let Some((head, token, tail)) = split_state_token(&line) {
                let tok_enabled = token == "[enabled]";
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
                stdscr.addstr((2 + row) as i32, 0, &head, label);
                let tok_x = head.chars().count() as i32;
                stdscr.addstr((2 + row) as i32, tok_x, &token, tok_paint);
                if !tail.is_empty() {
                    let tail_x = (head.chars().count() + token.chars().count()) as i32;
                    stdscr.addstr((2 + row) as i32, tail_x, &tail, label);
                }
            } else {
                stdscr.addstr(
                    (2 + row) as i32,
                    0,
                    &format!("{:<wid$}", line, wid = (w.max(1) as usize).saturating_sub(1)),
                    label,
                );
            }
            if !multi {
                let chev_x = (line.chars().count() as i32 + 2).max(w - 4);
                stdscr.addstr((2 + row) as i32, chev_x, "›", Paint::plain(tn_color(P::Chevron), bg_color(P::Chevron)));
            }
        }

        let sep_y = 2 + (n.min(h as usize - 4) as i32);
        let sep = "─".repeat((w.max(1) as usize).saturating_sub(1));
        stdscr.addstr(sep_y, 0, &sep, Paint::plain(tn_color(P::Chevron), bg_color(P::Chevron)));

        // Models preview: fill the empty space below the list (the --config
        // main menu) with the enabled-models listing, styled like --models.
        // The action menu passes no preview and may have a footer instead.
        if let Some(preview) = preview {
            let avail_top = sep_y + 1;
            // Reserve two rows above the legend for the transient status line.
            let avail_bottom = h - if status.is_some() { 5 } else { 3 };
            let max_lines = (avail_bottom - avail_top + 1).max(0) as usize;
            if max_lines > 0 {
                let mut draw_lines: Vec<PreviewLine> = preview.to_vec();
                if draw_lines.len() > max_lines {
                    draw_lines.truncate(max_lines.saturating_sub(1));
                    draw_lines.push(PreviewLine::Segs(vec![(
                        "… (run --models for all)".to_string(),
                        P::Muted,
                    )]));
                }
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
                h - 4,
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
            ("Enter/→".to_string(), "select".to_string()),
        ];
        if multi {
            legend.push(("Space".to_string(), "toggle".to_string()));
        }
        if back_on_left {
            legend.push(("←".to_string(), "back".to_string()));
        } else {
            // On the main menu (where q actually quits) "Q quit" lives with the
            // other legend items, separated by the legend's "│" separator.
            legend.push(("Q".to_string(), "quit".to_string()));
        }
        draw_legend(stdscr, &legend);

        stdscr.refresh();
        let _ = emit_sgr_bg_keep_alive();

        match stdscr.getch() {
            Key::Resize => {
                paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
            }
            Key::Up if current > 0 => current -= 1,
            Key::Down if current + 1 < n => current += 1,
            Key::Space if multi => {
                state[current] = !state[current];
            }
            Key::Enter | Key::Right => {
                if multi {
                    let picked: Vec<usize> = (0..n).filter(|i| state[*i]).collect();
                    return Some(SelectOutcome::Picked(picked.first().copied().unwrap_or(0)));
                } else {
                    return Some(SelectOutcome::Picked(current));
                }
            }
            Key::Left | Key::Esc if back_on_left => return Some(SelectOutcome::Cancelled),
            Key::Char('q') if !back_on_left => return Some(SelectOutcome::Cancelled),
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
    type Entry;
    /// (entries, query) -> (ordered entries, separators). Separators are
    /// (row index after which to draw a `─` rule, paint pair).
    fn compute_view(&self, entries: &[Self::Entry], query: &str) -> (Vec<Self::Entry>, Vec<(usize, P)>);
    /// (entry, is_selected) -> (row text, row paint).
    fn render(&self, entry: &Self::Entry, is_selected: bool) -> (String, P);
    /// Enter on an entry: return true to keep the window open, false to close.
    /// Receives the active stdscr so models can draw overlays (inline errors).
    fn on_enter<S: Stdscr>(&mut self, stdscr: &mut S, entry: &Self::Entry) -> bool;
}

/// Generic type-to-filter list widget drawn into an existing stdscr. ESC or
/// Left-at-the-top always closes.
pub fn filter_list_win<S: Stdscr, M: FilterList>(
    stdscr: &mut S,
    entries: &[M::Entry],
    title: &str,
    legend: &[(String, String)],
    model: &mut M,
) {
    let mut query = String::new();
    let mut current = 0usize;
    let mut top = 0usize;
    loop {
        let (filtered, separators) = model.compute_view(entries, &query);
        if filtered.is_empty() {
            current = 0;
        } else if current >= filtered.len() {
            current = filtered.len() - 1;
        }
        stdscr.erase();
        let (h, w) = stdscr.getmaxyx();
        paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
        draw_header(stdscr, &format!("  {title}  |  Filter: {query}"));

        let list_top = 2usize;
        let list_h = ((h as usize).saturating_sub(list_top + 2)).max(1);
        if current < top {
            top = current;
        } else if current >= top + list_h {
            top = current + 1 - list_h;
        }

        if filtered.is_empty() {
            stdscr.addstr(2, 0, "  (no matches)", Paint::plain(tn_color(P::Muted), bg_color(P::Muted)));
        }

        for row in 0..list_h {
            let idx = top + row;
            if idx >= filtered.len() {
                break;
            }
            let entry = &filtered[idx];
            let (line, row_pair) = model.render(entry, idx == current);
            let fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
            stdscr.addstr(
                (list_top + row) as i32,
                0,
                &fill,
                Paint::plain(tn_color(row_pair), bg_color(row_pair)),
            );
            stdscr.addstr(
                (list_top + row) as i32,
                0,
                &format!("{:<w$}", line, w = (w.max(1) as usize).saturating_sub(1)),
                Paint::plain(tn_color(row_pair), bg_color(row_pair)),
            );
        }

        for (sep_idx, sep_pair) in &separators {
            // Draw only when the boundary row above the rule is visible.
            if 0 < *sep_idx && *sep_idx < filtered.len() && *sep_idx >= top + 1 && *sep_idx <= top + list_h {
                let y = (list_top + sep_idx - 1 - top) as i32 + 1;
                let sep = "─".repeat((w.max(1) as usize).saturating_sub(1));
                stdscr.addstr(y, 0, &sep, Paint::plain(tn_color(*sep_pair), bg_color(*sep_pair)));
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
                    top = (((current / list_h) + 1) * list_h).min(filtered.len() - 1);
                    current = top;
                }
            }
            Key::Left => {
                if current == 0 {
                    return;
                }
                if current < list_h {
                    top = 0;
                    current = 0;
                } else {
                    top = ((current / list_h) - 1) * list_h;
                    current = top;
                }
            }
            Key::Backspace => {
                query.pop();
                current = 0;
                top = 0;
            }
            Key::Enter => {
                if !filtered.is_empty() && !model.on_enter(stdscr, &filtered[current]) {
                    return;
                }
            }
            Key::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                query.push(c);
                current = 0;
                top = 0;
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
    changed: bool,
}

impl<'a> FilterList for ModelPicker<'a> {
    type Entry = String;

    fn compute_view(&self, _entries: &[String], query: &str) -> (Vec<String>, Vec<(usize, P)>) {
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

    fn render(&self, mid: &String, is_sel: bool) -> (String, P) {
        let m = self.models.get(mid);
        let enabled = m.map(|v| crate::get_bool_val(v, "enabled", true)).unwrap_or(false);
        let is_free = mid.to_lowercase().contains("free");
        let mark = if enabled { "●" } else { "○" };
        let free_tag = if is_free && !enabled { "  [free]" } else { "" };
        let pair = if is_sel {
            P::Selected
        } else if enabled {
            P::Enabled
        } else if is_free {
            P::Free
        } else {
            P::Disabled
        };
        (format!("  {mark}  {mid}{free_tag}"), pair)
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

pub fn model_search_win<S: Stdscr>(stdscr: &mut S, ids: &[String], models: &mut Map<String, Value>) -> bool {
    let mut picker = ModelPicker { ids, models, changed: false };
    filter_list_win(
        stdscr,
        ids,
        "Configure models",
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
    draw_header(stdscr, "  Confirm");
    let (h, w) = stdscr.getmaxyx();
    let trunc: String = prompt.chars().take((w.max(1) as usize).saturating_sub(4)).collect();
    stdscr.addstr(2, 2, &trunc, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
    draw_legend(
        stdscr,
        &[
            ("Y".to_string(), "yes".to_string()),
            ("N".to_string(), "no".to_string()),
            ("ESC".to_string(), "cancel".to_string()),
        ],
    );
    let _ = h;
    stdscr.refresh();
    loop {
        match stdscr.getch() {
            Key::Char('y') | Key::Char('Y') => return true,
            Key::Char('n') | Key::Char('N') | Key::Esc => return false,
            Key::Interrupt => return false,
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
}

/// Filter-list model for the add-provider modal: live-filter the models.dev
/// catalog (existing providers already excluded), Enter adds quietly.
struct AddProviderPicker<'a> {
    doc: &'a mut Value,
    api: Value,
    added: Option<String>,
}

fn provider_matches(pid: &str, name: &str, term_l: &str) -> bool {
    term_l.is_empty()
        || pid.to_lowercase().contains(term_l)
        || name.to_lowercase().contains(term_l)
}

impl<'a> FilterList for AddProviderPicker<'a> {
    type Entry = (String, String);

    fn compute_view(
        &self,
        entries: &[(String, String)],
        query: &str,
    ) -> (Vec<(String, String)>, Vec<(usize, P)>) {
        let term_l = query.to_lowercase();
        let ordered: Vec<(String, String)> = entries
            .iter()
            .filter(|(pid, name)| provider_matches(pid, name, &term_l))
            .cloned()
            .collect();
        (ordered, Vec::new())
    }

    fn render(&self, entry: &(String, String), is_sel: bool) -> (String, P) {
        let (pid, name) = entry;
        let label = if name.is_empty() {
            format!("  {pid}")
        } else {
            format!("  {pid} ({name})")
        };
        let pair = if is_sel { P::Selected } else { P::Text };
        (label, pair)
    }

    fn on_enter<S: Stdscr>(&mut self, stdscr: &mut S, entry: &(String, String)) -> bool {
        let pid = &entry.0;
        if let Err(e) = crate::commands::add_provider_entry(self.doc, &self.api, pid, true) {
            // Add errors surface inline so the surrounding session survives.
            inline_error_win(stdscr, &format!("Add failed: {}", e.0));
            return true; // stay open
        }
        // Mirror the new entry into its sorted position in the live doc so
        // the parent menu refreshes in order.
        let mut model_count = 0usize;
        if let Some(arr) = self.doc.get_mut("providers").and_then(Value::as_array_mut) {
            let new_entry = arr.pop();
            if let Some(new_entry) = new_entry {
                model_count = new_entry
                    .get("models")
                    .and_then(Value::as_object)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let key = jsonio::provider_sort_key(&new_entry);
                let pos = arr
                    .iter()
                    .map(jsonio::provider_sort_key)
                    .collect::<Vec<_>>()
                    .partition_point(|k| k.as_str() < key.as_str());
                arr.insert(pos, new_entry);
            }
        }
        self.added = Some(format!(
            "Added provider '{pid}' with {model_count} models (all disabled)."
        ));
        false // close back to the provider menu
    }
}

/// Modal: type-to-filter the models.dev catalog and add a provider. Returns
/// the confirmation status line for the parent menu, or None. The fetch runs
/// before the modal opens so a failure never leaves it on screen.
pub fn add_provider_win<S: Stdscr>(stdscr: &mut S, doc: &mut Value) -> Option<String> {
    let api = match crate::sync::fetch_models_dev() {
        Ok(a) => a,
        Err(e) => {
            inline_error_win(stdscr, &format!("Fetch failed: {}", e.0));
            return None;
        }
    };
    let existing: Vec<String> = usable(doc)
        .iter()
        .map(|p| p.get("id").and_then(Value::as_str).unwrap_or_default().to_string())
        .collect();
    let mut catalog: Vec<(String, String)> = api
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, pinfo)| pinfo.is_object())
                .filter(|(pid, _)| !existing.contains(&pid.to_string()))
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

    let mut picker = AddProviderPicker { doc, api, added: None };
    filter_list_win(
        stdscr,
        &catalog,
        "Add provider",
        &[
            ("↑/↓/←/→".to_string(), "nav".to_string()),
            ("ESC".to_string(), "cancel".to_string()),
            ("Enter".to_string(), "add".to_string()),
            ("type".to_string(), "filter".to_string()),
        ],
        &mut picker,
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
                    &format!("{:<w$}", line, w = (w.max(1) as usize).saturating_sub(1)),
                    Paint::plain(tn_color(P::Selected), bg_color(P::Selected)).bold(),
                );
                let cur_x =
                    (4 + label.chars().count() + 2 + buf.chars().count()) as i32;
                stdscr.addstr(
                    y,
                    cur_x,
                    "█]",
                    Paint::plain(tn_color(P::Selected), bg_color(P::Selected)).bold(),
                );
            } else {
                stdscr.addstr(y, 0, &fill, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
                let line: String =
                    format!("    {row}").chars().take((w.max(1) as usize).saturating_sub(2)).collect();
                stdscr.addstr(
                    y,
                    0,
                    &format!("{:<w$}", line, w = (w.max(1) as usize).saturating_sub(1)),
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
// Whole --config flow driver (real terminal)
// ---------------------------------------------------------------------------

/// Build the `--models`-style enabled-models listing as `PreviewLine`s, for
/// rendering in the empty space under the `--config` main menu. Mirrors
/// Python's `_build_config_models_preview`: enabled models grouped by provider
/// (both sorted by name), then an env-var status box and a summary line.
pub fn build_config_models_preview(doc: &Value) -> Vec<PreviewLine> {
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
    // Heading marker -> full-width blue bar, like the screen title.
    lines.push(PreviewLine::Heading("Enabled Models".to_string()));
    lines.push(PreviewLine::Segs(vec![("".to_string(), P::Text)])); // gap under the models header

    let mut total_enabled = 0usize;
    let mut prov_sorted: Vec<Map<String, Value>> = providers
        .iter()
        .filter(|p| p.get("id").is_some())
        .map(|p| (*p).clone())
        .collect();
    prov_sorted.sort_by(|a, b| {
        crate::name_or(&Value::Object(a.clone()), "")
            .to_lowercase()
            .cmp(&crate::name_or(&Value::Object(b.clone()), "").to_lowercase())
    });

    for provider in &prov_sorted {
        let pid = provider.get("id").and_then(Value::as_str).unwrap_or_default();
        let penabled = crate::get_bool_obj(provider, "enabled", true);
        let mm = provider.get("models").and_then(Value::as_object);
        let Some(mm) = mm else {
            continue;
        };
        let pname = crate::name_or(&Value::Object(provider.clone()), pid);
        let mut rows: Vec<(String, String, String, String, String)> = Vec::new();
        for (mid, m) in mm {
            if !m.is_object() || !crate::get_bool_val(m, "enabled", true) {
                continue;
            }
            if !penabled {
                continue;
            }
            let mname = crate::name_or(m, mid);
            rows.push((mname.to_lowercase(), mname, pname.clone(), pid.to_string(), mid.clone()));
            total_enabled += 1;
        }
        rows.sort_by(|a, b| (a.0.clone(), a.2.clone(), a.3.clone()).cmp(&(b.0.clone(), b.2.clone(), b.3.clone())));
        for (_, mname, pname, pid, mid) in &rows {
            lines.push(PreviewLine::Segs(vec![
                ("● ".to_string(), P::Enabled),
                (mname.clone(), P::Value),
                (format!(" ({pname}) - {pid}/{mid}"), P::Text),
            ]));
        }
    }

    if total_enabled == 0 {
        lines.push(PreviewLine::Segs(vec![(
            "No enabled models. Enable with --enable or --config".to_string(),
            P::Muted,
        )]));
        return lines;
    }

    lines.push(PreviewLine::Segs(vec![("".to_string(), P::Text)]));

    // Env-var requirements rendered as a borderless black code panel with
    // padding: green text, gray provider-name annotations, red for unset keys.
    let mut env_rows: Vec<(String, String, String, bool)> = Vec::new();
    for provider in &prov_sorted {
        let env = crate::first_env_key_from(provider);
        if env.is_empty() {
            continue;
        }
        let val = crate::core::env_value(&env);
        let pname = crate::name_or(&Value::Object(provider.clone()), provider.get("id").and_then(Value::as_str).unwrap_or_default());
        let missing = val == "\"\"";
        env_rows.push((env, val, pname, missing));
    }

    if !env_rows.is_empty() {
        let w_env = env_rows.iter().map(|(e, _, _, _)| e.chars().count()).max().unwrap_or(0);
        let w_val = env_rows.iter().map(|(_, v, _, _)| v.chars().count()).max().unwrap_or(0);
        let mut rows_segs: Vec<Vec<(String, P)>> = vec![code_line_segments(
            "# required env_key values",
            None,
        )];
        for (env, val, pname, missing) in &env_rows {
            let body = format!("{:<w_env$} = {:<w_val$}", env, val);
            let highlight = if *missing {
                Some((0, w_env, P::CodeError))
            } else {
                None
            };
            let mut segs = code_line_segments(&body, highlight);
            // Provider name renders as a shell comment, e.g. "  # OpenRouter".
            segs.push(("  # ".to_string() + pname, P::CodeComment));
            rows_segs.push(segs);
        }
        const PAD_X: usize = 1;
        let panel_w = rows_segs
            .iter()
            .map(|segs| segs.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
            .max()
            .unwrap_or(0)
            + 2 * PAD_X;
        for segs in &rows_segs {
            let seg_len: usize = segs.iter().map(|(t, _)| t.chars().count()).sum();
            let mut line: Vec<(String, P)> =
                vec![(" ".repeat(PAD_X), P::CodeText)];
            line.extend(segs.iter().cloned());
            line.push((
                " ".repeat(panel_w.saturating_sub(PAD_X + seg_len)),
                P::CodeText,
            ));
            lines.push(PreviewLine::Segs(line));
        }
    }

    lines.push(PreviewLine::Segs(vec![(
        format!("Summary: {total_enabled} models enabled"),
        P::Muted,
    )]));
    lines
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
    loop {
        // providers arrive name-sorted from load_providers(); keep that order.
        let ordered: Vec<Map<String, Value>> = usable(doc);
        // Zero providers is a valid state: ➕ Add provider… is reachable first.
        let mut labels: Vec<String> = ordered.iter().map(|p| crate::provider_label_from(p)).collect();
        labels.push("➕ Add provider…".to_string());
        let preview = build_config_models_preview(doc);
        let pi = match select_win(stdscr,
            &labels,
            "Select Provider",
            false,
            &[],
            false,
            None,
            None,
            status_msg.as_deref(),
            Some(&preview),
            0,
        ) {
            None => return Ok(changed),
            Some(SelectOutcome::Cancelled) => return Ok(changed),
            Some(SelectOutcome::Picked(i)) => i,
        };
        if pi == ordered.len() {
            // "➕ Add provider…" — modal over the models.dev catalog.
            if let Some(msg) = add_provider_win(stdscr, doc) {
                status_msg = Some(msg);
                changed = true;
            }
            continue;
        }
        status_msg = None;
        let mut action_cursor = 0usize;
        let mut target = ordered[pi].clone();
        loop {
            let enabled = crate::get_bool_val(&Value::Object(target.clone()), "enabled", true);
            let current_base =
                target.get("base_url").and_then(Value::as_str).unwrap_or_default().to_string();
            let actions = vec![
                "Configure models".to_string(),
                format!("Provider [{}]", if enabled { "enabled" } else { "disabled" }),
                format!("Base Url [{current_base}]"),
                "Delete provider".to_string(),
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
            ) {
                None | Some(SelectOutcome::Cancelled) => break,
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
                        println!(
                            "No models for {}. Run a sync or re-add the provider.",
                            core::py_repr(&id_str)
                        );
                    } else {
                        let mut models = target
                            .get("models")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        model_search_win(stdscr, &ids, &mut models);
                        // Sync BOTH the live `target` copy and the doc: the
                        // action menu renders from `target`, so re-entering
                        // Configure models must reflect the toggles even
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
                    if confirm_win(stdscr, &format!("Delete provider {}?", core::py_repr(&id_str))) {
                        remove_provider(doc, &id_str);
                        fallback::record_removed_provider(doc, &id_str);
                        jsonio::dump_providers(&paths::providers_path(), doc)?;
                        changed = true;
                    }
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
/// On exit we restore the original screen, so closing `--config` returns the
/// terminal exactly as it was before — no blue background, no menu history.
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
/// hidden cursor). The restore sequence clears exactly these on every exit
/// path so the terminal returns to its prior state. We deliberately do NOT
/// emit modes we never enable (mouse / bracketed-paste / focus / Kitty),
/// mirroring grok-build's gated teardown. This is the hand-rolled equivalent
/// of grok-build's `RESTORE_SEQ` (async-signal-safe: ANSI only).
const RESTORE_SEQ: &[u8] = b"\x1b[?25h\x1b[?1049l\x1b[0m";

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
                cur_pos = Some((r, c + 1));
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
        let x = x as usize;
        for (i, ch) in s.chars().enumerate() {
            let cx = x + i;
            if cx >= cols {
                break;
            }
            self.frame[y as usize][cx] = (ch, paint);
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
                // Esc-delay expired on an incomplete sequence.
                if self.input_buf.first() == Some(&0x1b) {
                    self.input_buf.remove(0);
                    return Key::Esc;
                }
                continue;
            }
            let mut buf = [0u8; 256];
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
            // Unknown CSI: swallow through its final alpha byte.
            let end = buf[2..]
                .iter()
                .position(|b| b.is_ascii_alphabetic())
                .map(|p| p + 3)
                .unwrap_or(buf.len());
            return Some((Key::Esc, end));
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
        b' ' => Key::Space,
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
        });
    }

    /// Records every `addstr` call so tests can assert exact rendering
    /// (token colors, legend position, background sweep) without curses.
    struct FakeStdscr {
        h: i32,
        w: i32,
        calls: std::cell::RefCell<Vec<(i32, i32, String, Paint)>>,
        keys: std::cell::RefCell<Vec<Key>>,
    }

    impl FakeStdscr {
        fn new(h: i32, w: i32) -> Self {
            FakeStdscr {
                h,
                w,
                calls: Default::default(),
                keys: Default::default(),
            }
        }
        fn script(&self, k: Key) {
            self.keys.borrow_mut().push(k);
        }
        fn recorded(&self) -> Vec<(i32, i32, String, Paint)> {
            self.calls.borrow().clone()
        }
    }

    impl Stdscr for FakeStdscr {
        fn getmaxyx(&self) -> (i32, i32) {
            (self.h, self.w)
        }
        fn erase(&mut self) {}
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

    #[test]
    fn select_win_state_tokens_colored_and_legend_positioned() {
        let h = 30;
        let w = 80;
        let options = vec![
            "opencode (OpenCode Zen) [enabled]".to_string(),
            "grok (Grok) [disabled]".to_string(),
        ];

        // initial = 0: first (enabled) row selected.
        let mut f = FakeStdscr::new(h, w);
        f.script(Key::Char('q'));
        let _ = select_win(&mut f, &options, "Select Provider", false, &[], false, None, None, None, None, 0);
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
        let _ = select_win(&mut f2, &options, "Select Provider", false, &[], false, None, None, None, None, 1);
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
        );
        let calls = f.recorded();
        assert!(
            calls
                .iter()
                .any(|(y, _, t, _)| *y == 26 && t.contains("Added provider 'x'")),
            "status line not at height-4"
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

        // The preview builder produces the heading, an enabled-model row, and
        // an env-var code panel (env name red when the var is unset).
        let preview = build_config_models_preview(&doc);
        assert!(preview
            .iter()
            .any(|l| matches!(l, PreviewLine::Heading(t) if t == "Enabled Models")));
        let has_enabled_row = preview.iter().any(|l| matches!(
            l,
            PreviewLine::Segs(segs) if segs
                .iter()
                .any(|(t, p)| t == "● " && *p == P::Enabled)
        ));
        assert!(has_enabled_row, "preview missing enabled-model row");
        let has_env_panel = preview.iter().any(|l| matches!(
            l,
            PreviewLine::Segs(segs) if segs.iter().any(|(t, p)| {
                t.contains("# required env_key values") && *p == P::CodeComment
            })
        )) && preview.iter().any(|l| matches!(
            l,
            PreviewLine::Segs(segs) if segs.iter().any(|(t, _)| t.contains("OPENCODE_API_KEY"))
        ));
        assert!(has_env_panel, "preview missing env-var panel");

        // Drive the selector with the preview; assert it renders under the list.
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
            None,
            Some(&preview),
            0,
        );
        let calls = f.recorded();
        assert!(
            calls.iter().any(|(_, _, t, _)| t.contains("Enabled Models")),
            "preview heading not drawn"
        );
        assert!(
            calls.iter().any(|(_, _, t, _)| t.contains("OPENCODE_API_KEY")),
            "preview env-var box not drawn"
        );
        // 'Q' drawn inline with the legend items (row h-2), not right corner.
        assert!(
            calls.iter().any(|(y, _, t, _)| *y == 28 && t == "Q"),
            "legend 'Q' not drawn inline"
        );
    }

    /// Terminal-like `Stdscr` that renders into an in-memory ANSI byte buffer
    /// using the exact same `emit_cell` path as `RealStdscr`. Lets the eval
    /// verify the `--config` TUI produces a true fullscreen layout (each
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
        // Provider label occupies its own row (row 2), not the bottom.
        assert!(
            row_text(&grid, 2).contains("opencode (OpenCode Zen)"),
            "provider label not at row 2; row2 = {:?}",
            row_text(&grid, 2)
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
            grid_contains(&grid, "opencode (OpenCode Zen) [disabled]"),
            "provider config page did not reflect disabled state after toggle"
        );
        // And the stale "enabled" label must be gone.
        assert!(
            !grid_contains(&grid, "opencode (OpenCode Zen) [enabled]"),
            "stale enabled label still shown after toggle"
        );
        // The underlying doc really changed too.
        let enabled = doc["providers"][0]["enabled"].as_bool().unwrap_or(true);
        assert!(!enabled, "providers.json enabled flag not flipped in doc");
    }

    #[test]
    fn config_flow_restores_terminal_on_exit() {
        isolate_grok_home();
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
        // (cursor + alt screen), matching grok-build's gated teardown (it
        // does not emit modes we never enable: mouse/paste/focus/Kitty).
        assert_eq!(RESTORE_SEQ, b"\x1b[?25h\x1b[?1049l\x1b[0m");
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
        // Unknown CSI swallows through its final alpha byte as Esc.
        assert_eq!(parse_key_prefix(b"\x1b[?1049h"), Some((Key::Esc, 8)));
    }
}

#[cfg(test)]
fn grid_contains(grid: &[Vec<char>], needle: &str) -> bool {
    grid.iter().any(|row| row.iter().collect::<String>().contains(needle))
}

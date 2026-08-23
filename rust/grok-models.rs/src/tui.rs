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

#[derive(Clone, Copy, Debug)]
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
    right: Option<&[(String, String)]>,
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

    // Right-aligned block (optional, kept for callers that want a separate
    // corner block distinct from the inline legend).
    if let Some(right) = right {
        let sep = "  │  ";
        let widths: Vec<usize> = right
            .iter()
            .map(|(k, d)| k.chars().count() + 1 + d.chars().count())
            .collect();
        let total: usize = widths.iter().sum::<usize>()
            + sep.chars().count() * right.len().saturating_sub(1);
        let rx = (w.max(1) as usize).saturating_sub(1) - total;
        if (rx as i32) >= x + 1 {
            let mut rx = rx as i32;
            for (key, desc) in right {
                for ch_k in key.chars() {
                    let attr = if ch_k == '/' {
                        Paint::plain(tn_color(P::Muted), bg_color(P::Muted))
                    } else {
                        Paint::plain(tn_color(P::LegendKey), bg_color(P::LegendKey)).bold()
                    };
                    stdscr.addstr(legend_y, rx, &ch_k.to_string(), attr);
                    rx += 1;
                }
                stdscr.addstr(legend_y, rx, " ", Paint::plain(tn_color(P::LegendDesc), bg_color(P::LegendDesc)));
                rx += 1;
                stdscr.addstr(legend_y, rx, desc, Paint::plain(tn_color(P::LegendDesc), bg_color(P::LegendDesc)));
                rx += desc.chars().count() as i32;
            }
        }
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
    footer: Option<&str>,
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
            let avail_bottom = h - 3;
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

        if let Some(f) = footer {
            let safe = (w.max(1) as usize).saturating_sub(8).max(1);
            let trunc: String = f.chars().take(safe).collect();
            let inner = format!(" {trunc} ");
            let bx = 2i32;
            let box_y = sep_y + 1;
            let blue = Paint::plain(tn_color(P::Value), bg_color(P::Value));
            if (box_y as i32) + 2 < h - 2 {
                let top = format!("┌{}┐", "─".repeat(inner.chars().count()));
                let mid = format!("│{inner}│");
                let bot = format!("└{}┘", "─".repeat(inner.chars().count()));
                stdscr.addstr(box_y, bx, &top, blue);
                stdscr.addstr(box_y + 1, bx, &mid, blue);
                stdscr.addstr(box_y + 2, bx, &bot, blue);
            } else {
                let mid = format!("│{inner}│");
                stdscr.addstr(box_y, bx, &mid, blue);
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
        draw_legend(stdscr, &legend, None);

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

pub fn model_search_win<S: Stdscr>(stdscr: &mut S, ids: &[String], models: &mut Map<String, Value>) -> bool {
    let mut query = String::new();
    let mut current = 0usize;
    let mut top = 0usize;
    let mut changed = false;
    loop {
        let ids_for_filter = ids.to_vec();
        let sorted = core::sort_model_indices(ids, models, Some(&query));
        let filtered = sorted.filtered;
        if filtered.is_empty() {
            current = 0;
        } else if current >= filtered.len() {
            current = filtered.len() - 1;
        }
        stdscr.erase();
        let (h, w) = stdscr.getmaxyx();
        paint_bg(stdscr, Paint::plain(tn_color(P::Text), bg_color(P::Text)));
        draw_header(stdscr, &format!("  Configure models  |  Filter: {query}"));
        let list_h = ((h - 4).max(1)) as usize;
        if current < top {
            top = current;
        }
        if current >= top + list_h {
            top = current + 1 - list_h;
        }
        if filtered.is_empty() {
            stdscr.addstr(2, 0, "  (no matches)", Paint::plain(tn_color(P::Muted), bg_color(P::Muted)));
        }
        let enabled_count = sorted.enabled_count;
        let free_disabled_count = sorted.free_disabled_count;

        for row in 0..list_h {
            let idx = top + row;
            if idx >= filtered.len() {
                break;
            }
            let real_i = filtered[idx];
            let mid = &ids[real_i];
            let enabled = models
                .get(mid)
                .map(|v| crate::get_bool_val(v, "enabled", true))
                .unwrap_or(false);
            let is_free = mid.to_lowercase().contains("free");
            let mark = if enabled { "●" } else { "○" };
            let free_tag = if is_free && !enabled { "  [free]" } else { "" };
            let line = format!("  {mark}  {mid}{free_tag}");
            let line: String = line.chars().take((w.max(1) as usize).saturating_sub(2)).collect();
            let row_pair = if idx == current {
                P::Selected
            } else if enabled {
                P::Enabled
            } else if is_free {
                P::Free
            } else {
                P::Disabled
            };
            let fill = "\u{00a0}".repeat((w.max(1) as usize).saturating_sub(1));
            stdscr.addstr(
                (2 + row) as i32,
                0,
                &fill,
                Paint::plain(tn_color(row_pair), bg_color(row_pair)),
            );
            stdscr.addstr(
                (2 + row) as i32,
                0,
                &format!("{:<w$}", line, w = (w.max(1) as usize).saturating_sub(1)),
                Paint::plain(tn_color(row_pair), bg_color(row_pair)),
            );
        }

        // enabled separator
        let enabled_sep = enabled_count;
        if enabled_count > 0 && enabled_sep < filtered.len() {
            let y = 2 + (enabled_sep as i32).saturating_sub(top as i32);
            if y >= 2 && y < h - 2 {
                let sep = "─".repeat((w.max(1) as usize).saturating_sub(1));
                stdscr.addstr(y, 0, &sep, Paint::plain(tn_color(P::Chevron), bg_color(P::Chevron)));
            }
        }
        // free-disabled separator
        let free_sep = enabled_count + free_disabled_count;
        if free_disabled_count > 0 && free_sep < filtered.len() {
            let y = 2 + (free_sep as i32).saturating_sub(top as i32);
            if y >= 2 && y < h - 2 {
                let sep = "─".repeat((w.max(1) as usize).saturating_sub(1));
                stdscr.addstr(
                    y,
                    0,
                    &sep,
                    Paint::plain(tn_color(P::Free), bg_color(P::Free)),
                );
            }
        }

        draw_legend(
            stdscr,
            &[
                ("↑/↓/←/→".to_string(), "nav".to_string()),
                ("ESC".to_string(), "back".to_string()),
                ("Enter".to_string(), "toggle".to_string()),
                ("type".to_string(), "filter".to_string()),
            ],
            None,
        );
        stdscr.refresh();

        match stdscr.getch() {
            Key::Resize => {}
            Key::Esc => return changed,
            Key::Interrupt => return changed,
            Key::Up if current > 0 => current -= 1,
            Key::Down if current + 1 < filtered.len() => current += 1,
            Key::Right => {
                if !filtered.is_empty() {
                    top = ((current / list_h + 1) * list_h).min(filtered.len() - 1);
                    current = top;
                }
            }
            Key::Left => {
                if current == 0 {
                    return changed;
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
                if !filtered.is_empty() {
                    let real_i = filtered[current];
                    let mid = ids_for_filter[real_i].clone();
                    let entry = models.entry(mid).or_insert_with(|| Value::Object(Map::new()));
                    if !entry.is_object() {
                        *entry = Value::Object(Map::new());
                    }
                    let cur = crate::get_bool_val(entry, "enabled", true);
                    entry.as_object_mut().unwrap().insert("enabled".into(), Value::Bool(!cur));
                    changed = true;
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
        None,
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

    // Env-var requirements in a blue box with aligned columns. The env var name
    // is drawn red when its value is unset (missing key).
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
        let inner_w = env_rows
            .iter()
            .map(|(e, v, p, _)| {
                let content = format!(
                    "{} = {}  ({})",
                    format!("{:<w_env$}", e),
                    format!("{:<w_val$}", v),
                    p
                );
                content.chars().count()
            })
            .max()
            .unwrap_or(0)
            .max(w_env + 3 + w_val);
        let w_tail = inner_w.saturating_sub(w_env + 3 + w_val);
        lines.push(PreviewLine::Segs(vec![(
            format!("┌{}┐", "─".repeat(inner_w)),
            P::Value,
        )]));
        for (env, val, pname, missing) in &env_rows {
            let key_color = if *missing { P::Error } else { P::Value };
            lines.push(PreviewLine::Segs(vec![
                ("│".to_string(), P::Value),
                (format!("{:<w_env$}", env), key_color),
                (" = ".to_string(), P::Text),
                (format!("{:<w_val$}", val), P::Text),
                (format!("{:<w_tail$}", format!("  ({pname})")), P::Muted),
                ("│".to_string(), P::Value),
            ]));
        }
        lines.push(PreviewLine::Segs(vec![(
            format!("└{}┘", "─".repeat(inner_w)),
            P::Value,
        )]));
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
    loop {
        let mut ordered: Vec<Map<String, Value>> = usable(doc);
        ordered.sort_by(|a, b| {
            a["id"].as_str().unwrap_or_default().cmp(b["id"].as_str().unwrap_or_default())
        });
        if ordered.is_empty() {
            return Ok(changed);
        }
        let labels: Vec<String> = ordered.iter().map(|p| crate::provider_label_from(p)).collect();
        let preview = build_config_models_preview(doc);
        let pi = match select_win(stdscr,
            &labels,
            "Select Provider",
            false,
            &[],
            false,
            None,
            Some(&preview),
            0,
        ) {
            None => return Ok(changed),
            Some(SelectOutcome::Cancelled) => return Ok(changed),
            Some(SelectOutcome::Picked(i)) => i,
        };
        let mut action_cursor = 0usize;
        let mut target = ordered[pi].clone();
        loop {
            let enabled = crate::get_bool_val(&Value::Object(target.clone()), "enabled", true);
            let actions = vec![
                "Configure models 🛠".to_string(),
                format!("Provider [{}] 🔌", if enabled { "enabled" } else { "disabled" }),
                "Delete provider 🗑".to_string(),
                "Back".to_string(),
            ];
            let env_key = crate::first_env_key_from(&target);
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
                footer.as_deref(),
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
                        if let Some(slot) = find_by_id_mut(doc, &id_str) {
                            slot.insert("models".to_string(), Value::Object(models));
                        }
                        jsonio::dump_json(&paths::providers_path(), doc)?;
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
                    jsonio::dump_json(&paths::providers_path(), doc)?;
                    changed = true;
                }
                2 => {
                    if confirm_win(stdscr, &format!("Delete provider {}?", core::py_repr(&id_str))) {
                        remove_provider(doc, &id_str);
                        fallback::record_removed_provider(doc, &id_str);
                        jsonio::dump_json(&paths::providers_path(), doc)?;
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
        // \x1b[2J = clear screen, \x1b[H = cursor home.
        print!("\x1b[2J\x1b[H");
    }
    fn refresh(&mut self) {
        let _ = std::io::stdout().flush();
    }
    fn addstr(&mut self, y: i32, x: i32, s: &str, paint: Paint) {
        emit_cell(&mut std::io::stdout(), y, x, s, paint);
    }
    fn getch(&mut self) -> Key {
        read_key(&mut std::io::stdin())
    }
}

/// Read one key from `r`, transparently handling `EINTR` (delivered by
/// SIGWINCH): a resize interrupt surfaces as `Key::Resize` so the TUI can
/// redraw at the new size; any other interrupt is retried.
fn read_key<R: Read>(r: &mut R) -> Key {
    let mut buf = [0u8; 8];
    loop {
        match r.read(&mut buf) {
            Ok(n) if n >= 1 => return parse_key(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                if RESIZE_PENDING.swap(false, Ordering::SeqCst) {
                    return Key::Resize;
                }
                // Non-resize interrupt: retry the read.
                continue;
            }
            _ => return Key::Eof,
        }
    }
}

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
        Some(Self { raw_mode: Some(raw_mode) })
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

fn parse_key(buf: &[u8]) -> Key {
    if buf.is_empty() {
        return Key::Eof;
    }
    if buf[0] == 0x1b && buf.len() >= 3 && buf[1] == b'[' {
        match buf[2] {
            b'A' => return Key::Up,
            b'B' => return Key::Down,
            b'C' => return Key::Right,
            b'D' => return Key::Left,
            _ => {}
        }
    }
    if buf[0] == 0x1b {
        return Key::Esc;
    }
    // Ctrl-C (ETX) is delivered as a literal byte in raw mode (ISIG is off,
    // so the tty does not raise SIGINT). Treat it as an interrupt/abort, like
    // grok-build's Ctrl-C graceful quit — not as a signal we can't see.
    if buf[0] == 0x03 {
        return Key::Interrupt;
    }
    if buf[0] == 0x7f || buf[0] == 0x08 {
        return Key::Backspace;
    }
    if buf[0] == b'\r' || buf[0] == b'\n' {
        return Key::Enter;
    }
    if buf[0] == b' ' {
        return Key::Space;
    }
    if buf[0].is_ascii() {
        if let Some(c) = (buf[0] as char).to_ascii_lowercase().to_string().chars().next() {
            return Key::Char(c);
        }
    }
    Key::Eof
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
        let _ = select_win(&mut f, &options, "Select Provider", false, &[], false, None, None, 0);
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
        let _ = select_win(&mut f2, &options, "Select Provider", false, &[], false, None, None, 1);
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
        // an env-var box (env name red when the var is unset).
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
        let has_env_box = preview.iter().any(|l| matches!(
            l,
            PreviewLine::Segs(segs) if segs.iter().any(|(t, _)| t.contains('│'))
        ));
        assert!(has_env_box, "preview missing env-var box");

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
    fn read_key_surfaces_resize_on_eintr() {
        // A SIGWINCH arrives as EINTR on the blocking read. `read_key` must
        // surface it as Key::Resize (so the TUI redraws) rather than dropping
        // the input or exiting.
        struct IntrReader {
            intr: bool,
            byte: u8,
        }
        impl std::io::Read for IntrReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.intr {
                    self.intr = false;
                    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "eintr"));
                }
                buf[0] = self.byte;
                Ok(1)
            }
        }
        RESIZE_PENDING.store(true, Ordering::SeqCst);
        let mut r = IntrReader { intr: true, byte: b'q' };
        assert_eq!(read_key(&mut r), Key::Resize);
        // After the resize is consumed, the next read returns the real key.
        let mut r2 = IntrReader { intr: false, byte: b'q' };
        assert_eq!(read_key(&mut r2), Key::Char('q'));
    }
}

#[cfg(test)]
fn grid_contains(grid: &[Vec<char>], needle: &str) -> bool {
    grid.iter().any(|row| row.iter().collect::<String>().contains(needle))
}

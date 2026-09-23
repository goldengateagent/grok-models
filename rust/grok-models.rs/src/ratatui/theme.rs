//! Tokyo Night colors for the Ratatui UI. Values come from `crate::theme`.

use crate::theme::{self, Rgb};
use ::ratatui::style::{Color, Modifier, Style};

pub(crate) fn c(rgb: Rgb) -> Color {
    Color::Rgb(rgb.r, rgb.g, rgb.b)
}

pub(crate) fn bg() -> Color {
    c(theme::tn(0))
}
pub(crate) fn bg_dark() -> Color {
    c(theme::tn(1))
}
pub(crate) fn bg_visual() -> Color {
    c(theme::tn(3))
}
pub(crate) fn fg() -> Color {
    c(theme::tn(4))
}
pub(crate) fn fg_dark() -> Color {
    c(theme::tn(5))
}
pub(crate) fn muted() -> Color {
    c(theme::tn(6))
}
pub(crate) fn chevron() -> Color {
    c(theme::tn(7))
}
pub(crate) fn blue() -> Color {
    c(theme::accent(0))
}
pub(crate) fn cyan() -> Color {
    c(theme::accent(1))
}
pub(crate) fn green() -> Color {
    c(theme::accent(2))
}
pub(crate) fn red() -> Color {
    c(Rgb {
        r: theme::RED.0,
        g: theme::RED.1,
        b: theme::RED.2,
    })
}
pub(crate) fn yellow() -> Color {
    c(theme::YELLOW)
}
pub(crate) fn code_bg() -> Color {
    c(theme::CODE_BG)
}
pub(crate) fn code_text() -> Color {
    c(theme::CODE_TEXT)
}
pub(crate) fn code_comment() -> Color {
    c(theme::CODE_COMMENT)
}
pub(crate) fn code_string() -> Color {
    c(theme::CODE_STRING)
}
pub(crate) fn code_symbol() -> Color {
    c(theme::CODE_SYMBOL_GOLD)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Tone {
    Text,
    Muted,
    Green,
    Red,
    Cyan,
    Blue,
    Yellow,
    Code,
    CodeComment,
    CodeString,
    CodeSymbol,
}

pub(crate) fn base() -> Style {
    Style::default().fg(fg()).bg(bg())
}

pub(crate) fn selected() -> Style {
    Style::default()
        .fg(fg())
        .bg(bg_visual())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn tone(tone: Tone) -> Style {
    let fg = match tone {
        Tone::Text => fg(),
        Tone::Muted => muted(),
        Tone::Green => green(),
        Tone::Red => red(),
        Tone::Cyan => cyan(),
        Tone::Blue => blue(),
        Tone::Yellow => yellow(),
        Tone::Code => code_text(),
        Tone::CodeComment => code_comment(),
        Tone::CodeString => code_string(),
        Tone::CodeSymbol => code_symbol(),
    };
    Style::default().fg(fg)
}

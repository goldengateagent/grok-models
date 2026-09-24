//! Ratatui layout. No curses cell buffer — widgets only.

use super::code;
use super::state::{
    App, Col, Focus, Grid, GridRow, HomeItem, MenuLine, ModalView, ProviderDetail, SpanText, Tab,
    config_actions, counts, enabled_grid, home_menu, menu_section_lines, provider_detail,
};
use super::theme::{self, Tone};
use ::ratatui::Frame;
use ::ratatui::layout::{Constraint, Layout, Rect, Spacing};
use ::ratatui::style::{Color, Modifier, Style};
use ::ratatui::symbols::merge::MergeStrategy;
use ::ratatui::text::{Line, Span};
use ::ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Shadow, Table, TableState, Tabs, Wrap,
};
use serde_json::Value;

pub(crate) fn draw(frame: &mut Frame, app: &mut App, doc: &Value) {
    let area = frame.area();
    frame.render_widget(Block::default().style(theme::base()), area);

    let [header, tabs, body, status, help, _pad] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, header, doc);
    draw_tabs(frame, tabs, app);
    draw_status(frame, status, app);
    draw_help(frame, help, app);

    let page = body.height.saturating_sub(3) as usize;
    if page > 0 {
        app.page_rows = page.max(1);
    }
    app.scroll_rect = Rect::default();
    app.scroll_len = 0;

    match app.tab {
        Tab::Providers => draw_providers(frame, body, app, doc),
        Tab::Config if app.config_is_models() => {
            app.menu_rect = Rect::default();
            app.model_rect = body;
            if let Some(grid) = app.configure_grid() {
                let page = search_page_rows(body.height);
                app.page_rows = page.max(1);
                let offset = app.track_configure_offset(grid.selected, grid.rows.len(), page);
                draw_grid(frame, body, &grid, !app.tabs_focused, offset, 1, app);
            }
        }
        Tab::Config => draw_config(frame, body, app, doc),
        Tab::AddProvider => {
            app.menu_rect = Rect::default();
            app.model_rect = body;
            if let Some(grid) = app.add_provider_grid(doc) {
                let page = search_page_rows(body.height);
                app.page_rows = page.max(1);
                let offset = app.track_add_offset(true, grid.selected, grid.rows.len(), page);
                draw_grid(frame, body, &grid, !app.tabs_focused, offset, 1, app);
            }
        }
        Tab::AddModel => {
            app.menu_rect = Rect::default();
            app.model_rect = body;
            if let Some(grid) = app.add_model_grid(doc) {
                let page = search_page_rows(body.height);
                app.page_rows = page.max(1);
                let offset = app.track_add_offset(false, grid.selected, grid.rows.len(), page);
                draw_grid(frame, body, &grid, !app.tabs_focused, offset, 1, app);
            }
        }
        Tab::Benchmarks => {
            app.menu_rect = Rect::default();
            app.model_rect = body;
            let grid = app.bench_grid();
            let page = search_page_rows(body.height);
            app.page_rows = page.max(1);
            let offset = app.track_bench_offset(grid.selected, grid.rows.len(), page);
            draw_grid(frame, body, &grid, !app.tabs_focused, offset, 1, app);
        }
    }

    if let Some(modal) = app.modal_view() {
        draw_modal(frame, area, &modal);
    }
}

fn count_label(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn draw_header(frame: &mut Frame, area: Rect, doc: &Value) {
    let (providers, models) = counts(doc);
    let version = format!(" v{} ", env!("CARGO_PKG_VERSION"));
    let left = format!(
        " grok-models   {} · {}",
        count_label(providers, "provider", "providers"),
        count_label(models, "model", "models"),
    );
    let pad = (area.width as usize).saturating_sub(left.chars().count() + version.chars().count());
    let line = Line::from(vec![
        Span::styled(
            left,
            Style::default()
                .fg(theme::bg_dark())
                .bg(theme::blue())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(theme::blue())),
        Span::styled(
            version,
            Style::default()
                .fg(theme::bg_dark())
                .bg(theme::blue())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.tabs_focused {
            theme::cyan()
        } else {
            theme::blue()
        }))
        .style(Style::default().bg(theme::bg_dark()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let highlight = if app.tabs_focused {
        Style::default()
            .fg(theme::bg_dark())
            .bg(theme::cyan())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme::cyan())
            .bg(theme::bg_dark())
            .add_modifier(Modifier::BOLD)
    };
    let tabs = Tabs::new(Tab::LABELS)
        .select(app.tab.bar_index())
        .divider(Span::styled(" │ ", Style::default().fg(theme::chevron())))
        .style(Style::default().fg(theme::muted()).bg(theme::bg_dark()))
        .highlight_style(highlight)
        .padding(" ", " ");
    frame.render_widget(tabs, inner);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let (text, tone) = match &app.status {
        Some(msg) => (
            format!(" {msg}"),
            if app.status_error {
                theme::red()
            } else {
                theme::green()
            },
        ),
        None => (
            " Changes are written as you go. Quit syncs config.toml when something changed.".into(),
            theme::muted(),
        ),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            text,
            Style::default().fg(tone).bg(theme::bg()),
        )),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let pairs: &[(&str, &str)] = match app.tab {
        Tab::Providers => &[
            ("↑/↓", "nav"),
            ("Enter/→", "select"),
            ("PgUp/PgDn", "page"),
            ("S", "sort"),
            ("Tab", "tabs"),
            ("Q", "quit"),
        ],
        Tab::Config if app.config_is_models() => &[
            ("↑/↓/←/→", "nav"),
            ("ESC", "back"),
            ("Enter", "toggle"),
            ("Shift+S", "sort"),
            ("Type", "filter"),
        ],
        Tab::Config => &[
            ("↑/↓", "nav"),
            ("←", "back"),
            ("Enter/→", "select"),
            ("Tab", "tabs"),
        ],
        Tab::AddProvider => &[
            ("↑/↓/←/→", "nav"),
            ("ESC", "cancel"),
            ("Enter", "enable"),
            ("Type", "filter"),
            ("Tab", "tabs"),
        ],
        Tab::AddModel => &[
            ("↑/↓/←/→", "nav"),
            ("ESC", "cancel"),
            ("Enter", "enable"),
            ("Type", "filter"),
            ("Tab", "tabs"),
        ],
        Tab::Benchmarks => &[
            ("↑/↓/←/→", "nav"),
            ("ESC", "cancel"),
            ("Shift+S", "sort"),
            ("Type", "filter"),
            ("Tab", "tabs"),
        ],
    };
    render_help(frame, area, pairs, true);
}

fn draw_popup_help(frame: &mut Frame, area: Rect, pairs: &[(&str, &str)]) {
    // The popup block already supplies the one-column indent.
    render_help(frame, area, pairs, false);
}

fn render_help(frame: &mut Frame, area: Rect, pairs: &[(&str, &str)], leading: bool) {
    let mut spans = Vec::new();
    if leading {
        spans.push(Span::styled(" ", Style::default().bg(theme::bg_dark())));
    }
    for (i, (key, desc)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                "  │  ",
                Style::default().fg(theme::muted()).bg(theme::bg_dark()),
            ));
        }
        // U+FE0F selects the emoji form so arrows sit in the vertical center
        // of the row instead of on the text baseline.
        spans.push(Span::styled(
            text_arrows(key),
            Style::default()
                .fg(theme::blue())
                .bg(theme::bg_dark())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::default().fg(theme::fg_dark()).bg(theme::bg_dark()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .alignment(::ratatui::layout::Alignment::Left)
            .style(Style::default().bg(theme::bg_dark())),
        area,
    );
}

fn text_arrows(key: &str) -> String {
    // Windows console fonts lack the arrow + VS16 sequence, so emit the
    // same arrow codepoints without the suffix there.
    if cfg!(target_os = "windows") {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len() + 4);
    for ch in key.chars() {
        out.push(ch);
        if matches!(ch, '↑' | '↓' | '←' | '→') {
            out.push('\u{FE0F}');
        }
    }
    out
}

fn draw_providers(frame: &mut Frame, area: Rect, app: &mut App, doc: &Value) {
    let items = home_menu(doc);
    let split = items
        .iter()
        .position(|item| matches!(item, HomeItem::Action { .. }))
        .unwrap_or(items.len());
    let (providers, options) = items.split_at(split);
    let menu_on = app.focus == Focus::Menu && !app.tabs_focused;
    let nav = app.nav_index();
    let provider_lines = menu_section_lines(
        providers,
        menu_on.then_some(nav).filter(|i| *i < providers.len()),
    );
    let option_lines = menu_section_lines(
        options,
        (menu_on && nav >= providers.len()).then(|| nav - providers.len()),
    );
    let (prov_h, opt_h) = split_menu_heights(provider_lines.len(), option_lines.len(), area.height);
    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(prov_h),
        Constraint::Length(opt_h),
        Constraint::Fill(1),
    ])
    .spacing(Spacing::Overlap(1))
    .areas(area);
    app.menu_rect = Rect {
        x: top.x,
        y: top.y,
        width: top.width,
        height: mid.y.saturating_add(mid.height).saturating_sub(top.y),
    };
    app.model_rect = bottom;
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(theme::cyan())
                .add_modifier(Modifier::BOLD),
        ))
    };
    draw_menu(
        frame,
        top,
        heading(" Providers "),
        &provider_lines,
        menu_on && nav < providers.len(),
        1,
    );
    draw_menu(frame, mid, heading(" Options "), &option_lines, true, 1);
    let grid = enabled_grid(doc, app);
    let mut state = TableState::new().with_selected(grid.selected);
    *state.offset_mut() = app.model_offset;
    draw_grid_state(
        frame,
        bottom,
        &grid,
        app.focus == Focus::Models && !app.tabs_focused,
        true,
        1,
        &mut state,
        app,
    );
    app.model_offset = state.offset();
    let visible = bottom.height.saturating_sub(3) as usize;
    if visible > 0 {
        app.page_rows = visible.max(1);
    }
}

fn draw_config(frame: &mut Frame, area: Rect, app: &mut App, doc: &Value) {
    let Some(id) = app.provider_id().map(|s| s.to_string()) else {
        app.menu_rect = Rect::default();
        app.model_rect = Rect::default();
        let block = panel(" Config ", false);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(
                "Select a provider on the Providers tab.\n\nEnter opens configure, enable, base URL, and delete.",
            )
            .style(Style::default().fg(theme::muted()))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };
    let lines = config_actions(
        doc,
        &id,
        if app.tabs_focused {
            usize::MAX
        } else {
            app.action_index()
        },
    );
    let [top, bottom] = Layout::vertical([
        Constraint::Length(menu_height(lines.len(), area.height)),
        Constraint::Fill(1),
    ])
    .spacing(Spacing::Overlap(1))
    .areas(area);
    app.menu_rect = top;
    app.model_rect = bottom;
    let detail = provider_detail(doc, &id);
    draw_menu(
        frame,
        top,
        provider_heading(&detail),
        &lines,
        !app.tabs_focused,
        1,
    );
    draw_detail(frame, bottom, &detail);
}

fn provider_heading(detail: &ProviderDetail) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {} ", detail.name),
        Style::default()
            .fg(theme::cyan())
            .add_modifier(Modifier::BOLD),
    ))
}

fn draw_detail(frame: &mut Frame, area: Rect, detail: &ProviderDetail) {
    let block = panel(" Info ", true).padding(Padding::left(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(Span::styled(
            "Provider docs:",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            if detail.doc_url.is_empty() {
                "none".to_string()
            } else {
                detail.doc_url.clone()
            },
            Style::default().fg(theme::blue()),
        )),
    ];
    if detail.setup.is_some() || detail.masked.is_some() {
        lines.push(Line::from(""));
        let mut panel: Vec<String> = Vec::new();
        if let Some(setup) = &detail.setup {
            panel.extend(setup.lines().map(str::to_string));
        }
        if let Some(masked) = &detail.masked {
            if !panel.is_empty() {
                panel.push(String::new());
            }
            panel.push("# required env_key value".to_string());
            panel.extend(masked.lines().map(str::to_string));
        }
        let width = inner.width as usize;
        lines.extend(code_canvas(&panel, width));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "No API key env var for this provider.",
            Style::default().fg(theme::muted()),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// One solid black rectangle. Every row is the same width, including blanks,
/// and each line is colored with the shell tokenizer.
fn code_canvas(lines: &[String], max_width: usize) -> Vec<Line<'static>> {
    const PAD: usize = 1;
    let segs: Vec<Vec<(String, code::CodeColor)>> = lines
        .iter()
        .map(|line| code::code_line_segments(line, None))
        .collect();
    let content = segs
        .iter()
        .map(|row| {
            row.iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let width = (content + PAD * 2).min(max_width.max(1)).max(1);
    let black = Style::default().fg(theme::code_text()).bg(theme::code_bg());
    segs.into_iter()
        .map(|row| {
            let mut spans = vec![Span::styled(" ".repeat(PAD.min(width)), black)];
            let mut used = PAD.min(width);
            for (text, paint) in row {
                if used >= width {
                    break;
                }
                let room = width - used;
                let shown: String = text.chars().take(room).collect();
                let n = shown.chars().count();
                if n == 0 {
                    continue;
                }
                spans.push(Span::styled(shown, code_paint(paint)));
                used += n;
            }
            if used < width {
                spans.push(Span::styled(" ".repeat(width - used), black));
            }
            Line::from(spans)
        })
        .collect()
}

fn code_paint(paint: code::CodeColor) -> Style {
    let fg = match paint {
        code::CodeColor::Comment => theme::code_comment(),
        code::CodeColor::Error => theme::red(),
        code::CodeColor::String => theme::code_string(),
        code::CodeColor::Symbol => theme::code_symbol(),
        code::CodeColor::Text | code::CodeColor::Var => theme::code_text(),
    };
    Style::default().fg(fg).bg(theme::code_bg())
}

fn draw_menu(
    frame: &mut Frame,
    area: Rect,
    title: Line<'static>,
    lines: &[MenuLine],
    focused: bool,
    bottom_pad: u16,
) {
    let block = panel_title(title, focused).padding(Padding::bottom(bottom_pad));
    let items: Vec<ListItem> = lines
        .iter()
        .map(|line| ListItem::new(menu_line(line, area.width)))
        .collect();
    let selected = lines.iter().position(|l| l.selected);
    let mut state = ListState::default();
    state.select(selected);
    // No highlight style: it would repaint the env-var chip. Selected rows
    // already carry their own background, and protected spans keep theirs.
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new())
        .highlight_symbol("");
    frame.render_stateful_widget(list, area, &mut state);
}

fn menu_line(line: &MenuLine, width: u16) -> Line<'static> {
    let rule = line.spans.len() == 1 && line.spans[0].text.chars().all(|ch| ch == '─' || ch == ' ');
    if rule {
        // Inside the border, so the rule meets both sides of the panel.
        let cols = width.saturating_sub(2).max(1) as usize;
        return Line::from(Span::styled(
            "─".repeat(cols),
            theme::tone(line.spans[0].tone),
        ));
    }
    Line::from(
        line.spans
            .iter()
            .map(|span| {
                let mut style = theme::tone(span.tone);
                if span.protect {
                    style = style.bg(theme::code_bg());
                } else if line.selected {
                    style = style.bg(theme::bg_visual()).add_modifier(Modifier::BOLD);
                }
                Span::styled(span.text.clone(), style)
            })
            .collect::<Vec<_>>(),
    )
}

fn draw_grid(
    frame: &mut Frame,
    area: Rect,
    grid: &Grid,
    focused: bool,
    offset: usize,
    left_pad: u16,
    app: &mut App,
) {
    let mut state = TableState::new()
        .with_selected(grid.selected)
        .with_offset(offset);
    draw_grid_state(
        frame, area, grid, focused, focused, left_pad, &mut state, app,
    );
}

fn draw_grid_state(
    frame: &mut Frame,
    area: Rect,
    grid: &Grid,
    focused: bool,
    border_on: bool,
    left_pad: u16,
    state: &mut TableState,
    app: &mut App,
) {
    let title = match &grid.query {
        Some(q) => {
            let caret = if app.cursor_on { "█" } else { " " };
            format!(" {} | Search: {q}{caret}", grid.title)
        }
        None => format!(" {} ", grid.title),
    };
    let search = grid.query.is_some();
    let header = Row::new(grid.headers.iter().map(header_cell).collect::<Vec<Cell>>())
        .height(if search { 2 } else { 1 })
        .style(
            Style::default()
                .fg(theme::fg())
                .bg(theme::bg_visual())
                .add_modifier(Modifier::BOLD),
        );
    let ncols = grid.headers.len().max(1) as u16;
    let rows: Vec<Row> = grid.rows.iter().map(|row| grid_row(row, ncols)).collect();
    let widths: Vec<Constraint> = grid
        .headers
        .iter()
        .map(|col| {
            if col.fill {
                Constraint::Fill(1)
            } else {
                Constraint::Length(col.width)
            }
        })
        .collect();
    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(&title, border_on).padding(Padding::left(left_pad)))
        .row_highlight_style(if focused {
            // Background only, so column colors (enabled, scores) stay put.
            Style::default()
                .bg(theme::bg_visual())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        })
        .column_spacing(2);
    frame.render_stateful_widget(table, area, state);
    if search {
        // Header is two rows. The second is a rule under the labels, same span
        // as a section separator, in the unfocused panel grey.
        let y = area.y.saturating_add(2);
        let x0 = area.x.saturating_add(1).saturating_add(left_pad);
        let x1 = area.right().saturating_sub(1);
        let buf = frame.buffer_mut();
        for x in x0..x1 {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol("─");
                cell.set_fg(theme::chevron());
                cell.set_bg(theme::bg());
            }
        }
    }
    let visible = area.height.saturating_sub(if search { 4 } else { 3 }) as usize;
    if grid.rows.len() > visible && area.width > 0 && area.height > 2 {
        let mut scroll = ScrollbarState::new(grid.rows.len())
            .position(state.offset())
            .viewport_content_length(visible.max(1));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(theme::chevron())),
            area,
            &mut scroll,
        );
        // Right column, including the arrow caps. Clicks and drags map onto rows.
        app.scroll_rect = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        app.scroll_len = grid.rows.len();
    } else {
        app.scroll_rect = Rect::default();
        app.scroll_len = 0;
    }
}

fn header_cell(col: &Col) -> Cell<'static> {
    let text = if col.right {
        format!("{:>width$}", col.label, width = col.width as usize)
    } else {
        col.label.clone()
    };
    let pad = text.chars().take_while(|ch| *ch == ' ').count();
    let rest: String = text.chars().skip(pad).collect();
    let base = Style::default()
        .fg(theme::fg())
        .add_modifier(Modifier::BOLD);
    let marked = if col.hot {
        Style::default()
            .fg(theme::yellow())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        base
    };
    Cell::from(Line::from(vec![
        Span::styled(" ".repeat(pad), base),
        Span::styled(rest, marked),
    ]))
}

fn grid_row(row: &GridRow, ncols: u16) -> Row<'static> {
    match row {
        GridRow::Sep(tone) => {
            let color = match tone {
                super::state::SepTone::Green => theme::green(),
                super::state::SepTone::Cyan => theme::cyan(),
            };
            Row::new(vec![
                Cell::from(Span::styled("─".repeat(240), Style::default().fg(color)))
                    .column_span(ncols),
            ])
        }
        GridRow::Cells(cells) => Row::new(cells.iter().map(span_of).collect::<Vec<_>>()),
    }
}

fn span_of(cell: &SpanText) -> Span<'static> {
    Span::styled(cell.text.clone(), theme::tone(cell.tone))
}

fn draw_modal(frame: &mut Frame, area: Rect, modal: &ModalView) {
    match modal {
        ModalView::Error(message) => {
            let rect = centered(area, 68, 8);
            let block = popup(" Error ");
            let inner = below_header(block.inner(rect));
            frame.render_widget(Clear, rect);
            frame.render_widget(block, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        message.clone(),
                        Style::default().fg(theme::red()),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Press any key to go back",
                        Style::default().fg(theme::muted()),
                    )),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
        }
        ModalView::Confirm { prompt, yes } => {
            let rect = centered(area, 68, 8);
            let block = popup(" Confirm ");
            let inner = below_header(block.inner(rect));
            frame.render_widget(Clear, rect);
            frame.render_widget(block, rect);
            let yes_style = if *yes {
                theme::selected()
            } else {
                theme::tone(Tone::Green)
            };
            let no_style = if *yes {
                theme::tone(Tone::Muted)
            } else {
                theme::selected()
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        prompt.clone(),
                        Style::default().fg(theme::fg()),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("  Yes  ", yes_style),
                        Span::raw("   "),
                        Span::styled("  No  ", no_style),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Y yes    N no    Esc cancel",
                        Style::default().fg(theme::muted()),
                    )),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
        }
        ModalView::Input {
            title,
            hint,
            buffer,
        } => {
            let rect = centered(area, 72, 8);
            let block = popup(&format!(" {title} "));
            let inner = below_header(block.inner(rect));
            frame.render_widget(Clear, rect);
            frame.render_widget(block, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        format!("{buffer}█"),
                        Style::default()
                            .fg(theme::fg())
                            .bg(theme::bg_visual())
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        hint.clone(),
                        Style::default().fg(theme::muted()),
                    )),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
        }
        ModalView::Pick {
            title,
            info,
            choices,
        } => {
            let height = if title == "Web Search" {
                17
            } else if info.is_some() {
                19
            } else {
                14
            };
            let rect = centered(area, 76, height);
            frame.render_widget(Clear, rect);
            let block = popup(&format!(" {title} "));
            let inner = below_header(block.inner(rect));
            frame.render_widget(block, rect);
            let [body, help_area] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
            let list_area = if let Some(text) = info {
                let (info_h, gap) = if title == "Web Search" {
                    (2, 1)
                } else {
                    (7, 0)
                };
                let [info_area, _gap, list_area] = Layout::vertical([
                    Constraint::Length(info_h),
                    Constraint::Length(gap),
                    Constraint::Fill(1),
                ])
                .areas(body);
                frame.render_widget(
                    Paragraph::new(text.clone())
                        .style(Style::default().fg(theme::muted()))
                        .wrap(Wrap { trim: true }),
                    info_area,
                );
                list_area
            } else {
                body
            };
            let selected = choices.iter().position(|line| line.selected);
            let items: Vec<ListItem> = choices
                .iter()
                .map(|line| ListItem::new(menu_line(line, list_area.width)))
                .collect();
            let mut state = ListState::default();
            state.select(selected);
            // No highlight style: it would repaint the env-var chip.
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(Style::new())
                    .highlight_symbol(""),
                list_area,
                &mut state,
            );
            draw_popup_help(
                frame,
                help_area,
                &[("ESC/←", "close"), ("Enter/→", "select")],
            );
        }
    }
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    panel_title(
        Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(if focused { theme::cyan() } else { theme::fg() })
                .add_modifier(Modifier::BOLD),
        )),
        focused,
    )
}

fn panel_title(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(if focused {
            theme::blue()
        } else {
            theme::chevron()
        }))
        .style(theme::base())
        .merge_borders(MergeStrategy::Exact)
}

fn below_header(inner: Rect) -> Rect {
    Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    }
}

fn popup(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme::yellow())
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme::blue()))
        .padding(Padding::left(1))
        .style(Style::default().bg(theme::bg_dark()).fg(theme::fg()))
        .shadow(Shadow::default())
}

/// Search tables use one extra header row for the rule under the column labels.
fn search_page_rows(height: u16) -> usize {
    height.saturating_sub(4) as usize
}

/// Options list is only as tall as its rows. Enabled models keep the rest.
fn menu_height(rows: usize, total: u16) -> u16 {
    let wanted = (rows as u16).saturating_add(3);
    let reserve = 6u16.min(total.saturating_sub(3));
    wanted.min(total.saturating_sub(reserve)).max(3.min(total))
}

/// Providers and Options are only as tall as their rows plus one blank line.
/// Enabled Models keeps the rest. Two shared borders are subtracted once.
fn split_menu_heights(providers: usize, options: usize, total: u16) -> (u16, u16) {
    let wanted = |rows: usize| (rows as u16).saturating_add(3);
    let mut prov = wanted(providers);
    let mut opts = wanted(options);
    let reserve = 6u16.min(total.saturating_sub(3));
    let cap = total.saturating_sub(reserve).saturating_add(2);
    while prov + opts > cap && (prov > 3 || opts > 3) {
        if opts > 3 {
            opts -= 1;
        } else {
            prov -= 1;
        }
    }
    (prov.max(3.min(total)), opts.max(3.min(total)))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(8);
    let height = height.min(area.height.saturating_sub(2)).max(4);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

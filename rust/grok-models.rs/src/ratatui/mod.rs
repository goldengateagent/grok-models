//! Default full-screen interface.
//!
//! The previous screens stay in `crate::tui` and start with `--legacy`. This
//! module draws with Ratatui widgets and calls the same provider, sync, and
//! config operations.

mod code;
mod draw;
mod state;
mod theme;

use crate::Res;
use ::ratatui::Terminal;
use ::ratatui::backend::CrosstermBackend;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use serde_json::Value;
use state::App;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

/// Run the Ratatui interface. `Ok(true)` means providers.json changed and the
/// caller should rewrite config.toml, matching the default screen.
pub fn run(doc: &mut Value) -> Res<bool> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Ok(false);
    }
    let mut stdout = io::stdout();
    enable_raw_mode().map_err(|e| crate::Error::new(e.to_string()))?;
    execute!(stdout, EnterAlternateScreen, event::EnableMouseCapture)
        .map_err(|e| crate::Error::new(e.to_string()))?;
    let _guard = TermGuard;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(|e| crate::Error::new(e.to_string()))?;
    let mut app = App::new();
    let mut mouse = true;
    let result = loop {
        terminal
            .draw(|frame| draw::draw(frame, &mut app, doc))
            .map_err(|e| crate::Error::new(e.to_string()))?;
        let want_mouse = !app.terminal_selects_text();
        if want_mouse != mouse {
            let result = if want_mouse {
                execute!(io::stdout(), event::EnableMouseCapture)
            } else {
                execute!(io::stdout(), event::DisableMouseCapture)
            };
            result.map_err(|e| crate::Error::new(e.to_string()))?;
            mouse = want_mouse;
        }
        if !event::poll(Duration::from_millis(500)).map_err(|e| crate::Error::new(e.to_string()))? {
            app.blink_cursor();
            continue;
        }
        match event::read().map_err(|e| crate::Error::new(e.to_string()))? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if app.on_key(doc, key)? {
                    break Ok(app.changed);
                }
            }
            Event::Mouse(mouse) => app.on_mouse(doc, mouse),
            _ => {}
        }
    };
    drop(_guard);
    let _ = disable_raw_mode();
    result
}

/// Restores the terminal even if the screen panics.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, LeaveAlternateScreen, event::DisableMouseCapture);
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{ModelsDev, ModelsDevProvider};
    use ::ratatui::backend::TestBackend;
    use serde_json::json;
    use std::collections::HashMap;

    fn sample() -> Value {
        json!({
            "providers": [{
                "id": "opencode",
                "name": "OpenCode",
                "enabled": true,
                "env_key": "OPENCODE_API_KEY",
                "base_url": "https://example.test/v1",
                "doc": "https://example.test/docs",
                "models": {
                    "glm-5": {"name": "GLM-5", "enabled": true, "api_backend": "responses"}
                }
            }],
            "include_descriptions": true,
            "last_updated": "01-02-2026 03:04 PM"
        })
    }

    fn frame(app: &mut App, doc: &Value) -> String {
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw::draw(f, app, doc)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn providers_screen_shows_actions_and_enabled_model() {
        let doc = sample();
        let mut app = App::new();
        let text = frame(&mut app, &doc);
        assert!(text.contains("grok-models"), "{text}");
        assert!(text.contains("Providers"), "{text}");
        assert!(text.contains("Add Provider"), "{text}");
        assert!(text.contains("Add Model"), "{text}");
        assert!(text.contains("Benchmarks"), "{text}");
        assert!(text.contains("Codex Config"), "{text}");
        assert!(text.contains("Web Search"), "{text}");
        assert!(text.contains("Model Descriptions"), "{text}");
        assert!(text.contains("Update Model List"), "{text}");
        assert!(text.contains("Sync Model Config"), "{text}");
        assert!(text.contains("GLM-5"), "{text}");
        assert!(text.contains("quit"), "{text}");
        let add_at = text.find("Sync Model Config").expect("options list");
        let models_at = text.find("Enabled Models").expect("models pane");
        assert!(
            add_at < models_at,
            "enabled models must sit under the options list:\n{text}"
        );
    }

    #[test]
    fn benchmarks_screen_lists_scores_with_filter_header() {
        let doc = sample();
        let mut app = App::new();
        app.tab = state::Tab::Benchmarks;
        let text = frame(&mut app, &doc);
        assert!(text.contains("Artificial Analysis"), "{text}");
        assert!(text.contains("(81)"), "{text}");
        assert!(text.contains("| Search:"), "{text}");
        assert!(text.contains("Name"), "{text}");
        assert!(text.contains("Slug"), "{text}");
        assert!(text.contains("Claude 4 Sonnet (Reasoning)"), "{text}");
        assert!(text.contains("claude-4-sonnet-thinking"), "{text}");
        assert!(text.contains("Shift+S"), "{text}");
        assert!(text.contains("ESC"), "{text}");
        assert!(text.contains("cancel"), "{text}");
        assert!(text.contains("filter"), "{text}");
    }

    #[test]
    fn config_screen_shows_provider_actions_and_key_setup() {
        let mut doc = sample();
        let mut app = App::new();
        app.on_key(
            &mut doc,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        )
        .unwrap();
        let text = frame(&mut app, &doc);
        assert!(text.contains("Configure Models"), "{text}");
        assert!(text.contains("Base Url"), "{text}");
        assert!(text.contains("Delete Provider"), "{text}");
        assert!(text.contains("example.test"), "{text}");
        assert!(text.contains("OPENCODE_API_KEY"), "{text}");
        let actions = text.find("Delete Provider").expect("actions");
        let info = text.find("Provider docs").expect("info");
        assert!(
            actions < info,
            "config info must sit under the actions list:\n{text}"
        );
    }

    #[test]
    fn selected_provider_row_keeps_env_colors() {
        let doc = sample();
        let mut app = App::new();
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw::draw(f, &mut app, &doc)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut style = None;
        'scan: for y in 0..buf.area().height {
            for x in 0..buf.area().width.saturating_sub(8) {
                let word: String = (0..8).map(|i| buf[(x + i, y)].symbol()).collect();
                if word == "OPENCODE" {
                    style = Some(buf[(x, y)].style());
                    break 'scan;
                }
            }
        }
        let style = style.expect("env var on the provider row");
        assert_eq!(
            style.fg,
            Some(::ratatui::style::Color::Rgb(0, 255, 0)),
            "env var name must stay green on the highlighted row"
        );
        assert_eq!(
            style.bg,
            Some(::ratatui::style::Color::Rgb(0, 0, 0)),
            "env var chip must keep its black background"
        );
    }

    #[test]
    fn add_provider_screen_lists_suggested_bucket() {
        let doc = sample();
        let mut providers = HashMap::new();
        for id in ["opencode", "kilo"] {
            providers.insert(
                id.to_string(),
                ModelsDevProvider {
                    name: Some(id.to_string()),
                    ..Default::default()
                },
            );
        }
        let mut app = App::new();
        app.testing_set_api(ModelsDev { providers });
        app.testing_tab_add_provider();
        let text = frame(&mut app, &doc);
        assert!(text.contains("Add Provider"), "{text}");
        assert!(text.contains("kilo"), "{text}");
        assert!(text.contains("opencode"), "{text}");
        assert!(text.contains("[enabled]"), "{text}");
        assert!(text.contains("[disabled]"), "{text}");
        let dashes = text
            .lines()
            .filter(|line| line.chars().filter(|c| *c == '─').count() > 20)
            .count();
        assert!(
            dashes > 0,
            "section rules must cross the row, not a short stub:\n{text}"
        );
        assert!(
            text.contains("↑/↓/←/→") || text.contains("↑\u{fe0f}/↓\u{fe0f}/←\u{fe0f}/→\u{fe0f}"),
            "{text}"
        );
        assert!(text.contains("cancel"), "{text}");
        assert!(text.contains("Type"), "{text}");
        assert!(text.contains("Tab"), "{text}");
    }
}

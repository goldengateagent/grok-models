//! Screen state and the operations behind it.
//!
//! Drawing lives in `draw`. Mutations call the existing provider, sync, and
//! config helpers — this module does not paint the terminal.

use super::theme::Tone;
use crate::benchmarks::{self, Scores};
use crate::core::{self, SortedIndices};
use crate::env::{paths, vars};
use crate::fetch::ModelsDev;
use crate::json_utils::{self, get_bool_value, get_name_or};
use crate::jsonio::{self, INCLUDE_DESCRIPTIONS_DEFAULT};
use crate::sync::SyncWarning;
use crate::{Res, fail};
use ::ratatui::layout::Rect;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::HashSet;

const SUGGESTED: &[&str] = &[
    "opencode",
    "opencode-go",
    "openrouter",
    "ollama-cloud",
    "gmicloud",
    "kilo",
];

const WEB_SEARCH_INFO: &str =
    "web_search tool custom model ('responses' api_backend). Default is Grok.";

const CODEX_INFO: &str = "\
$CODEX_HOME/config.toml and $CODEX_HOME/<provider>-models.json are updated to enable this provider's enabled models. Codex only allows one configured provider by setting:

  model_provider = <provider>
  model_catalog_json = <provider>-models.json

Disabling removes this config from config.toml and deletes its models json file.";

const MODEL_NAME_COL_MAX: usize = 35;
const MODEL_ID_COL_MAX: usize = 25;
const PROVIDER_NAME_COL_MAX: usize = 25;
const PROVIDER_ID_COL_MAX: usize = 30;
const MAIN_PROVIDER_NAME_COL_MAX: usize = 15;
const INTEL_COL_W: usize = 5;
const CODING_COL_W: usize = 6;
const MODE_COL_W: usize = 10;
const PROVIDER_TOKEN_W: usize = 10;
const PROVIDER_ENV_GAP: usize = 2;
const PROVIDER_ENV_PAD: i32 = 1;
const MODEL_DESC_LABEL: &str = "Model Descriptions";
const WEB_SEARCH_LABEL: &str = "Web Search";
const CODEX_CONFIG_LABEL: &str = "Codex Config";
const UPDATE_LIST_LABEL: &str = "Update Model List";
const SYNC_CONFIG_LABEL: &str = "Sync Model Config";

fn clipped_paren_name(name: &str, max: usize) -> String {
    format!("({})", name.chars().take(max).collect::<String>())
}

fn provider_display(provider: &Map<String, Value>) -> String {
    let provider_id = provider
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = provider
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(provider_id);
    format!("({name}) - {provider_id}")
}

fn provider_state_token_col(providers: &[Map<String, Value>]) -> usize {
    let name_w = providers
        .iter()
        .map(|p| {
            let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = p.get("name").and_then(Value::as_str).unwrap_or(pid);
            clipped_paren_name(name, MAIN_PROVIDER_NAME_COL_MAX).len()
        })
        .max()
        .unwrap_or(0);
    let id_w = providers
        .iter()
        .map(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .len()
        })
        .max()
        .unwrap_or(0);
    let provider_col = if providers.is_empty() {
        0
    } else {
        name_w + 3 + id_w + 1
    };
    provider_col
        .max(MODEL_DESC_LABEL.len() + 1)
        .max(WEB_SEARCH_LABEL.len() + 1)
        .max(CODEX_CONFIG_LABEL.len() + 1)
        .max(UPDATE_LIST_LABEL.len() + 1)
        .max(SYNC_CONFIG_LABEL.len() + 1)
}

fn pad_state_label(label: &str, token: &str, token_col: usize) -> String {
    let mut out = String::from(label);
    if out.len() < token_col {
        out.push_str(&" ".repeat(token_col - out.len()));
    }
    out.push_str(token);
    out
}

fn provider_menu_labels(providers: &[Map<String, Value>]) -> Vec<String> {
    let names: Vec<String> = providers
        .iter()
        .map(|p| {
            let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = p.get("name").and_then(Value::as_str).unwrap_or(pid);
            clipped_paren_name(name, MAIN_PROVIDER_NAME_COL_MAX)
        })
        .collect();
    let name_w = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let id_w = providers
        .iter()
        .map(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .len()
        })
        .max()
        .unwrap_or(0);
    let token_col = provider_state_token_col(providers);
    let env_w = providers
        .iter()
        .map(|p| crate::core::provider_env_key_from_json(p).len())
        .max()
        .unwrap_or(0);
    names
        .iter()
        .zip(providers.iter())
        .map(|(name, p)| {
            let state = if p.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
                "enabled"
            } else {
                "disabled"
            };
            let pid = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let token = format!("[{state}]");
            let head = format!("{:<name_w$} - {:<id_w$}", name, pid);
            let mut left = format!("{:<token_col$}{:<tw$}", head, token, tw = PROVIDER_TOKEN_W);
            let envk = crate::core::provider_env_key_from_json(p);
            if !envk.is_empty() {
                left.push_str(&" ".repeat(PROVIDER_ENV_GAP));
                left.push_str(&format!("{envk:<env_w$} = "));
                left.push_str(&crate::env::vars::env_key_masked(&envk));
            }
            left
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Providers = 0,
    Config = 1,
    AddProvider = 2,
    AddModel = 3,
    Benchmarks = 4,
}

impl Tab {
    /// Tabs you can open from the bar. Config is a provider page, not a tab.
    pub(crate) const LABELS: [&'static str; 4] =
        ["Providers", "Add Provider", "Add Model", "Benchmarks"];

    pub(crate) fn bar_index(self) -> Option<usize> {
        match self {
            Self::Providers | Self::Config => Some(0),
            Self::AddProvider => Some(1),
            Self::AddModel => Some(2),
            Self::Benchmarks => Some(3),
        }
    }

    fn from_key(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Providers),
            1 => Some(Self::AddProvider),
            2 => Some(Self::AddModel),
            3 => Some(Self::Benchmarks),
            _ => None,
        }
    }

    fn cycle(self, forward: bool) -> Self {
        let order = [
            Self::Providers,
            Self::AddProvider,
            Self::AddModel,
            Self::Benchmarks,
        ];
        let at = order.iter().position(|tab| *tab == self);
        let next = match (at, forward) {
            (Some(i), true) => (i + 1) % order.len(),
            (Some(i), false) => (i + order.len() - 1) % order.len(),
            (None, true) => 1,
            (None, false) => 0,
        };
        order[next]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    Menu,
    Models,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnabledSort {
    Model,
    Intel,
    Coding,
    Provider,
}

impl EnabledSort {
    fn cycle(self) -> Self {
        match self {
            Self::Model => Self::Intel,
            Self::Intel => Self::Coding,
            Self::Coding => Self::Provider,
            Self::Provider => Self::Model,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelSort {
    Name,
    Intel,
    Coding,
}

impl ModelSort {
    fn cycle(self) -> Self {
        match self {
            Self::Name => Self::Intel,
            Self::Intel => Self::Coding,
            Self::Coding => Self::Name,
        }
    }

    fn hot_col(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Intel => 1,
            Self::Coding => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigView {
    Empty,
    Actions,
    Models,
}

#[derive(Clone, Debug)]
struct ConfigureSession {
    provider_id: String,
    pname: String,
    ids: Vec<String>,
    models: Map<String, Value>,
    query: String,
    selected: usize,
    /// First visible row. Stays put until the selection leaves the page.
    offset: usize,
    sort: ModelSort,
    changed: bool,
}

#[derive(Clone, Debug)]
enum Modal {
    None,
    Error(String),
    ConfirmDelete {
        provider_id: String,
        label: String,
        yes: bool,
    },
    BaseUrl {
        provider_id: String,
        buffer: String,
    },
    Pick(Picker),
}

#[derive(Clone, Debug)]
struct Picker {
    title: String,
    info: Option<String>,
    choices: Vec<String>,
    /// Parallel to `choices`. `None` means the disabled sentinel.
    values: Vec<Option<String>>,
    selected: usize,
    kind: PickKind,
}

#[derive(Clone, Debug)]
enum PickKind {
    Codex,
    WebSearch,
    Reasoning { pid: String, mid: String },
}

#[derive(Clone, Debug)]
struct Filter {
    query: String,
    selected: usize,
    /// First visible row. Stays put until the selection leaves the page.
    offset: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct App {
    pub(crate) tab: Tab,
    /// Cursor is on the tab bar, not in the screen list.
    pub(crate) tabs_focused: bool,
    config_view: ConfigView,
    provider_id: Option<String>,
    pub(crate) focus: Focus,
    nav: usize,
    action: usize,
    pub(crate) enabled_sort: EnabledSort,
    model_index: Option<usize>,
    pub(crate) model_offset: usize,
    pub(crate) page_rows: usize,
    /// Last drawn options list, used to hit-test the mouse wheel.
    pub(crate) menu_rect: Rect,
    /// Last drawn model table, used to hit-test the mouse wheel.
    pub(crate) model_rect: Rect,
    /// Right-hand scrollbar column from the last draw. Empty when no bar.
    pub(crate) scroll_rect: Rect,
    /// Row count that scrollbar maps onto.
    pub(crate) scroll_len: usize,
    scroll_drag: bool,
    /// Search caret on the configure-models title. Flips on the idle tick.
    pub(crate) cursor_on: bool,
    configure: Option<ConfigureSession>,
    add_provider: Filter,
    add_model: Filter,
    bench: Filter,
    bench_sort: benchmarks::BenchSort,
    api: Option<ModelsDev>,
    modal: Modal,
    pub(crate) status: Option<String>,
    pub(crate) status_error: bool,
    pub(crate) changed: bool,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            tab: Tab::Providers,
            tabs_focused: false,
            config_view: ConfigView::Empty,
            provider_id: None,
            focus: Focus::Menu,
            nav: 0,
            action: 0,
            enabled_sort: EnabledSort::Model,
            model_index: None,
            model_offset: 0,
            page_rows: 12,
            menu_rect: Rect::default(),
            model_rect: Rect::default(),
            scroll_rect: Rect::default(),
            scroll_len: 0,
            scroll_drag: false,
            cursor_on: true,
            configure: None,
            add_provider: Filter {
                query: String::new(),
                selected: 0,
                offset: 0,
            },
            add_model: Filter {
                query: String::new(),
                selected: 0,
                offset: 0,
            },
            bench: Filter {
                query: String::new(),
                selected: 0,
                offset: 0,
            },
            bench_sort: benchmarks::BenchSort::Name,
            api: None,
            modal: Modal::None,
            status: None,
            status_error: false,
            changed: false,
        }
    }

    pub(crate) fn on_key(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return Ok(false);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return self.interrupt(doc, true);
        }
        if !matches!(self.modal, Modal::None) {
            return self.on_modal(doc, key);
        }
        if self.tabs_focused {
            return self.on_tabs(doc, key);
        }
        match self.tab {
            Tab::Providers => self.on_providers(doc, key),
            Tab::Config => self.on_config(doc, key),
            Tab::AddProvider => self.on_add_provider(doc, key),
            Tab::AddModel => self.on_add_model(doc, key),
            Tab::Benchmarks => self.on_benchmarks(doc, key),
        }
    }

    fn interrupt(&mut self, doc: &mut Value, quit_at_root: bool) -> Res<bool> {
        if !matches!(self.modal, Modal::None) {
            self.modal = Modal::None;
            return Ok(false);
        }
        if self.tab == Tab::Config && self.config_view == ConfigView::Models {
            self.commit_configure(doc)?;
            self.config_view = ConfigView::Actions;
            return Ok(false);
        }
        if self.tab != Tab::Providers {
            self.tab = Tab::Providers;
            self.focus = Focus::Menu;
            return Ok(false);
        }
        Ok(quit_at_root)
    }

    fn note(&mut self, message: impl Into<String>, error: bool) {
        self.status = Some(message.into());
        self.status_error = error;
    }

    fn on_tabs(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => self.go_tab(doc, self.tab.cycle(false))?,
            KeyCode::Right | KeyCode::Tab => self.go_tab(doc, self.tab.cycle(true))?,
            KeyCode::Char(c @ '1'..='4') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(tab) = Tab::from_key((c as u8 - b'1') as usize) {
                    self.go_tab(doc, tab)?;
                }
            }
            KeyCode::Down | KeyCode::Enter | KeyCode::Esc => self.tabs_focused = false,
            KeyCode::Char('q') | KeyCode::Char('Q') if self.tab == Tab::Providers => {
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn go_tab(&mut self, doc: &mut Value, tab: Tab) -> Res<()> {
        if self.tab == Tab::Config && self.config_view == ConfigView::Models && tab != Tab::Config {
            self.commit_configure(doc)?;
            self.config_view = ConfigView::Actions;
        }
        if matches!(tab, Tab::AddProvider | Tab::AddModel) && self.api.is_none() {
            match crate::fetch::fetch_models_dev() {
                Ok(api) => self.api = Some(api),
                Err(e) => {
                    self.modal = Modal::Error(format!("Fetch failed: {}", e.message));
                    return Ok(());
                }
            }
        }
        if tab == Tab::Config && self.provider_id.is_none() {
            self.config_view = ConfigView::Empty;
        }
        self.tab = tab;
        self.focus = Focus::Menu;
        Ok(())
    }

    fn on_providers(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        let menu = home_menu(doc);
        let selectable = selectable_indices(&menu);
        let models = enabled_models(doc, self.enabled_sort);
        let nsel = selectable.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(true),
            KeyCode::Tab => self.go_tab(doc, self.tab.cycle(true))?,
            KeyCode::BackTab => self.go_tab(doc, self.tab.cycle(false))?,
            KeyCode::Char(c @ '1'..='4') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(tab) = Tab::from_key((c as u8 - b'1') as usize) {
                    self.go_tab(doc, tab)?;
                }
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.enabled_sort = self.enabled_sort.cycle();
                self.clamp_model(models.len());
            }
            KeyCode::Up => self.move_up(&menu, &models),
            KeyCode::Down => self.move_down(&menu, &models),
            KeyCode::Home => {
                if self.focus == Focus::Models {
                    self.focus_model(0, &models);
                } else if nsel > 0 {
                    self.nav = 0;
                }
            }
            KeyCode::End => {
                if self.focus == Focus::Models {
                    self.focus_model(models.len().saturating_sub(1), &models);
                } else if nsel > 0 {
                    self.nav = nsel - 1;
                }
            }
            KeyCode::PageDown => self.page_models(&models, true),
            KeyCode::PageUp => self.page_models(&models, false),
            KeyCode::Enter | KeyCode::Right => {
                if self.focus == Focus::Models {
                    self.open_reasoning(doc, &models);
                } else if let Some(&vis) = selectable.get(self.nav) {
                    self.activate_menu(doc, &menu[vis])?;
                }
            }
            KeyCode::Left => {
                if self.focus == Focus::Models {
                    self.focus = Focus::Menu;
                    self.model_index = None;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn move_up(&mut self, menu: &[HomeItem], models: &[EnabledModel]) {
        if self.focus == Focus::Models {
            let i = self.model_index.unwrap_or(0);
            if i == 0 {
                self.focus = Focus::Menu;
                self.model_index = None;
                self.model_offset = 0;
            } else {
                self.focus_model(i - 1, models);
            }
            return;
        }
        if self.nav > 0 {
            self.nav -= 1;
        } else {
            self.tabs_focused = true;
        }
        let _ = menu;
    }

    fn move_down(&mut self, menu: &[HomeItem], models: &[EnabledModel]) {
        let nsel = selectable_indices(menu).len();
        if self.focus == Focus::Models {
            let i = self.model_index.unwrap_or(0);
            if i + 1 < models.len() {
                self.focus_model(i + 1, models);
            }
            return;
        }
        if self.nav + 1 < nsel {
            self.nav += 1;
        } else if !models.is_empty() {
            self.focus = Focus::Models;
            self.focus_model(0, models);
            self.model_offset = 0;
        }
    }

    fn focus_model(&mut self, index: usize, models: &[EnabledModel]) {
        if models.is_empty() {
            self.model_index = None;
            self.focus = Focus::Menu;
            return;
        }
        let index = index.min(models.len() - 1);
        self.model_index = Some(index);
        let page = self.page_rows.max(1);
        let max_top = models.len().saturating_sub(page);
        if index < self.model_offset {
            self.model_offset = index;
        } else if index >= self.model_offset + page {
            self.model_offset = (index + 1).saturating_sub(page).min(max_top);
        }
    }

    fn clamp_model(&mut self, len: usize) {
        if len == 0 {
            self.model_index = None;
            if self.focus == Focus::Models {
                self.focus = Focus::Menu;
            }
            self.model_offset = 0;
            return;
        }
        if let Some(i) = self.model_index {
            self.model_index = Some(i.min(len - 1));
        }
        let page = self.page_rows.max(1);
        self.model_offset = self.model_offset.min(len.saturating_sub(page));
    }

    fn page_models(&mut self, models: &[EnabledModel], down: bool) {
        if models.is_empty() {
            return;
        }
        let page = self.page_rows.max(1);
        if self.focus != Focus::Models {
            let max_top = models.len().saturating_sub(page);
            self.model_offset = if down {
                (self.model_offset + page).min(max_top)
            } else {
                self.model_offset.saturating_sub(page)
            };
            return;
        }
        let i = self.model_index.unwrap_or(0);
        let next = if down {
            (i + page).min(models.len() - 1)
        } else {
            i.saturating_sub(page)
        };
        self.focus_model(next, models);
    }

    fn activate_menu(&mut self, doc: &mut Value, item: &HomeItem) -> Res<()> {
        match item {
            HomeItem::Provider { id, .. } => {
                self.provider_id = Some(id.clone());
                self.config_view = ConfigView::Actions;
                self.action = 0;
                self.tab = Tab::Config;
            }
            HomeItem::Action { kind, .. } => self.activate_action(doc, *kind)?,
        }
        Ok(())
    }

    fn activate_action(&mut self, doc: &mut Value, kind: ActionKind) -> Res<()> {
        match kind {
            ActionKind::Codex => self.open_codex(doc),
            ActionKind::Descriptions => self.toggle_descriptions(doc)?,
            ActionKind::WebSearch => self.open_web(doc),
            ActionKind::UpdateList => self.update_list(doc)?,
            ActionKind::SyncConfig => self.sync_config(doc)?,
        }
        Ok(())
    }

    fn toggle_descriptions(&mut self, doc: &mut Value) -> Res<()> {
        let on = doc
            .get("include_descriptions")
            .and_then(Value::as_bool)
            .unwrap_or(INCLUDE_DESCRIPTIONS_DEFAULT);
        let new_val = !on;
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("include_descriptions".into(), Value::Bool(new_val));
        }
        jsonio::dump_providers(&paths::providers_path(), doc)?;
        crate::config_toml::update_config_toml()?;
        if let Ok(fresh) = jsonio::load_providers() {
            *doc = fresh;
        }
        self.changed = true;
        self.note(
            format!(
                "Model Descriptions {}",
                if new_val { "enabled" } else { "disabled" }
            ),
            false,
        );
        Ok(())
    }

    fn update_list(&mut self, doc: &mut Value) -> Res<()> {
        match crate::providers::update_providers_json() {
            Ok(response) => {
                if let Ok(fresh) = jsonio::load_providers() {
                    *doc = fresh;
                }
                let fetch_messages: Vec<&str> = response
                    .warnings
                    .iter()
                    .filter_map(|w| match w {
                        SyncWarning::LiveFetchFailed { message } => Some(message.as_str()),
                        _ => None,
                    })
                    .collect();
                let msg = if fetch_messages.len() == 1 {
                    fetch_messages[0].to_string()
                } else if fetch_messages.len() > 1 {
                    format!("{} (+{} more)", fetch_messages[0], fetch_messages.len() - 1)
                } else {
                    format!(
                        "Updated model list · {} providers synced",
                        response.providers_synced
                    )
                };
                let err = msg.starts_with("error");
                self.note(msg, err);
                self.changed = true;
            }
            Err(e) => {
                let msg = if e.message.starts_with("error ") {
                    e.message
                } else {
                    format!("error {}: fetch live model list failed", e.message)
                };
                self.note(msg, true);
            }
        }
        Ok(())
    }

    fn sync_config(&mut self, doc: &mut Value) -> Res<()> {
        match crate::config_toml::update_config_toml() {
            Ok(_) => {
                if let Ok(fresh) = jsonio::load_providers() {
                    *doc = fresh;
                }
                self.note("Synced model config", false);
            }
            Err(e) => {
                let msg = if e.message.starts_with("error ") {
                    e.message
                } else {
                    format!("error {}: sync model config failed", e.message)
                };
                self.note(msg, true);
            }
        }
        Ok(())
    }

    fn open_codex(&mut self, doc: &Value) {
        let enabled = enabled_providers(doc);
        let mut values = vec![None];
        let mut choices = vec!["disabled".to_string()];
        for p in &enabled {
            values.push(p.get("id").and_then(Value::as_str).map(|s| s.to_string()));
            choices.extend(provider_menu_labels(std::slice::from_ref(p)));
        }
        // provider_menu_labels on one row at a time mis-aligns. Build once.
        choices = vec!["▸ disabled".to_string()];
        choices.extend(
            provider_menu_labels(&enabled)
                .into_iter()
                .map(|label| format!("▸ {label}")),
        );
        let writing = doc
            .get("write_codex_config_toml")
            .and_then(Value::as_bool)
            .unwrap_or(jsonio::WRITE_CODEX_CONFIG_TOML_DEFAULT);
        let pid = jsonio::codex_model_provider_id(doc);
        let initial = if !writing || pid.is_empty() {
            0
        } else {
            values
                .iter()
                .position(|v| v.as_deref() == Some(pid.as_str()))
                .unwrap_or(0)
        };
        self.modal = Modal::Pick(Picker {
            title: "Codex Config".into(),
            info: Some(CODEX_INFO.into()),
            choices,
            values,
            selected: initial,
            kind: PickKind::Codex,
        });
    }

    fn open_web(&mut self, doc: &Value) {
        let models = jsonio::enabled_web_search_models(doc);
        let mut values = vec![None];
        let mut choices = vec!["▸ disabled".to_string()];
        for (name, key) in &models {
            values.push(Some(key.clone()));
            choices.push(format!("▸ {name}"));
        }
        let current = jsonio::web_search_id(doc);
        let initial = if current.is_empty() {
            0
        } else {
            values
                .iter()
                .position(|v| v.as_deref() == Some(current.as_str()))
                .unwrap_or(0)
        };
        self.modal = Modal::Pick(Picker {
            title: "Web Search".into(),
            info: Some(WEB_SEARCH_INFO.into()),
            choices,
            values,
            selected: initial,
            kind: PickKind::WebSearch,
        });
    }

    fn open_reasoning(&mut self, doc: &Value, models: &[EnabledModel]) {
        let Some(i) = self.model_index else { return };
        let Some(row) = models.get(i) else { return };
        let Some((labels, values, mname)) = reasoning_options(doc, &row.pid, &row.mid) else {
            self.note("No reasoning levels", false);
            return;
        };
        if values.is_empty() {
            self.note("No reasoning levels", false);
            return;
        }
        let active = reasoning_level(doc, &row.pid, &row.mid);
        let selected = values.iter().position(|v| v == &active).unwrap_or(0);
        self.modal = Modal::Pick(Picker {
            title: format!("Reasoning: {mname}"),
            info: None,
            choices: labels,
            values: values.into_iter().map(Some).collect(),
            selected,
            kind: PickKind::Reasoning {
                pid: row.pid.clone(),
                mid: row.mid.clone(),
            },
        });
    }

    fn on_config(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        if self.config_view == ConfigView::Models {
            return self.on_configure(doc, key);
        }
        match key.code {
            KeyCode::Esc => return self.interrupt(doc, false),
            KeyCode::Tab => self.go_tab(doc, self.tab.cycle(true))?,
            KeyCode::BackTab => self.go_tab(doc, self.tab.cycle(false))?,
            KeyCode::Char(c @ '1'..='4') => {
                if let Some(tab) = Tab::from_key((c as u8 - b'1') as usize) {
                    self.go_tab(doc, tab)?;
                }
            }
            KeyCode::Up if self.action == 0 => self.tabs_focused = true,
            KeyCode::Up => self.action -= 1,
            KeyCode::Down if self.action + 1 < 4 => self.action += 1,
            KeyCode::Left => {
                self.tab = Tab::Providers;
            }
            KeyCode::Enter | KeyCode::Right => self.activate_config(doc)?,
            _ => {}
        }
        Ok(false)
    }

    fn activate_config(&mut self, doc: &mut Value) -> Res<()> {
        let Some(id) = self.provider_id.clone() else {
            self.tab = Tab::Providers;
            return Ok(());
        };
        match self.action {
            0 => self.open_configure(doc, &id),
            1 => {
                let view = core::find_provider_by_id(doc, &id).unwrap_or_default();
                let enabled = !json_utils::get_bool_map(&view, "enabled");
                crate::providers::set_provider_enabled(doc, &id, enabled)?;
                crate::config_toml::update_config_toml()?;
                if let Ok(fresh) = jsonio::load_providers() {
                    *doc = fresh;
                }
                self.changed = true;
                self.note(
                    format!("Provider {}", if enabled { "enabled" } else { "disabled" }),
                    false,
                );
            }
            2 => {
                let view = core::find_provider_by_id(doc, &id).unwrap_or_default();
                let current = view
                    .get("base_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.modal = Modal::BaseUrl {
                    provider_id: id,
                    buffer: current,
                };
            }
            3 => {
                let view = core::find_provider_by_id(doc, &id).unwrap_or_default();
                self.modal = Modal::ConfirmDelete {
                    provider_id: id,
                    label: provider_display(&view),
                    yes: false,
                };
            }
            _ => {
                self.tab = Tab::Providers;
            }
        }
        Ok(())
    }

    fn open_configure(&mut self, doc: &Value, id: &str) {
        let view = core::find_provider_by_id(doc, id).unwrap_or_default();
        let ids: Vec<String> = match view.get("models") {
            Some(Value::Object(m)) => m.keys().cloned().collect(),
            _ => Vec::new(),
        };
        if ids.is_empty() {
            self.modal = Modal::Error(format!(
                "No models for '{id}'. Run a sync or re-add the provider."
            ));
            return;
        }
        let pname = view
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(id)
            .to_string();
        let models = view
            .get("models")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        self.configure = Some(ConfigureSession {
            provider_id: id.to_string(),
            pname,
            ids,
            models,
            query: String::new(),
            selected: 0,
            offset: 0,
            sort: ModelSort::Name,
            changed: false,
        });
        self.config_view = ConfigView::Models;
    }

    fn commit_configure(&mut self, doc: &mut Value) -> Res<()> {
        // Same as the existing screen: leaving Configure Models always writes
        // the models map back and flushes providers.json. Codex sync loads
        // that file; it does not use this in-memory copy.
        let Some(session) = self.configure.as_mut() else {
            return Ok(());
        };
        let id = session.provider_id.clone();
        let models = Value::Object(session.models.clone());
        if let Some(slot) = core::find_provider_by_id_mut(doc, &id) {
            slot.insert("models".into(), models);
        }
        jsonio::dump_providers(&paths::providers_path(), doc)?;
        crate::config_toml::update_config_toml()?;
        if let Ok(fresh) = jsonio::load_providers() {
            *doc = fresh;
        }
        session.changed = false;
        self.changed = true;
        Ok(())
    }

    fn on_configure(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        if self.configure.is_none() {
            self.config_view = ConfigView::Actions;
            return Ok(false);
        }
        let (rows, len, selected) = {
            let session = self.configure.as_mut().unwrap();
            let rows = configure_rows(session);
            let len = rows.len();
            if session.selected >= len && len > 0 {
                session.selected = len - 1;
            }
            (rows, len, session.selected)
        };
        let page = self.page_rows.max(1);
        match key.code {
            KeyCode::Esc => {
                self.commit_configure(doc)?;
                self.config_view = ConfigView::Actions;
            }
            KeyCode::Left if selected == 0 => {
                self.commit_configure(doc)?;
                self.config_view = ConfigView::Actions;
            }
            KeyCode::Left => {
                self.configure.as_mut().unwrap().selected = selected.saturating_sub(page);
            }
            KeyCode::Right if len > 0 => {
                let session = self.configure.as_mut().unwrap();
                session.selected = (selected + page).min(len - 1);
            }
            KeyCode::Up if selected == 0 => self.tabs_focused = true,
            KeyCode::Up => {
                self.configure.as_mut().unwrap().selected = selected - 1;
            }
            KeyCode::Down if selected + 1 < len => {
                self.configure.as_mut().unwrap().selected = selected + 1;
            }
            KeyCode::PageDown if len > 0 => {
                self.configure.as_mut().unwrap().selected = (selected + page).min(len - 1);
            }
            KeyCode::PageUp => {
                self.configure.as_mut().unwrap().selected = selected.saturating_sub(page);
            }
            KeyCode::Home => self.configure.as_mut().unwrap().selected = 0,
            KeyCode::End if len > 0 => self.configure.as_mut().unwrap().selected = len - 1,
            KeyCode::Backspace => {
                let session = self.configure.as_mut().unwrap();
                session.query.pop();
                session.selected = 0;
                session.offset = 0;
            }
            KeyCode::Char('S') => {
                let session = self.configure.as_mut().unwrap();
                session.sort = session.sort.cycle();
            }
            KeyCode::Enter => {
                let msg = rows.get(selected).cloned().map(|mid| {
                    let session = self.configure.as_mut().unwrap();
                    toggle_model_entry(&mut session.models, &mid);
                    session.changed = true;
                    let en = model_enabled(&session.models, &mid);
                    let name = model_name(&session.models, &mid);
                    format!("{} {name}", if en { "Enabled" } else { "Disabled" })
                });
                if let Some(msg) = msg {
                    self.note(msg, false);
                }
            }
            KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                let session = self.configure.as_mut().unwrap();
                session.query.push(c);
                session.selected = 0;
                session.offset = 0;
            }
            _ => {}
        }
        Ok(false)
    }

    fn on_add_provider(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        let Some(api) = self.api.clone() else {
            self.tab = Tab::Providers;
            return Ok(false);
        };
        let rows = add_provider_rows(&api, doc, &self.add_provider.query);
        self.filter_nav(key.code, rows.len(), true)?;
        if self.tab != Tab::AddProvider {
            return Ok(false);
        }
        match key.code {
            KeyCode::Enter => {
                if let Some(row) = rows.get(self.add_provider.selected) {
                    self.add_one_provider(doc, &api, &row.pid);
                }
            }
            KeyCode::Char(c) if is_filter_char(c) && c != 'S' => {
                // 'S' is not a sort key here; it still filters, including 'S'.
            }
            _ => {}
        }
        // filter_nav already consumed typing. Enter handled above.
        let _ = key;
        Ok(false)
    }

    fn on_add_model(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        let Some(api) = self.api.clone() else {
            self.tab = Tab::Providers;
            return Ok(false);
        };
        let rows = add_model_rows(&api, doc, &self.add_model.query);
        self.filter_nav(key.code, rows.len(), false)?;
        if self.tab != Tab::AddModel {
            return Ok(false);
        }
        if key.code == KeyCode::Enter {
            if let Some(row) = rows.get(self.add_model.selected).cloned() {
                self.toggle_catalog_model(doc, &api, &row)?;
            }
        }
        Ok(false)
    }

    fn on_benchmarks(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        let len = benchmarks::rows(&self.bench.query, self.bench_sort).len();
        if self.bench.selected >= len && len > 0 {
            self.bench.selected = len - 1;
        }
        let selected = self.bench.selected;
        let page = self.page_rows.max(1);
        match key.code {
            KeyCode::Esc => self.go_tab(doc, Tab::Providers)?,
            KeyCode::Tab => self.go_tab(doc, self.tab.cycle(true))?,
            KeyCode::BackTab => self.go_tab(doc, self.tab.cycle(false))?,
            KeyCode::Up if selected == 0 => self.tabs_focused = true,
            KeyCode::Up => self.bench.selected = selected - 1,
            KeyCode::Down if selected + 1 < len => self.bench.selected = selected + 1,
            KeyCode::Left if selected == 0 => self.go_tab(doc, Tab::Providers)?,
            KeyCode::Left => self.bench.selected = selected.saturating_sub(page),
            KeyCode::Right if len > 0 => self.bench.selected = (selected + page).min(len - 1),
            KeyCode::PageDown if len > 0 => {
                self.bench.selected = (selected + page).min(len - 1);
            }
            KeyCode::PageUp => self.bench.selected = selected.saturating_sub(page),
            KeyCode::Home => self.bench.selected = 0,
            KeyCode::End if len > 0 => self.bench.selected = len - 1,
            KeyCode::Backspace => {
                self.bench.query.pop();
                self.bench.selected = 0;
            }
            KeyCode::Char('S') => self.bench_sort = self.bench_sort.cycle(),
            KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                self.bench.query.push(c);
                self.bench.selected = 0;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Shared filter-list keys. `back_to_providers` closes the tab.
    fn filter_nav(&mut self, code: KeyCode, len: usize, providers: bool) -> Res<bool> {
        let filter = if providers {
            &mut self.add_provider
        } else {
            &mut self.add_model
        };
        if filter.selected >= len && len > 0 {
            filter.selected = len - 1;
        }
        match code {
            KeyCode::Esc => {
                self.tab = Tab::Providers;
                self.focus = Focus::Menu;
            }
            KeyCode::Left if filter.selected == 0 => {
                self.tab = Tab::Providers;
                self.focus = Focus::Menu;
            }
            KeyCode::Left => {
                let page = self.page_rows.max(1);
                filter.selected = filter.selected.saturating_sub(page);
            }
            KeyCode::Right if len > 0 => {
                let page = self.page_rows.max(1);
                filter.selected = (filter.selected + page).min(len - 1);
            }
            KeyCode::Up if filter.selected == 0 => self.tabs_focused = true,
            KeyCode::Up => filter.selected -= 1,
            KeyCode::Down if filter.selected + 1 < len => filter.selected += 1,
            KeyCode::PageDown if len > 0 => {
                let page = self.page_rows.max(1);
                filter.selected = (filter.selected + page).min(len - 1);
            }
            KeyCode::PageUp => {
                let page = self.page_rows.max(1);
                filter.selected = filter.selected.saturating_sub(page);
            }
            KeyCode::Home => filter.selected = 0,
            KeyCode::End if len > 0 => filter.selected = len - 1,
            KeyCode::Backspace => {
                filter.query.pop();
                filter.selected = 0;
                filter.offset = 0;
            }
            KeyCode::Tab => {
                self.tab = self.tab.cycle(true);
            }
            KeyCode::Char(c) if is_filter_char(c) => {
                filter.query.push(c);
                filter.selected = 0;
                filter.offset = 0;
            }
            _ => {}
        }
        Ok(false)
    }

    fn add_one_provider(&mut self, doc: &mut Value, api: &ModelsDev, pid: &str) {
        if added_ids(doc).contains(pid) {
            self.note(
                format!("Provider '{pid}' is already configured. Delete it from its menu."),
                false,
            );
            return;
        }
        match crate::providers::add_provider_entry(doc, api, pid) {
            Err(e) => {
                self.modal = Modal::Error(format!("Add failed: {}", e.message));
            }
            Ok(r) => {
                let msg = if r.already_present {
                    format!("Provider '{pid}' already exists.")
                } else {
                    format!(
                        "Added provider '{pid}' with {} models (all disabled).",
                        r.model_count
                    )
                };
                let msg = match r.fetch_warning_url {
                    Some(url) => crate::fetch::live_fetch_error_status(&url),
                    None => msg,
                };
                let err = msg.starts_with("error");
                self.note(msg, err);
                self.changed = true;
            }
        }
    }

    fn toggle_catalog_model(
        &mut self,
        doc: &mut Value,
        api: &ModelsDev,
        row: &CatalogModel,
    ) -> Res<()> {
        if combo_enabled(doc, &row.pid, &row.mid) {
            let Some(slot) = core::find_provider_by_id_mut(doc, &row.pid) else {
                self.modal =
                    Modal::Error(format!("Disable failed: provider {:?} missing", row.pid));
                return Ok(());
            };
            let mut disabled = false;
            if let Some(models) = slot.get_mut("models").and_then(Value::as_object_mut) {
                if let Some(m) = models.get_mut(&row.mid) {
                    if let Some(obj) = m.as_object_mut() {
                        obj.insert("enabled".into(), Value::Bool(false));
                        disabled = true;
                    }
                }
            }
            if disabled {
                jsonio::dump_providers(&paths::providers_path(), doc)?;
                self.changed = true;
                self.note(
                    format!(
                        "Disabled {} ({}) - {}/{}.",
                        row.mname, row.pname, row.pid, row.mid
                    ),
                    false,
                );
            }
            return Ok(());
        }
        let existing: Vec<String> = core::provider_entries(doc)
            .iter()
            .filter_map(|p| p.get("id").and_then(Value::as_str).map(|s| s.to_string()))
            .collect();
        let mut added = false;
        let mut fetch_warning_url = None;
        if !existing.iter().any(|e| e == &row.pid) {
            match crate::providers::add_provider_entry(doc, api, &row.pid) {
                Err(e) => {
                    self.modal = Modal::Error(format!("Add failed: {}", e.message));
                    return Ok(());
                }
                Ok(r) => {
                    added = !r.already_present;
                    fetch_warning_url = r.fetch_warning_url;
                }
            }
        }
        let Some(slot) = core::find_provider_by_id_mut(doc, &row.pid) else {
            self.modal = Modal::Error(format!("Enable failed: provider {:?} missing", row.pid));
            return Ok(());
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
            .entry(row.mid.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !m.is_object() {
            *m = Value::Object(Map::new());
        }
        m.as_object_mut()
            .unwrap()
            .insert("enabled".into(), Value::Bool(true));
        jsonio::dump_providers(&paths::providers_path(), doc)?;
        let prefix = if added {
            format!("Added provider '{}'. ", row.pid)
        } else {
            String::new()
        };
        let msg = match fetch_warning_url {
            Some(url) => crate::fetch::live_fetch_error_status(&url),
            None => format!(
                "{prefix}Enabled {} ({}) - {}/{}.",
                row.mname, row.pname, row.pid, row.mid
            ),
        };
        let err = msg.starts_with("error");
        self.note(msg, err);
        self.changed = true;
        Ok(())
    }

    fn on_modal(&mut self, doc: &mut Value, key: KeyEvent) -> Res<bool> {
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                return Ok(false);
            }
            _ => {}
        }
        match self.modal.clone() {
            Modal::None => {}
            Modal::Error(_) => {
                self.modal = Modal::None;
            }
            Modal::ConfirmDelete {
                provider_id,
                label,
                yes,
            } => {
                let mut yes = yes;
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.delete_provider(doc, &provider_id)?;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.modal = Modal::None;
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Tab => {
                        yes = !yes;
                        self.modal = Modal::ConfirmDelete {
                            provider_id,
                            label,
                            yes,
                        };
                    }
                    KeyCode::Enter => {
                        if yes {
                            self.delete_provider(doc, &provider_id)?;
                        } else {
                            self.modal = Modal::None;
                        }
                    }
                    _ => {}
                }
            }
            Modal::BaseUrl {
                provider_id,
                mut buffer,
            } => match key.code {
                KeyCode::Enter => {
                    self.save_base_url(doc, &provider_id, &buffer)?;
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    self.modal = Modal::BaseUrl {
                        provider_id,
                        buffer,
                    };
                }
                KeyCode::Char(c) if c.is_ascii_graphic() => {
                    buffer.push(c);
                    self.modal = Modal::BaseUrl {
                        provider_id,
                        buffer,
                    };
                }
                _ => {}
            },
            Modal::Pick(mut picker) => match key.code {
                KeyCode::Up if picker.selected > 0 => {
                    picker.selected -= 1;
                    self.modal = Modal::Pick(picker);
                }
                KeyCode::Down if picker.selected + 1 < picker.choices.len() => {
                    picker.selected += 1;
                    self.modal = Modal::Pick(picker);
                }
                KeyCode::Enter | KeyCode::Right => self.apply_pick(doc, &picker)?,
                KeyCode::Left => {
                    self.modal = Modal::None;
                }
                KeyCode::Home => {
                    picker.selected = 0;
                    self.modal = Modal::Pick(picker);
                }
                KeyCode::End if !picker.choices.is_empty() => {
                    picker.selected = picker.choices.len() - 1;
                    self.modal = Modal::Pick(picker);
                }
                _ => {}
            },
        }
        Ok(false)
    }

    fn delete_provider(&mut self, doc: &mut Value, provider_id: &str) -> Res<()> {
        crate::providers::delete_provider_and_flush(doc, provider_id)?;
        self.changed = true;
        self.modal = Modal::None;
        self.provider_id = None;
        self.config_view = ConfigView::Empty;
        self.tab = Tab::Providers;
        self.nav = 0;
        self.focus = Focus::Menu;
        self.note(format!("Deleted provider {provider_id}"), false);
        Ok(())
    }

    fn save_base_url(&mut self, doc: &mut Value, provider_id: &str, value: &str) -> Res<()> {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            if let Some(slot) = core::find_provider_by_id_mut(doc, provider_id) {
                slot.remove("base_url");
            }
        } else if let Some(slot) = core::find_provider_by_id_mut(doc, provider_id) {
            slot.insert("base_url".into(), Value::String(trimmed));
        }
        jsonio::dump_providers(&paths::providers_path(), doc)?;
        crate::config_toml::update_config_toml()?;
        if let Ok(fresh) = jsonio::load_providers() {
            *doc = fresh;
        }
        self.changed = true;
        self.modal = Modal::None;
        self.note("Base URL saved", false);
        Ok(())
    }

    fn apply_pick(&mut self, doc: &mut Value, picker: &Picker) -> Res<()> {
        let sel = picker.values.get(picker.selected).cloned().flatten();
        match &picker.kind {
            PickKind::Codex => {
                apply_codex(doc, sel.as_deref())?;
                if let Ok(fresh) = jsonio::load_providers() {
                    *doc = fresh;
                }
                self.changed = true;
                self.note(
                    format!("Codex Config {}", jsonio::codex_status_token(doc)),
                    false,
                );
            }
            PickKind::WebSearch => {
                jsonio::set_web_search(doc, sel.as_deref());
                let _ = jsonio::dump_providers(&paths::providers_path(), doc);
                let _ = crate::config_toml::update_config_toml();
                if let Ok(fresh) = jsonio::load_providers() {
                    *doc = fresh;
                }
                self.changed = true;
                self.note(
                    format!("Web Search {}", jsonio::web_search_status_token(doc)),
                    false,
                );
            }
            PickKind::Reasoning { pid, mid } => {
                let Some(chosen) = sel else {
                    self.modal = Modal::None;
                    return Ok(());
                };
                apply_reasoning(doc, pid, mid, &chosen)?;
                self.changed = true;
                self.note(format!("Reasoning set to {chosen}"), false);
            }
        }
        self.modal = Modal::None;
        Ok(())
    }
}

fn apply_codex(doc: &mut Value, sel: Option<&str>) -> Res<()> {
    let previous = jsonio::codex_model_provider_id(doc);
    let is_switch = sel.is_some()
        && !previous.is_empty()
        && previous != "disabled"
        && sel != Some(previous.as_str());
    if is_switch {
        jsonio::set_codex_selection(doc, None);
        let _ = jsonio::dump_providers(&paths::providers_path(), doc);
        let _ = crate::config_toml::update_config_toml();
        jsonio::set_codex_selection(doc, sel);
        let _ = jsonio::dump_providers(&paths::providers_path(), doc);
        let _ = crate::config_toml::update_config_toml();
    } else {
        jsonio::set_codex_selection(doc, sel);
        let _ = jsonio::dump_providers(&paths::providers_path(), doc);
        let _ = crate::config_toml::update_config_toml();
    }
    Ok(())
}

fn apply_reasoning(doc: &mut Value, pid: &str, mid: &str, chosen: &str) -> Res<()> {
    let Some(slot) = core::find_provider_by_id_mut(doc, pid) else {
        return fail(format!("provider {pid} missing"));
    };
    let Some(m) = slot
        .get_mut("models")
        .and_then(Value::as_object_mut)
        .and_then(|mm| mm.get_mut(mid))
        .and_then(Value::as_object_mut)
    else {
        return fail(format!("model {mid} missing"));
    };
    m.insert("reasoning_effort".into(), Value::String(chosen.to_string()));
    if let Some(arr) = m.get_mut("reasoning_efforts").and_then(Value::as_array_mut) {
        for row in arr {
            if let Some(obj) = row.as_object_mut() {
                let is = obj.get("value").and_then(Value::as_str) == Some(chosen);
                obj.insert("default".into(), Value::Bool(is));
            }
        }
    }
    jsonio::dump_providers(&paths::providers_path(), doc)?;
    crate::config_toml::update_config_toml()?;
    if let Ok(fresh) = jsonio::load_providers() {
        *doc = fresh;
    }
    Ok(())
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && y >= rect.y
        && x < rect.x.saturating_add(rect.width)
        && y < rect.y.saturating_add(rect.height)
}

/// `rect` is the scrollbar column, including the arrow caps at each end.
/// The result is the first visible row, so the thumb stays under the pointer.
fn track_offset(rect: Rect, y: u16, len: usize, page: usize) -> usize {
    let page = page.max(1);
    let max_top = len.saturating_sub(page);
    if max_top == 0 || rect.height <= 2 {
        return 0;
    }
    let last = rect.y.saturating_add(rect.height).saturating_sub(1);
    if y <= rect.y {
        return 0;
    }
    if y >= last {
        return max_top;
    }
    let track = (rect.height - 2) as usize;
    let pos = y.saturating_sub(rect.y).saturating_sub(1) as usize;
    let pos = pos.min(track.saturating_sub(1));
    pos * max_top / track.saturating_sub(1).max(1)
}

/// Data-row index whose visual row is `visual`, skipping separator rows.
fn data_for_visual(visual: usize, seps: &[(usize, SepTone)], data_len: usize) -> usize {
    if data_len == 0 {
        return 0;
    }
    let mut best = 0;
    for data in 0..data_len {
        let shown = visual_selected(data, seps, data_len).unwrap_or(0);
        if shown <= visual {
            best = data;
        } else {
            break;
        }
    }
    best
}

fn is_filter_char(c: char) -> bool {
    c.is_ascii_graphic() || c == ' '
}

fn toggle_model_entry(models: &mut Map<String, Value>, mid: &str) {
    let entry = models
        .entry(mid.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    let cur = get_bool_value(entry, "enabled");
    entry
        .as_object_mut()
        .unwrap()
        .insert("enabled".into(), Value::Bool(!cur));
}

fn model_enabled(models: &Map<String, Value>, mid: &str) -> bool {
    models
        .get(mid)
        .is_some_and(|v| v.is_object() && get_bool_value(v, "enabled"))
}

fn model_name(models: &Map<String, Value>, mid: &str) -> String {
    models
        .get(mid)
        .map(|v| get_name_or(v, mid))
        .unwrap_or_else(|| mid.to_string())
}

fn is_free(mid: &str) -> bool {
    mid.to_lowercase().contains("free")
}

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
            .is_some_and(|m| m.is_object() && get_bool_value(m, "enabled"));
    }
    false
}

fn added_ids(doc: &Value) -> HashSet<String> {
    core::provider_entries(doc)
        .iter()
        .filter_map(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .collect()
}

fn enabled_providers(doc: &Value) -> Vec<Map<String, Value>> {
    doc.get("providers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|p| {
                    p.is_object()
                        && p.get("id").is_some()
                        && p.get("enabled").and_then(Value::as_bool).unwrap_or(true)
                })
                .filter_map(|p| p.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

fn reasoning_level(doc: &Value, pid: &str, mid: &str) -> String {
    let Some(arr) = doc.get("providers").and_then(Value::as_array) else {
        return "none".into();
    };
    for p in arr {
        if p.get("id").and_then(Value::as_str) != Some(pid) {
            continue;
        }
        let Some(m) = p
            .get("models")
            .and_then(Value::as_object)
            .and_then(|mm| mm.get(mid))
        else {
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

fn reasoning_options(
    doc: &Value,
    pid: &str,
    mid: &str,
) -> Option<(Vec<String>, Vec<String>, String)> {
    let models = doc
        .get("providers")?
        .as_array()?
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))?
        .get("models")?
        .as_object()?;
    let m = models.get(mid)?;
    let efforts = m.get("reasoning_efforts").and_then(Value::as_array)?;
    if efforts.is_empty() {
        return None;
    }
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
        return None;
    }
    Some((labels, values, get_name_or(m, mid)))
}

fn score_of(mid: &str) -> Option<&'static Scores> {
    benchmarks::scores_for_live_id(mid)
}

fn score_desc(a: &str, b: &str, intel: bool) -> Ordering {
    let pick = |s: &Scores| if intel { s.intel } else { s.coding };
    let av = score_of(a).map(pick).unwrap_or(f32::NEG_INFINITY);
    let bv = score_of(b).map(pick).unwrap_or(f32::NEG_INFINITY);
    bv.partial_cmp(&av).unwrap_or(Ordering::Equal)
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// --- view models -----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionKind {
    Codex,
    Descriptions,
    WebSearch,
    UpdateList,
    SyncConfig,
}

#[derive(Clone, Debug)]
pub(crate) enum HomeItem {
    Provider { id: String, label: String },
    Action { kind: ActionKind, label: String },
}

#[derive(Clone, Debug)]
pub(crate) struct SpanText {
    pub text: String,
    pub tone: Tone,
    /// Keep this span's colors when the row is highlighted (env-var chip).
    pub protect: bool,
}

fn st(text: impl Into<String>, tone: Tone) -> SpanText {
    SpanText {
        text: text.into(),
        tone,
        protect: false,
    }
}

fn locked(text: impl Into<String>, tone: Tone) -> SpanText {
    let mut span = st(text, tone);
    span.protect = true;
    span
}

#[derive(Clone, Debug)]
pub(crate) struct MenuLine {
    pub spans: Vec<SpanText>,
    pub selected: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct EnabledModel {
    pub pid: String,
    pub mid: String,
    pub name: String,
    pub provider: String,
    pub intel: Option<f32>,
    pub coding: Option<f32>,
    pub level: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SepTone {
    Green,
    Cyan,
}

#[derive(Clone, Debug)]
pub(crate) enum GridRow {
    Sep(SepTone),
    Cells(Vec<SpanText>),
}

#[derive(Clone, Debug)]
pub(crate) struct Col {
    pub label: String,
    pub hot: bool,
    pub right: bool,
    pub width: u16,
    pub fill: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Grid {
    pub title: String,
    pub query: Option<String>,
    pub headers: Vec<Col>,
    pub rows: Vec<GridRow>,
    pub selected: Option<usize>,
}

#[derive(Clone, Debug)]
struct CatalogModel {
    pid: String,
    mid: String,
    mname: String,
    pname: String,
}

pub(crate) fn home_menu(doc: &Value) -> Vec<HomeItem> {
    let ordered = core::provider_entries(doc);
    let labels = provider_menu_labels(&ordered);
    let token_col = provider_state_token_col(&ordered);
    let mut items = Vec::new();
    for (p, label) in ordered.iter().zip(labels) {
        let id = p
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        items.push(HomeItem::Provider {
            id,
            label: format!("▸ {label}"),
        });
    }
    let descriptions_on = doc
        .get("include_descriptions")
        .and_then(Value::as_bool)
        .unwrap_or(INCLUDE_DESCRIPTIONS_DEFAULT);
    let push_state = |items: &mut Vec<HomeItem>, kind: ActionKind, label: &str, token: &str| {
        items.push(HomeItem::Action {
            kind,
            label: format!("▸ {}", pad_state_label(label, token, token_col)),
        });
    };
    push_state(
        &mut items,
        ActionKind::Codex,
        CODEX_CONFIG_LABEL,
        &format!("[{}]", jsonio::codex_status_token(doc)),
    );
    push_state(
        &mut items,
        ActionKind::Descriptions,
        MODEL_DESC_LABEL,
        &format!(
            "[{}]",
            if descriptions_on {
                "enabled"
            } else {
                "disabled"
            }
        ),
    );
    push_state(
        &mut items,
        ActionKind::WebSearch,
        WEB_SEARCH_LABEL,
        &format!("[{}]", jsonio::web_search_status_token(doc)),
    );
    match doc.get("last_updated").and_then(Value::as_str) {
        Some(ts) if !ts.is_empty() => push_state(
            &mut items,
            ActionKind::UpdateList,
            UPDATE_LIST_LABEL,
            &format!("[{ts}]"),
        ),
        _ => items.push(HomeItem::Action {
            kind: ActionKind::UpdateList,
            label: format!("▸ {UPDATE_LIST_LABEL}"),
        }),
    }
    match doc.get("last_synced").and_then(Value::as_str) {
        Some(ts) if !ts.is_empty() => push_state(
            &mut items,
            ActionKind::SyncConfig,
            SYNC_CONFIG_LABEL,
            &format!("[{ts}]"),
        ),
        _ => items.push(HomeItem::Action {
            kind: ActionKind::SyncConfig,
            label: format!("▸ {SYNC_CONFIG_LABEL}"),
        }),
    }
    items
}

fn selectable_indices(items: &[HomeItem]) -> Vec<usize> {
    (0..items.len()).collect()
}

pub(crate) fn menu_section_lines(items: &[HomeItem], selected: Option<usize>) -> Vec<MenuLine> {
    let mut lines: Vec<MenuLine> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let label = match item {
                HomeItem::Provider { label, .. } | HomeItem::Action { label, .. } => label,
            };
            MenuLine {
                spans: color_label(label),
                selected: Some(i) == selected,
            }
        })
        .collect();
    equalize_env_chips(&mut lines);
    lines
}

/// Black env chips share one width, with a column of padding on each side,
/// matching the main-menu env box. Name padding before `=` is left intact.
fn equalize_env_chips(lines: &mut [MenuLine]) {
    let side = PROVIDER_ENV_PAD.max(0) as usize;
    let mut max_inner = 0usize;
    for line in lines.iter() {
        let width = protected_cols(line);
        if width > 0 {
            max_inner = max_inner.max(width);
        }
    }
    if max_inner == 0 {
        return;
    }
    let target = max_inner + side * 2;
    for line in lines.iter_mut() {
        let width = protected_cols(line);
        if width == 0 {
            continue;
        }
        let Some(at) = line.spans.iter().position(|span| span.protect) else {
            continue;
        };
        if side > 0 {
            line.spans.insert(at, locked(" ".repeat(side), Tone::Code));
        }
        let extra = target - width - side;
        if extra > 0 {
            line.spans.push(locked(" ".repeat(extra), Tone::Code));
        }
    }
}

fn protected_cols(line: &MenuLine) -> usize {
    line.spans
        .iter()
        .filter(|span| span.protect)
        .map(|span| span.text.chars().count())
        .sum()
}

/// Update and sync stamps look like `[01-02-2026 03:04 PM]`.
fn is_timestamp(token: &str) -> bool {
    let inner = token
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("");
    let mut parts = inner.split_whitespace();
    let date = parts.next().unwrap_or("");
    let time = parts.next().unwrap_or("");
    let ampm = parts.next().unwrap_or("");
    date.len() == 10
        && date.as_bytes().get(2) == Some(&b'-')
        && date.as_bytes().get(5) == Some(&b'-')
        && time.len() == 5
        && time.as_bytes().get(2) == Some(&b':')
        && (ampm == "AM" || ampm == "PM")
}

fn color_label(label: &str) -> Vec<SpanText> {
    let (head, token, tail) = split_token(label);
    let gap = if head.starts_with('▸') { " " } else { "  " };
    let mut spans = vec![st(format!("{gap}{head}"), Tone::Text)];
    if let Some(token) = token {
        let tone = if token == "[enabled]" || is_timestamp(&token) {
            Tone::Green
        } else if token == "[disabled]" {
            Tone::Red
        } else {
            // Codex and Web Search show a name here, in the normal foreground.
            Tone::Text
        };
        spans.push(st(token, tone));
    }
    if !tail.is_empty() {
        let gap = tail.len() - tail.trim_start().len();
        if gap > 0 {
            spans.push(st(" ".repeat(gap), Tone::Text));
        }
        let env = &tail[gap..];
        if env.contains('=') {
            spans.extend(
                shell_spans(env)
                    .into_iter()
                    .map(|(text, tone)| locked(text, tone)),
            );
        } else if !env.is_empty() {
            spans.push(st(env, Tone::Muted));
        }
    }
    spans
}

/// Shell colors for an env assignment (`NAME = "value"`), kept on a black chip.
fn shell_spans(src: &str) -> Vec<(String, Tone)> {
    if src.trim_start().starts_with('#') {
        return vec![(src.to_string(), Tone::CodeComment)];
    }
    let Some(eq) = src.find('=') else {
        return vec![(src.to_string(), Tone::Code)];
    };
    // Keep the spaces that pad the name out to the widest env key, so `=`
    // lands in the same column on every provider row.
    let name_field = &src[..eq];
    let name = name_field.trim_end();
    let name_pad = &name_field[name.len()..];
    let after_field = &src[eq + 1..];
    let after_pad = after_field.len() - after_field.trim_start().len();
    let after = &after_field[after_pad..];
    let name_tone = if after == "\"\"" {
        Tone::Red
    } else {
        Tone::Code
    };
    let mut eq_span = String::from(name_pad);
    eq_span.push('=');
    eq_span.push_str(&" ".repeat(after_pad));
    let mut out = vec![(name.to_string(), name_tone), (eq_span, Tone::CodeSymbol)];
    if let Some(rest) = after.strip_prefix('"') {
        out.push(("\"".into(), Tone::CodeSymbol));
        if let Some(end) = rest.find('"') {
            out.push((rest[..end].to_string(), Tone::CodeString));
            out.push(("\"".into(), Tone::CodeSymbol));
            let more = &rest[end + 1..];
            if !more.is_empty() {
                out.push((more.to_string(), Tone::Code));
            }
        } else {
            out.push((rest.to_string(), Tone::CodeString));
        }
    } else if !after.is_empty() {
        out.push((after.to_string(), Tone::Code));
    }
    out.retain(|(text, _)| !text.is_empty());
    out
}

fn split_token(s: &str) -> (String, Option<String>, String) {
    if let Some(start) = s.find('[') {
        if let Some(rel) = s[start..].find(']') {
            let end = start + rel;
            return (
                s[..start].to_string(),
                Some(s[start..=end].to_string()),
                s[end + 1..].to_string(),
            );
        }
    }
    (s.to_string(), None, String::new())
}

pub(crate) fn enabled_models(doc: &Value, sort: EnabledSort) -> Vec<EnabledModel> {
    let mut rows = Vec::new();
    for provider in core::provider_entries(doc) {
        if !json_utils::get_bool_map(&provider, "enabled") {
            continue;
        }
        let pid = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if pid.is_empty() {
            continue;
        }
        let pname = provider
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(pid.as_str())
            .to_string();
        let Some(mm) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (mid, m) in mm {
            if !m.is_object() || !get_bool_value(m, "enabled") {
                continue;
            }
            let scores = score_of(mid);
            rows.push(EnabledModel {
                name: clip(&get_name_or(m, mid), MODEL_NAME_COL_MAX),
                provider: clip(&pname, PROVIDER_NAME_COL_MAX),
                intel: scores.map(|s| s.intel),
                coding: scores.map(|s| s.coding),
                level: reasoning_level(doc, &pid, mid),
                pid: pid.clone(),
                mid: mid.clone(),
            });
        }
    }
    rows.sort_by(|a, b| match sort {
        EnabledSort::Model => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.provider.to_lowercase().cmp(&b.provider.to_lowercase()))
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| a.mid.cmp(&b.mid)),
        EnabledSort::Provider => a
            .provider
            .to_lowercase()
            .cmp(&b.provider.to_lowercase())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| a.mid.cmp(&b.mid)),
        EnabledSort::Intel => score_desc(&a.mid, &b.mid, true)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        EnabledSort::Coding => score_desc(&a.mid, &b.mid, false)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
    });
    rows
}

pub(crate) fn enabled_grid(doc: &Value, app: &App) -> Grid {
    let models = enabled_models(doc, app.enabled_sort);
    let hot = |sort| app.enabled_sort == sort;
    // "● " is two columns. The header sits on the name, not the bullet.
    let name_header = "  Models";
    let name_w = capped_width(
        name_header,
        models
            .iter()
            .map(|m| m.name.chars().count() + 2)
            .max()
            .unwrap_or(0),
        MODEL_NAME_COL_MAX + 2,
    );
    let prov_w = capped_width(
        "Provider",
        models
            .iter()
            .map(|m| m.provider.chars().count())
            .max()
            .unwrap_or(0),
        PROVIDER_NAME_COL_MAX,
    );
    let headers = vec![
        Col {
            label: name_header.into(),
            hot: hot(EnabledSort::Model),
            right: false,
            width: name_w,
            fill: false,
        },
        Col {
            label: "Intel".into(),
            hot: hot(EnabledSort::Intel),
            right: true,
            width: INTEL_COL_W as u16,
            fill: false,
        },
        Col {
            label: "Coding".into(),
            hot: hot(EnabledSort::Coding),
            right: true,
            width: CODING_COL_W as u16,
            fill: false,
        },
        Col {
            label: "Default".into(),
            hot: false,
            right: false,
            width: 12,
            fill: false,
        },
        Col {
            label: "Provider".into(),
            hot: hot(EnabledSort::Provider),
            right: false,
            width: prov_w,
            fill: false,
        },
    ];
    let rows: Vec<GridRow> = models
        .iter()
        .map(|m| {
            let score = |v: Option<f32>, w: usize| match v {
                Some(n) => SpanText {
                    text: format!("{n:>w$.1}"),
                    tone: Tone::Blue,
                    protect: false,
                },
                None => SpanText {
                    text: " ".repeat(w),
                    tone: Tone::Muted,
                    protect: false,
                },
            };
            let level_tone = if m.level == "none" {
                Tone::Muted
            } else {
                Tone::Cyan
            };
            GridRow::Cells(vec![
                SpanText {
                    text: format!("● {}", m.name),
                    tone: Tone::Green,
                    protect: false,
                },
                score(m.intel, INTEL_COL_W),
                score(m.coding, CODING_COL_W),
                SpanText {
                    text: format!("({})", m.level),
                    tone: level_tone,
                    protect: false,
                },
                SpanText {
                    text: m.provider.clone(),
                    tone: Tone::Text,
                    protect: false,
                },
            ])
        })
        .collect();
    let rows = if rows.is_empty() {
        vec![GridRow::Cells(vec![SpanText {
            text: "No enabled models. Enable with --enable or grok-models".into(),
            tone: Tone::Muted,
            protect: false,
        }])]
    } else {
        rows
    };
    let selected = if app.focus == Focus::Models {
        app.model_index
    } else {
        None
    };
    Grid {
        title: format!("Enabled Models  {}", models.len()),
        query: None,
        headers,
        rows,
        selected,
    }
}

pub(crate) fn provider_detail(doc: &Value, id: &str) -> ProviderDetail {
    let view = core::find_provider_by_id(doc, id).unwrap_or_default();
    let name = view
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_string();
    let enabled = json_utils::get_bool_map(&view, "enabled");
    let base_url = view
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let doc_url = view
        .get("doc")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let env_key = core::provider_env_key_from_json(&view);
    let masked = if env_key.is_empty() {
        None
    } else {
        Some(vars::env_key_masked_display(&env_key))
    };
    let setup = if env_key.is_empty() {
        None
    } else {
        Some(format!(
            "# config {id} api keys\npbpaste > key-file\necho 'export {env_key}=\"$(cat ~/key-file)\"' >> ~/.zshrc"
        ))
    };
    ProviderDetail {
        name,
        enabled,
        base_url,
        doc_url,
        masked,
        setup,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderDetail {
    pub name: String,
    pub enabled: bool,
    pub base_url: String,
    pub doc_url: String,
    pub masked: Option<String>,
    pub setup: Option<String>,
}

pub(crate) fn config_actions(doc: &Value, id: &str, selected: usize) -> Vec<MenuLine> {
    let detail = provider_detail(doc, id);
    let rows = [
        "▸ Configure Models".to_string(),
        format!(
            "▸ Provider [{}]",
            if detail.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        format!("▸ Base Url [{}]", detail.base_url),
        "▸ Delete Provider".to_string(),
    ];
    rows.into_iter()
        .enumerate()
        .map(|(i, label)| MenuLine {
            spans: color_label(&label),
            selected: i == selected,
        })
        .collect()
}

fn configure_rows(session: &ConfigureSession) -> Vec<String> {
    let sorted: SortedIndices =
        core::sort_model_indices(&session.ids, &session.models, Some(&session.query));
    let mut ordered: Vec<String> = sorted
        .filtered
        .iter()
        .map(|&i| session.ids[i].clone())
        .collect();
    reorder_groups(&mut ordered, &session.models, session.sort);
    ordered
}

fn reorder_groups(ordered: &mut [String], models: &Map<String, Value>, sort: ModelSort) {
    let intel = match sort {
        ModelSort::Intel => true,
        ModelSort::Coding => false,
        ModelSort::Name => return,
    };
    let group = |mid: &str| -> u8 {
        if model_enabled(models, mid) {
            0
        } else if is_free(mid) {
            1
        } else {
            2
        }
    };
    let mut i = 0;
    while i < ordered.len() {
        let g = group(&ordered[i]);
        let start = i;
        i += 1;
        while i < ordered.len() && group(&ordered[i]) == g {
            i += 1;
        }
        ordered[start..i].sort_by(|a, b| {
            score_desc(a, b, intel).then_with(|| {
                model_name(models, a)
                    .to_lowercase()
                    .cmp(&model_name(models, b).to_lowercase())
            })
        });
    }
}

fn sep_before(enabled_count: usize, free_disabled: usize, len: usize) -> Vec<(usize, SepTone)> {
    let mut seps = Vec::new();
    if enabled_count > 0 && enabled_count < len {
        seps.push((enabled_count, SepTone::Green));
    }
    let free_at = enabled_count + free_disabled;
    if free_disabled > 0 && free_at < len {
        seps.push((free_at, SepTone::Cyan));
    }
    seps
}

fn stitch<T: Clone>(
    rows: &[T],
    seps: &[(usize, SepTone)],
    map: impl Fn(&T) -> Vec<SpanText>,
) -> (Vec<GridRow>, Option<usize>, usize) {
    let mut out = Vec::new();
    let mut si = 0;
    for (i, row) in rows.iter().enumerate() {
        while si < seps.len() && seps[si].0 == i {
            out.push(GridRow::Sep(seps[si].1));
            si += 1;
        }
        out.push(GridRow::Cells(map(row)));
    }
    (out, None, 0)
}

fn configure_grid(session: &ConfigureSession) -> Grid {
    let sorted = core::sort_model_indices(&session.ids, &session.models, Some(&session.query));
    let enabled_count = sorted.enabled_count;
    let free_count = sorted.free_disabled_count;
    let mids = configure_rows(session);
    let seps = sep_before(enabled_count, free_count, mids.len());
    let (rows, _, _) = stitch(&mids, &seps, |mid| {
        let enabled = model_enabled(&session.models, mid);
        let name = clip(&model_name(&session.models, mid), MODEL_NAME_COL_MAX);
        let name_tone = if enabled {
            Tone::Green
        } else if is_free(mid) {
            Tone::Cyan
        } else {
            Tone::Text
        };
        let scores = score_of(mid);
        let score = |v: Option<f32>, w: usize| match v {
            Some(n) => SpanText {
                text: format!("{n:>w$.1}"),
                tone: Tone::Blue,
                protect: false,
            },
            None => SpanText {
                text: " ".repeat(w),
                tone: Tone::Muted,
                protect: false,
            },
        };
        let state = if enabled { "[enabled]" } else { "[disabled]" };
        vec![
            SpanText {
                text: name,
                tone: name_tone,
                protect: false,
            },
            score(scores.map(|s| s.intel), INTEL_COL_W),
            score(scores.map(|s| s.coding), CODING_COL_W),
            SpanText {
                text: format!("({})", clip(&session.pname, PROVIDER_NAME_COL_MAX)),
                tone: Tone::Text,
                protect: false,
            },
            SpanText {
                text: state.into(),
                tone: if enabled { Tone::Green } else { Tone::Red },
                protect: false,
            },
        ]
    });
    let selected = visual_selected(session.selected, &seps, mids.len());
    let name_w = capped_width(
        "Model",
        session
            .ids
            .iter()
            .map(|mid| model_name(&session.models, mid).chars().count())
            .max()
            .unwrap_or(0),
        MODEL_NAME_COL_MAX,
    );
    let prov_w = capped_width(
        "Provider",
        session.pname.chars().count() + 2,
        PROVIDER_NAME_COL_MAX + 2,
    );
    let mut headers = model_headers(session.sort.hot_col(), name_w);
    headers.push(Col {
        label: "Provider".into(),
        hot: false,
        right: false,
        width: prov_w,
        fill: false,
    });
    headers.push(Col {
        label: "Mode".into(),
        hot: false,
        right: false,
        width: MODE_COL_W as u16,
        fill: false,
    });
    Grid {
        // `(n)` is padded to 3 digits so `Search:` does not slide left.
        title: format!("Configure Models  {:<5}", format!("({})", mids.len())),
        query: Some(session.query.clone()),
        headers,
        rows,
        selected,
    }
}

/// Keep `selected` inside the visible page without pinning it to the bottom.
/// Scrolling up moves the highlight until it reaches the top row, then the page follows.
pub(crate) fn reveal_offset(
    offset: usize,
    selected: Option<usize>,
    len: usize,
    page: usize,
) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    if len == 0 {
        return 0;
    }
    let page = page.max(1);
    let selected = selected.min(len - 1);
    let max_top = len.saturating_sub(page);
    let offset = if selected < offset {
        selected
    } else if selected >= offset.saturating_add(page) {
        (selected + 1).saturating_sub(page)
    } else {
        offset
    };
    offset.min(max_top)
}

fn capped_width(header: &str, widest: usize, max: usize) -> u16 {
    header.chars().count().max(widest).min(max).max(1) as u16
}

fn model_headers(hot: usize, name_w: u16) -> Vec<Col> {
    vec![
        Col {
            label: "Model".into(),
            hot: hot == 0,
            right: false,
            width: name_w,
            fill: false,
        },
        Col {
            label: "Intel".into(),
            hot: hot == 1,
            right: true,
            width: INTEL_COL_W as u16,
            fill: false,
        },
        Col {
            label: "Coding".into(),
            hot: hot == 2,
            right: true,
            width: CODING_COL_W as u16,
            fill: false,
        },
    ]
}

fn visual_selected(row: usize, seps: &[(usize, SepTone)], len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let row = row.min(len - 1);
    let extra = seps.iter().filter(|(i, _)| *i <= row).count();
    Some(row + extra)
}

#[derive(Clone, Debug)]
struct ProviderRow {
    pid: String,
}

fn add_provider_rows(api: &ModelsDev, doc: &Value, query: &str) -> Vec<ProviderRow> {
    let term = query.to_lowercase();
    let added = added_ids(doc);
    let mut rows: Vec<(usize, ProviderRow)> = api
        .providers
        .iter()
        .filter(|(pid, provider)| {
            let name = provider.name.as_deref().unwrap_or("");
            term.is_empty()
                || pid.to_lowercase().contains(&term)
                || name.to_lowercase().contains(&term)
        })
        .map(|(pid, _provider)| {
            let bucket = if added.contains(pid) {
                0
            } else if SUGGESTED.contains(&pid.as_str()) {
                1
            } else {
                2
            };
            (bucket, ProviderRow { pid: pid.clone() })
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.pid.cmp(&b.1.pid)));
    rows.into_iter().map(|(_, row)| row).collect()
}

pub(crate) fn add_provider_grid(
    api: &ModelsDev,
    doc: &Value,
    filter: &str,
    selected: usize,
) -> Grid {
    let added = added_ids(doc);
    let rows = add_provider_rows(api, doc, filter);
    let n_added = rows.iter().filter(|r| added.contains(&r.pid)).count();
    let n_sugg = rows
        .iter()
        .filter(|r| !added.contains(&r.pid) && SUGGESTED.contains(&r.pid.as_str()))
        .count();
    let seps = sep_before(n_added, n_sugg, rows.len());
    let configured = core::provider_entries(doc);
    let shown: Vec<(String, String, bool, Tone)> = rows
        .iter()
        .map(|row| {
            let cat_name = api
                .providers
                .get(&row.pid)
                .and_then(|p| p.name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| row.pid.clone());
            let found = configured
                .iter()
                .find(|p| p.get("id").and_then(Value::as_str) == Some(row.pid.as_str()));
            let name = found
                .and_then(|p| p.get("name").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .unwrap_or(&cat_name);
            let enabled = found
                .map(|p| json_utils::get_bool_map(p, "enabled"))
                .unwrap_or(false);
            let tone = if added.contains(&row.pid) {
                Tone::Green
            } else if SUGGESTED.contains(&row.pid.as_str()) {
                Tone::Cyan
            } else {
                Tone::Text
            };
            (paren_name(name), row.pid.clone(), enabled, tone)
        })
        .collect();
    let all = add_provider_rows(api, doc, "");
    let name_w = capped_width(
        "Provider",
        all.iter()
            .map(|row| provider_paren_width(api, &configured, row).chars().count())
            .max()
            .unwrap_or(0),
        PROVIDER_NAME_COL_MAX + 2,
    );
    let id_w = capped_width(
        "Provider ID",
        all.iter()
            .map(|row| row.pid.chars().count())
            .max()
            .unwrap_or(0),
        PROVIDER_ID_COL_MAX,
    );
    let (grid_rows, _, _) = stitch(&shown, &seps, |(name, pid, enabled, tone)| {
        vec![
            st(name.clone(), *tone),
            st(pid.clone(), Tone::Text),
            st(
                if *enabled { "[enabled]" } else { "[disabled]" },
                if *enabled { Tone::Green } else { Tone::Red },
            ),
        ]
    });
    Grid {
        title: format!("Add Provider  {:<6}", format!("({})", rows.len())),
        query: Some(filter.to_string()),
        headers: vec![
            Col {
                label: "Provider".into(),
                hot: false,
                right: false,
                width: name_w,
                fill: false,
            },
            Col {
                label: "Provider ID".into(),
                hot: false,
                right: false,
                width: id_w,
                fill: false,
            },
            Col {
                label: "Mode".into(),
                hot: false,
                right: false,
                width: MODE_COL_W as u16,
                fill: false,
            },
        ],
        rows: grid_rows,
        selected: visual_selected(selected, &seps, rows.len()),
    }
}

fn paren_name(name: &str) -> String {
    format!("({})", clip(name, PROVIDER_NAME_COL_MAX))
}

fn provider_paren_width(
    api: &ModelsDev,
    configured: &[Map<String, Value>],
    row: &ProviderRow,
) -> String {
    let cat_name = api
        .providers
        .get(&row.pid)
        .and_then(|p| p.name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| row.pid.clone());
    let found = configured
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(row.pid.as_str()));
    let name = found
        .and_then(|p| p.get("name").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .unwrap_or(&cat_name);
    paren_name(name)
}

fn add_model_rows(api: &ModelsDev, doc: &Value, query: &str) -> Vec<CatalogModel> {
    let mut catalog = Vec::new();
    let mut seen = HashSet::new();
    for (pid, provider) in &api.providers {
        let pname = provider
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(pid);
        for (mid, info) in &provider.models {
            let mname = info
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(mid);
            catalog.push(CatalogModel {
                pid: pid.clone(),
                mid: mid.clone(),
                mname: mname.to_string(),
                pname: pname.to_string(),
            });
            seen.insert((pid.clone(), mid.clone()));
        }
    }
    if let Some(arr) = doc.get("providers").and_then(Value::as_array) {
        for p in arr {
            let Some(pid) = p.get("id").and_then(Value::as_str) else {
                continue;
            };
            let pname = p
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(pid);
            let Some(mm) = p.get("models").and_then(Value::as_object) else {
                continue;
            };
            for (mid, m) in mm {
                if seen.contains(&(pid.to_string(), mid.clone())) {
                    continue;
                }
                if !m.is_object() || !get_bool_value(m, "enabled") {
                    continue;
                }
                let mname = m
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(mid);
                catalog.push(CatalogModel {
                    pid: pid.to_string(),
                    mid: mid.clone(),
                    mname: mname.to_string(),
                    pname: pname.to_string(),
                });
            }
        }
    }
    let term = query.to_lowercase();
    let mut rows: Vec<CatalogModel> = catalog
        .into_iter()
        .filter(|row| {
            term.is_empty()
                || row.mname.to_lowercase().contains(&term)
                || row.mid.to_lowercase().contains(&term)
        })
        .collect();
    rows.sort_by(|a, b| {
        let en = |row: &CatalogModel| {
            if combo_enabled(doc, &row.pid, &row.mid) {
                0u8
            } else {
                1
            }
        };
        let free = |row: &CatalogModel| if is_free(&row.mid) { 0u8 } else { 1 };
        en(a)
            .cmp(&en(b))
            .then_with(|| free(a).cmp(&free(b)))
            .then_with(|| a.mname.to_lowercase().cmp(&b.mname.to_lowercase()))
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| a.mid.cmp(&b.mid))
    });
    rows
}

pub(crate) fn add_model_grid(api: &ModelsDev, doc: &Value, filter: &str, selected: usize) -> Grid {
    let rows = add_model_rows(api, doc, filter);
    let enabled_count = rows
        .iter()
        .filter(|r| combo_enabled(doc, &r.pid, &r.mid))
        .count();
    let free_count = rows[enabled_count.min(rows.len())..]
        .iter()
        .filter(|r| is_free(&r.mid))
        .count();
    let seps = sep_before(enabled_count, free_count, rows.len());
    let (grid_rows, _, _) = stitch(&rows, &seps, |row| {
        let enabled = combo_enabled(doc, &row.pid, &row.mid);
        let name_tone = if enabled {
            Tone::Green
        } else if is_free(&row.mid) {
            Tone::Cyan
        } else {
            Tone::Text
        };
        vec![
            SpanText {
                text: clip(&row.mname, MODEL_NAME_COL_MAX),
                tone: name_tone,
                protect: false,
            },
            SpanText {
                text: format!("({})", clip(&row.pname, PROVIDER_NAME_COL_MAX)),
                tone: Tone::Text,
                protect: false,
            },
            SpanText {
                text: if enabled { "[enabled]" } else { "[disabled]" }.into(),
                tone: if enabled { Tone::Green } else { Tone::Red },
                protect: false,
            },
        ]
    });
    let all = add_model_rows(api, doc, "");
    Grid {
        title: format!("Add Model  {:<6}", format!("({})", rows.len())),
        query: Some(filter.to_string()),
        headers: vec![
            Col {
                label: "Model".into(),
                hot: false,
                right: false,
                width: capped_width(
                    "Model",
                    all.iter()
                        .map(|r| r.mname.chars().count())
                        .max()
                        .unwrap_or(0),
                    MODEL_NAME_COL_MAX,
                ),
                fill: false,
            },
            Col {
                label: "Provider".into(),
                hot: false,
                right: false,
                width: capped_width(
                    "Provider",
                    all.iter()
                        .map(|r| r.pname.chars().count())
                        .max()
                        .unwrap_or(0),
                    PROVIDER_NAME_COL_MAX + 2,
                ),
                fill: false,
            },
            Col {
                label: "Mode".into(),
                hot: false,
                right: false,
                width: MODE_COL_W as u16,
                fill: false,
            },
        ],
        rows: grid_rows,
        selected: visual_selected(selected, &seps, rows.len()),
    }
}

pub(crate) fn counts(doc: &Value) -> (usize, usize) {
    let providers = core::provider_entries(doc).len();
    let enabled = enabled_models(doc, EnabledSort::Model).len();
    (providers, enabled)
}

impl App {
    pub(crate) fn modal_view(&self) -> Option<ModalView> {
        match &self.modal {
            Modal::None => None,
            Modal::Error(message) => Some(ModalView::Error(message.clone())),
            Modal::ConfirmDelete { label, yes, .. } => Some(ModalView::Confirm {
                prompt: format!("Delete Provider {label}?"),
                yes: *yes,
            }),
            Modal::BaseUrl { buffer, .. } => Some(ModalView::Input {
                title: "Base Url".into(),
                hint: "Enter saves. Esc cancels. Empty clears the override.".into(),
                buffer: buffer.clone(),
            }),
            Modal::Pick(picker) => {
                let mut choices: Vec<MenuLine> = picker
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(i, choice)| {
                        let mut spans = color_label(choice);
                        // The popup block already indents one column.
                        if let Some(first) = spans.first_mut() {
                            if let Some(rest) = first.text.strip_prefix(' ') {
                                first.text = rest.to_string();
                            }
                        }
                        MenuLine {
                            spans,
                            selected: i == picker.selected,
                        }
                    })
                    .collect();
                equalize_env_chips(&mut choices);
                Some(ModalView::Pick {
                    title: picker.title.clone(),
                    info: picker.info.clone(),
                    choices,
                })
            }
        }
    }

    pub(crate) fn track_configure_offset(
        &mut self,
        selected: Option<usize>,
        len: usize,
        page: usize,
    ) -> usize {
        if let Some(session) = self.configure.as_mut() {
            session.offset = reveal_offset(session.offset, selected, len, page);
            session.offset
        } else {
            0
        }
    }

    pub(crate) fn track_bench_offset(
        &mut self,
        selected: Option<usize>,
        len: usize,
        page: usize,
    ) -> usize {
        self.bench.offset = reveal_offset(self.bench.offset, selected, len, page);
        self.bench.offset
    }

    pub(crate) fn track_add_offset(
        &mut self,
        providers: bool,
        selected: Option<usize>,
        len: usize,
        page: usize,
    ) -> usize {
        let filter = if providers {
            &mut self.add_provider
        } else {
            &mut self.add_model
        };
        filter.offset = reveal_offset(filter.offset, selected, len, page);
        filter.offset
    }

    pub(crate) fn configure_grid(&self) -> Option<Grid> {
        self.configure.as_ref().map(configure_grid)
    }

    pub(crate) fn add_provider_grid(&self, doc: &Value) -> Option<Grid> {
        self.api.as_ref().map(|api| {
            add_provider_grid(
                api,
                doc,
                &self.add_provider.query,
                self.add_provider.selected,
            )
        })
    }

    pub(crate) fn bench_grid(&self) -> Grid {
        let found = benchmarks::rows(&self.bench.query, self.bench_sort);
        let widths = benchmarks::rows("", self.bench_sort);
        let hot = self.bench_sort.column();
        let name_w = capped_width(
            "Name",
            widths
                .iter()
                .map(|row| row.name.chars().count())
                .max()
                .unwrap_or(0),
            MODEL_NAME_COL_MAX,
        );
        let slug_w = capped_width(
            "Slug",
            widths
                .iter()
                .map(|row| row.slug.chars().count())
                .max()
                .unwrap_or(0),
            MODEL_ID_COL_MAX,
        );
        let score = |n: f32, w: usize| SpanText {
            text: format!("{n:>w$.1}"),
            tone: Tone::Blue,
            protect: false,
        };
        let rows = found
            .iter()
            .map(|row| {
                GridRow::Cells(vec![
                    SpanText {
                        text: clip(row.name, name_w as usize),
                        tone: Tone::Text,
                        protect: false,
                    },
                    SpanText {
                        text: clip(row.slug, slug_w as usize),
                        tone: Tone::Text,
                        protect: false,
                    },
                    score(row.intel, INTEL_COL_W),
                    score(row.coding, CODING_COL_W),
                ])
            })
            .collect();
        Grid {
            title: format!("Artificial Analysis  {:<5}", format!("({})", found.len())),
            query: Some(self.bench.query.clone()),
            headers: vec![
                Col {
                    label: "Name".into(),
                    hot: hot == 0,
                    right: false,
                    width: name_w,
                    fill: false,
                },
                Col {
                    label: "Slug".into(),
                    hot: hot == 1,
                    right: false,
                    width: slug_w,
                    fill: false,
                },
                Col {
                    label: "Intel".into(),
                    hot: hot == 2,
                    right: true,
                    width: INTEL_COL_W as u16,
                    fill: false,
                },
                Col {
                    label: "Coding".into(),
                    hot: hot == 3,
                    right: true,
                    width: CODING_COL_W as u16,
                    fill: false,
                },
            ],
            rows,
            selected: if found.is_empty() {
                None
            } else {
                Some(self.bench.selected.min(found.len() - 1))
            },
        }
    }

    pub(crate) fn add_model_grid(&self, doc: &Value) -> Option<Grid> {
        self.api
            .as_ref()
            .map(|api| add_model_grid(api, doc, &self.add_model.query, self.add_model.selected))
    }

    pub(crate) fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    pub(crate) fn config_is_models(&self) -> bool {
        self.config_view == ConfigView::Models
    }

    /// The provider info page does not scroll. Leave mouse capture off so the
    /// terminal can select the export lines. Every other screen keeps it.
    pub(crate) fn blink_cursor(&mut self) {
        self.cursor_on = !self.cursor_on;
    }

    pub(crate) fn terminal_selects_text(&self) -> bool {
        self.tab == Tab::Config
            && self.config_view != ConfigView::Models
            && matches!(self.modal, Modal::None)
    }

    pub(crate) fn action_index(&self) -> usize {
        self.action
    }

    pub(crate) fn nav_index(&self) -> usize {
        self.nav
    }

    /// Wheel, and click or drag on the scrollbar column.
    pub(crate) fn on_mouse(&mut self, doc: &Value, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_at(doc, mouse.column, mouse.row, true),
            MouseEventKind::ScrollUp => self.scroll_at(doc, mouse.column, mouse.row, false),
            MouseEventKind::Down(MouseButton::Left) => {
                if rect_contains(self.scroll_rect, mouse.column, mouse.row) {
                    self.scroll_drag = true;
                    self.jump_scroll(doc, mouse.row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.scroll_drag => {
                self.jump_scroll(doc, mouse.row);
            }
            MouseEventKind::Up(_) => self.scroll_drag = false,
            _ => {}
        }
    }

    /// Map a scrollbar click onto the viewport. Arrow caps jump to the ends.
    fn jump_scroll(&mut self, doc: &Value, y: u16) {
        let len = self.scroll_len;
        if len == 0 {
            return;
        }
        let offset = track_offset(self.scroll_rect, y, len, self.page_rows);
        let index = self.selection_at_visual(doc, offset);
        match self.tab {
            Tab::AddProvider => {
                self.add_provider.selected = index;
                self.add_provider.offset = offset;
            }
            Tab::AddModel => {
                self.add_model.selected = index;
                self.add_model.offset = offset;
            }
            Tab::Config if self.config_view == ConfigView::Models => {
                if let Some(session) = self.configure.as_mut() {
                    session.selected = index;
                    session.offset = offset;
                }
            }
            Tab::Providers => {
                let models = enabled_models(doc, self.enabled_sort);
                self.focus_model(index, &models);
                self.model_offset = offset;
            }
            Tab::Benchmarks => {
                self.bench.selected = index;
                self.bench.offset = offset;
            }
            Tab::Config => {}
        }
    }

    /// `visual` is a row in the drawn table, including separator lines.
    fn selection_at_visual(&self, doc: &Value, visual: usize) -> usize {
        match self.tab {
            Tab::AddProvider => {
                let Some(api) = self.api.as_ref() else {
                    return visual.min(self.scroll_len.saturating_sub(1));
                };
                let rows = add_provider_rows(api, doc, &self.add_provider.query);
                let added = added_ids(doc);
                let n_added = rows.iter().filter(|r| added.contains(&r.pid)).count();
                let n_sugg = rows
                    .iter()
                    .filter(|r| !added.contains(&r.pid) && SUGGESTED.contains(&r.pid.as_str()))
                    .count();
                data_for_visual(visual, &sep_before(n_added, n_sugg, rows.len()), rows.len())
            }
            Tab::AddModel => {
                let Some(api) = self.api.as_ref() else {
                    return visual.min(self.scroll_len.saturating_sub(1));
                };
                let rows = add_model_rows(api, doc, &self.add_model.query);
                let enabled_count = rows
                    .iter()
                    .filter(|r| combo_enabled(doc, &r.pid, &r.mid))
                    .count();
                let free_count = rows[enabled_count.min(rows.len())..]
                    .iter()
                    .filter(|r| is_free(&r.mid))
                    .count();
                data_for_visual(
                    visual,
                    &sep_before(enabled_count, free_count, rows.len()),
                    rows.len(),
                )
            }
            Tab::Config if self.config_view == ConfigView::Models => {
                let Some(session) = self.configure.as_ref() else {
                    return 0;
                };
                let sorted =
                    core::sort_model_indices(&session.ids, &session.models, Some(&session.query));
                let mids = configure_rows(session);
                data_for_visual(
                    visual,
                    &sep_before(sorted.enabled_count, sorted.free_disabled_count, mids.len()),
                    mids.len(),
                )
            }
            _ => visual.min(self.scroll_len.saturating_sub(1)),
        }
    }

    /// One wheel notch. The list under the pointer moves by a single row.
    /// A notch with no coordinates still scrolls the list that has focus.
    pub(crate) fn scroll_at(&mut self, doc: &Value, x: u16, y: u16, down: bool) {
        if !matches!(self.modal, Modal::None) {
            return;
        }
        let over_menu = rect_contains(self.menu_rect, x, y);
        let over_models = rect_contains(self.model_rect, x, y);
        match self.tab {
            Tab::Providers => {
                if over_menu && !over_models {
                    self.scroll_menu(doc, down);
                } else if over_models || self.focus == Focus::Models {
                    self.scroll_models_row(doc, down);
                } else {
                    self.scroll_menu(doc, down);
                }
            }
            Tab::Config if self.config_view == ConfigView::Models => self.scroll_configure(down),
            Tab::Config => {
                if over_menu || !over_models {
                    self.scroll_action(down);
                }
            }
            Tab::AddProvider => self.scroll_add(doc, true, down),
            Tab::AddModel => self.scroll_add(doc, false, down),
            Tab::Benchmarks => self.scroll_benchmarks(down),
        }
    }

    fn scroll_menu(&mut self, doc: &Value, down: bool) {
        self.focus = Focus::Menu;
        self.model_index = None;
        let nsel = selectable_indices(&home_menu(doc)).len();
        if nsel == 0 {
            return;
        }
        if down {
            if self.nav + 1 < nsel {
                self.nav += 1;
            }
        } else if self.nav > 0 {
            self.nav -= 1;
        }
    }

    fn scroll_models_row(&mut self, doc: &Value, down: bool) {
        let models = enabled_models(doc, self.enabled_sort);
        if models.is_empty() {
            return;
        }
        if self.focus != Focus::Models {
            self.focus = Focus::Models;
            self.model_index = Some(self.model_offset.min(models.len() - 1));
        }
        let i = self.model_index.unwrap_or(0);
        let next = if down {
            (i + 1).min(models.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.focus_model(next, &models);
    }

    fn scroll_action(&mut self, down: bool) {
        if down {
            if self.action + 1 < 5 {
                self.action += 1;
            }
        } else if self.action > 0 {
            self.action -= 1;
        }
    }

    fn scroll_configure(&mut self, down: bool) {
        let Some(session) = self.configure.as_ref() else {
            return;
        };
        let len = configure_rows(session).len();
        let selected = session.selected;
        let Some(session) = self.configure.as_mut() else {
            return;
        };
        if len == 0 {
            return;
        }
        session.selected = if down {
            (selected + 1).min(len - 1)
        } else {
            selected.saturating_sub(1)
        };
    }

    fn scroll_benchmarks(&mut self, down: bool) {
        let len = benchmarks::rows(&self.bench.query, self.bench_sort).len();
        if len == 0 {
            return;
        }
        self.bench.selected = if down {
            (self.bench.selected + 1).min(len - 1)
        } else {
            self.bench.selected.saturating_sub(1)
        };
    }

    fn scroll_add(&mut self, doc: &Value, providers: bool, down: bool) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let len = if providers {
            add_provider_rows(&api, doc, &self.add_provider.query).len()
        } else {
            add_model_rows(&api, doc, &self.add_model.query).len()
        };
        if len == 0 {
            return;
        }
        let selected = if providers {
            &mut self.add_provider.selected
        } else {
            &mut self.add_model.selected
        };
        if down {
            if *selected + 1 < len {
                *selected += 1;
            }
        } else if *selected > 0 {
            *selected -= 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn testing_set_api(&mut self, api: ModelsDev) {
        self.api = Some(api);
    }

    #[cfg(test)]
    pub(crate) fn testing_tab_add_provider(&mut self) {
        self.tab = Tab::AddProvider;
    }
}

pub(crate) enum ModalView {
    Error(String),
    Confirm {
        prompt: String,
        yes: bool,
    },
    Input {
        title: String,
        hint: String,
        buffer: String,
    },
    Pick {
        title: String,
        info: Option<String>,
        choices: Vec<MenuLine>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn doc() -> Value {
        json!({
            "providers": [
                {
                    "id": "opencode",
                    "name": "OpenCode",
                    "enabled": true,
                    "env_key": "OPENCODE_API_KEY",
                    "base_url": "https://example.test/v1",
                    "doc": "https://example.test/docs",
                    "models": {
                        "glm-5": {
                            "name": "GLM-5",
                            "enabled": true,
                            "api_backend": "responses",
                            "reasoning_effort": "high",
                            "reasoning_efforts": [
                                {"value": "low", "label": "Low", "default": false},
                                {"value": "high", "label": "High", "default": true}
                            ]
                        },
                        "free-mini": {"name": "Free Mini", "enabled": false},
                        "zeta": {"name": "Zeta", "enabled": true, "reasoning_effort": "none"}
                    }
                },
                {
                    "id": "quiet",
                    "name": "Quiet",
                    "enabled": false,
                    "models": {"x": {"name": "X", "enabled": true}}
                }
            ],
            "include_descriptions": false,
            "last_updated": "01-02-2026 03:04 PM",
            "last_synced": "01-02-2026 03:05 PM"
        })
    }

    #[test]
    fn home_menu_lists_providers_and_every_action() {
        let items = home_menu(&doc());
        let labels: Vec<String> = items
            .iter()
            .map(|item| match item {
                HomeItem::Provider { label, .. } | HomeItem::Action { label, .. } => label.clone(),
            })
            .collect();
        let text = labels.join("\n");
        assert!(text.contains("opencode"), "{text}");
        assert!(text.contains("Codex Config"), "{text}");
        assert!(text.contains("Model Descriptions"), "{text}");
        assert!(text.contains("[disabled]"), "{text}");
        assert!(text.contains("Web Search"), "{text}");
        assert!(text.contains("Update Model List"), "{text}");
        assert!(text.contains("01-02-2026 03:04 PM"), "{text}");
        assert!(text.contains("Sync Model Config"), "{text}");
        assert!(!text.contains("Add Provider"), "{text}");
        assert!(!text.contains("Add Model"), "{text}");
        let mut rows = vec![
            MenuLine {
                spans: color_label("A - a          [enabled]  A_KEY          = \"\""),
                selected: false,
            },
            MenuLine {
                spans: color_label("Beta - long-id [enabled]  LONGER_API_KEY = \"abcdefghij...\""),
                selected: false,
            },
        ];
        equalize_env_chips(&mut rows);
        let chips: Vec<String> = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .filter(|span| span.protect)
                    .map(|span| span.text.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(chips[0].chars().count(), chips[1].chars().count());
        assert_eq!(chips[0].find('='), chips[1].find('='));
        let lines = menu_section_lines(&items, Some(0));
        for needle in ["01-02-2026 03:04 PM", "01-02-2026 03:05 PM"] {
            let tone = lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.text.contains(needle))
                .map(|span| span.tone);
            assert_eq!(tone, Some(Tone::Green), "{needle}");
        }
        assert!(labels.iter().all(|l| l != "rule"));
    }

    #[test]
    fn enabled_models_skip_disabled_providers_and_sort_intel() {
        let rows = enabled_models(&doc(), EnabledSort::Model);
        let ids: Vec<&str> = rows.iter().map(|r| r.mid.as_str()).collect();
        assert!(ids.contains(&"glm-5"), "{ids:?}");
        assert!(ids.contains(&"zeta"));
        assert!(!ids.contains(&"free-mini"));
        assert!(!ids.contains(&"x"), "disabled provider must be hidden");
        let by_intel = enabled_models(&doc(), EnabledSort::Intel);
        assert_eq!(by_intel[0].mid, "glm-5");
        assert_eq!(reasoning_level(&doc(), "opencode", "glm-5"), "high");
    }

    #[test]
    fn add_provider_buckets_added_then_suggested() {
        let mut providers = HashMap::new();
        for id in ["zz-last", "opencode", "kilo", "plain"] {
            providers.insert(
                id.to_string(),
                crate::fetch::ModelsDevProvider {
                    name: Some(id.to_string()),
                    models: HashMap::new(),
                    ..Default::default()
                },
            );
        }
        let api = ModelsDev { providers };
        let rows = add_provider_rows(&api, &doc(), "");
        let ids: Vec<&str> = rows.iter().map(|r| r.pid.as_str()).collect();
        let added_at = ids.iter().position(|id| *id == "opencode").unwrap();
        let kilo_at = ids.iter().position(|id| *id == "kilo").unwrap();
        let plain_at = ids.iter().position(|id| *id == "plain").unwrap();
        assert!(added_at < kilo_at && kilo_at < plain_at, "{ids:?}");
        let grid = add_provider_grid(&api, &doc(), "", 0);
        let filtered = add_provider_grid(&api, &doc(), "plain", 0);
        assert_eq!(grid.headers.len(), 3);
        assert_eq!(filtered.headers[0].width, grid.headers[0].width);
        assert_eq!(filtered.headers[1].width, grid.headers[1].width);
        assert!(grid.headers[1].width <= PROVIDER_ID_COL_MAX as u16);
        assert!(grid.headers[0].width <= (PROVIDER_NAME_COL_MAX + 2) as u16);
        assert_eq!(grid.headers[2].label, "Mode");
        assert_eq!(grid.headers[2].width, 10);
        let text: String = grid
            .rows
            .iter()
            .filter_map(|row| match row {
                GridRow::Cells(cells) => Some(
                    cells
                        .iter()
                        .map(|c| c.text.as_str())
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
                GridRow::Sep(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[enabled]"), "{text}");
        assert!(text.contains("[disabled]"), "{text}");
        let filtered = add_provider_rows(&api, &doc(), "kilo");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pid, "kilo");
    }

    #[test]
    fn descriptions_toggle_persists() {
        let _homes = crate::env::test_support::TestHomes::setup();
        let mut doc = doc();
        let mut app = App::new();
        app.toggle_descriptions(&mut doc).unwrap();
        assert_eq!(doc["include_descriptions"], true);
        assert!(app.changed);
        assert_eq!(app.status.as_deref(), Some("Model Descriptions enabled"));
    }

    #[test]
    fn reasoning_pick_writes_default() {
        let _homes = crate::env::test_support::TestHomes::setup();
        let mut doc = doc();
        apply_reasoning(&mut doc, "opencode", "glm-5", "low").unwrap();
        assert_eq!(
            doc["providers"][0]["models"]["glm-5"]["reasoning_effort"],
            "low"
        );
        let efforts = doc["providers"][0]["models"]["glm-5"]["reasoning_efforts"]
            .as_array()
            .unwrap();
        assert_eq!(efforts[0]["default"], true);
        assert_eq!(efforts[1]["default"], false);
    }

    #[test]
    fn configure_groups_enabled_free_then_rest() {
        let value = doc();
        let provider = &value["providers"][0];
        let models = provider["models"].as_object().unwrap().clone();
        let ids: Vec<String> = models.keys().cloned().collect();
        let session = ConfigureSession {
            provider_id: "opencode".into(),
            pname: "OpenCode".into(),
            ids,
            models,
            query: String::new(),
            selected: 0,
            offset: 0,
            sort: ModelSort::Name,
            changed: false,
        };
        let rows = configure_rows(&session);
        assert_eq!(rows[0], "glm-5");
        assert!(rows.iter().any(|id| id == "free-mini"));
        assert!(rows.iter().any(|id| id == "zeta"));
        let glm_at = rows.iter().position(|id| id == "glm-5").unwrap();
        let zeta_at = rows.iter().position(|id| id == "zeta").unwrap();
        let free_at = rows.iter().position(|id| id == "free-mini").unwrap();
        assert!(glm_at < free_at && zeta_at < free_at, "{rows:?}");
    }

    #[test]
    fn codex_pick_writes_providers_json() {
        let homes = crate::env::test_support::TestHomes::setup();
        let mut doc = doc();
        jsonio::dump_providers(&crate::env::paths::providers_path(), &mut doc).unwrap();
        let mut app = App::new();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        // provider row, then the rule, then Codex Config
        app.on_key(&mut doc, down).unwrap();
        app.on_key(&mut doc, down).unwrap();
        app.on_key(&mut doc, enter).unwrap();
        app.on_key(&mut doc, down).unwrap();
        app.on_key(&mut doc, enter).unwrap();
        let path = homes.grok_home.join("providers.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"write_codex_config_toml\": true"), "{text}");
        assert!(
            text.contains("\"codex_model_provider\": \"opencode\""),
            "{text}"
        );
    }

    #[test]
    fn reveal_offset_moves_highlight_before_the_page() {
        let page = 5;
        let mut offset = 0;
        for selected in 0..8 {
            offset = reveal_offset(offset, Some(selected), 20, page);
        }
        assert_eq!(
            offset, 3,
            "scrolling down keeps the row on the last visible line"
        );
        offset = reveal_offset(offset, Some(7), 20, page);
        assert_eq!(offset, 3, "one step up stays on the same page");
        offset = reveal_offset(offset, Some(3), 20, page);
        assert_eq!(
            offset, 3,
            "highlight walks to the top before the page moves"
        );
        offset = reveal_offset(offset, Some(2), 20, page);
        assert_eq!(offset, 2);
    }

    #[test]
    fn configure_toggle_commits_on_back() {
        let _homes = crate::env::test_support::TestHomes::setup();
        let mut doc = doc();
        let mut app = App::new();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.on_key(&mut doc, enter).unwrap();
        assert_eq!(app.tab, Tab::Config);
        app.on_key(&mut doc, enter).unwrap();
        assert!(app.config_is_models());
        app.on_key(&mut doc, enter).unwrap();
        app.on_key(&mut doc, esc).unwrap();
        let provider = doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "opencode")
            .unwrap();
        assert_eq!(provider["models"]["glm-5"]["enabled"], false);
        assert!(app.changed);
    }

    #[test]
    fn wheel_over_each_pane_steps_that_list() {
        let doc = doc();
        let mut app = App::new();
        app.menu_rect = Rect::new(0, 4, 80, 12);
        app.model_rect = Rect::new(0, 16, 80, 14);
        app.scroll_at(&doc, 4, 6, true);
        assert_eq!(app.focus, Focus::Menu);
        assert_eq!(app.nav_index(), 1);
        app.scroll_at(&doc, 4, 18, true);
        app.scroll_at(&doc, 4, 18, true);
        assert_eq!(app.focus, Focus::Models);
        assert_eq!(app.model_index, Some(1));
        app.scroll_at(&doc, 4, 6, false);
        assert_eq!(app.focus, Focus::Menu);
    }

    #[test]
    fn scrollbar_drag_moves_add_provider_selection() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let doc = doc();
        let mut app = App::new();
        app.testing_tab_add_provider();
        app.scroll_rect = Rect::new(79, 4, 1, 22);
        app.scroll_len = 41;
        let at = |kind, row| MouseEvent {
            kind,
            column: 79,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        app.on_mouse(&doc, at(MouseEventKind::Down(MouseButton::Left), 4));
        assert_eq!(app.add_provider.selected, 0);
        assert_eq!(app.add_provider.offset, 0);
        app.on_mouse(&doc, at(MouseEventKind::Drag(MouseButton::Left), 25));
        assert_eq!(app.add_provider.offset, 41 - app.page_rows.max(1));
        assert_eq!(app.add_provider.selected, app.add_provider.offset);
        app.on_mouse(&doc, at(MouseEventKind::Up(MouseButton::Left), 25));
        let stayed = app.add_provider.selected;
        app.on_mouse(&doc, at(MouseEventKind::Drag(MouseButton::Left), 4));
        assert_eq!(
            app.add_provider.selected, stayed,
            "a drag that did not start on the bar must not move the list"
        );
    }
}

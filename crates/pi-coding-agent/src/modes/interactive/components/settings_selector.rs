//! Interactive settings selector.
//!
//! Port of `modes/interactive/components/settings-selector.ts`.
//!
//! Deviations from the oracle (see slice report):
//! - `HTTP_IDLE_TIMEOUT_CHOICES` / `format_http_idle_timeout_ms` mirror
//!   `core/http-dispatcher.ts` locally because the HTTP dispatcher module is
//!   not ported; move them there when it lands.

use std::cell::RefCell;
use std::rc::Rc;

use pi_ai::types::{ModelThinkingLevel, Transport};
use pi_tui::components::{
    SelectItem, SelectList, SelectListLayoutOptions, SettingItem, SettingsList,
    SettingsListOptions, Text,
};
use pi_tui::terminal_image::{ImageProtocol, get_capabilities};
use pi_tui::{Component, ComponentBox, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_display_text;
use crate::modes::interactive::theme::{
    TerminalTheme, ThemeColor, get_select_list_theme, get_settings_list_theme,
    parse_auto_theme_setting, theme,
};
use crate::settings_manager::WarningSettings;

fn settings_submenu_select_list_layout() -> SelectListLayoutOptions {
    SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        ..Default::default()
    }
}

fn thinking_level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

fn thinking_level_from_str(value: &str) -> Option<ModelThinkingLevel> {
    Some(match value {
        "off" => ModelThinkingLevel::Off,
        "minimal" => ModelThinkingLevel::Minimal,
        "low" => ModelThinkingLevel::Low,
        "medium" => ModelThinkingLevel::Medium,
        "high" => ModelThinkingLevel::High,
        "xhigh" => ModelThinkingLevel::Xhigh,
        "max" => ModelThinkingLevel::Max,
        _ => return None,
    })
}

fn thinking_description(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "No reasoning",
        ModelThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ModelThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ModelThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ModelThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ModelThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ModelThinkingLevel::Max => "Maximum reasoning",
    }
}

/// Oracle `DEFAULT_PROJECT_TRUST_LABELS` (value, label) in declaration order.
const DEFAULT_PROJECT_TRUST_LABELS: [(&str, &str); 3] = [
    ("ask", "Ask"),
    ("always", "Always trust"),
    ("never", "Never trust"),
];

fn default_project_trust_label(value: &str) -> &'static str {
    DEFAULT_PROJECT_TRUST_LABELS
        .iter()
        .find(|(v, _)| *v == value)
        .map_or("Ask", |(_, label)| label)
}

fn default_project_trust_by_label(label: &str) -> Option<&'static str> {
    DEFAULT_PROJECT_TRUST_LABELS
        .iter()
        .find(|(_, l)| *l == label)
        .map(|(value, _)| *value)
}

fn transport_as_str(transport: Transport) -> &'static str {
    match transport {
        Transport::Sse => "sse",
        Transport::Websocket => "websocket",
        Transport::WebsocketCached => "websocket-cached",
        Transport::Auto => "auto",
    }
}

fn transport_from_str(value: &str) -> Option<Transport> {
    Some(match value {
        "sse" => Transport::Sse,
        "websocket" => Transport::Websocket,
        "websocket-cached" => Transport::WebsocketCached,
        "auto" => Transport::Auto,
        _ => return None,
    })
}

/// Mirror of `core/http-dispatcher.ts` `HTTP_IDLE_TIMEOUT_CHOICES`.
const HTTP_IDLE_TIMEOUT_CHOICES: [(&str, u64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

/// Mirror of `core/http-dispatcher.ts` `formatHttpIdleTimeoutMs`.
#[must_use]
pub fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    if let Some((label, _)) = HTTP_IDLE_TIMEOUT_CHOICES
        .iter()
        .find(|(_, ms)| *ms == timeout_ms)
    {
        return (*label).to_owned();
    }
    // JS `${timeoutMs / 1000} sec` — number formatting without trailing zeros.
    let secs = timeout_ms as f64 / 1000.0;
    if secs.fract() == 0.0 {
        format!("{} sec", secs as u64)
    } else {
        format!("{secs} sec")
    }
}

/// Oracle `SettingsConfig`.
pub struct SettingsConfig {
    pub auto_compact: bool,
    pub show_images: bool,
    pub image_width_cells: u32,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub enable_skill_commands: bool,
    /// `"all"` | `"one-at-a-time"`.
    pub steering_mode: String,
    /// `"all"` | `"one-at-a-time"`.
    pub follow_up_mode: String,
    pub transport: Transport,
    pub http_idle_timeout_ms: u64,
    pub thinking_level: ModelThinkingLevel,
    pub available_thinking_levels: Vec<ModelThinkingLevel>,
    pub current_theme: String,
    pub terminal_theme: TerminalTheme,
    pub available_themes: Vec<String>,
    pub hide_thinking_block: bool,
    pub show_cache_miss_notices: bool,
    pub collapse_changelog: bool,
    pub enable_install_telemetry: bool,
    /// `"fork"` | `"tree"` | `"none"`.
    pub double_escape_action: String,
    /// `"default"` | `"no-tools"` | `"user-only"` | `"labeled-only"` | `"all"`.
    pub tree_filter_mode: String,
    pub show_hardware_cursor: bool,
    pub editor_padding_x: u32,
    /// `0` | `1`.
    pub output_pad: u8,
    pub autocomplete_max_visible: u32,
    pub quiet_startup: bool,
    /// `"ask"` | `"always"` | `"never"`.
    pub default_project_trust: String,
    pub clear_on_shrink: bool,
    pub show_terminal_progress: bool,
    pub warnings: WarningSettings,
}

/// Boxed `&str` callback (clippy `type_complexity`).
type StrCallback = Box<dyn FnMut(&str)>;
/// Theme preview callback shared across submenus.
type PreviewFn = Box<dyn Fn(&str)>;
/// `(id, new_value)` change handler passed to `SettingsList`.
type SettingsChangeFn = Box<dyn FnMut(&str, &str)>;
/// `(current_value, done)` submenu factory (pi-tui `SettingItem::submenu`).
type SubmenuFactory = Box<dyn FnMut(&str, Box<dyn FnMut(Option<String>)>) -> ComponentBox>;

/// Oracle `SettingsCallbacks`.
///
/// Fields are `Fn` (not `FnMut`) because the theme submenu shares the bundle
/// across several closures via `Rc`.
pub struct SettingsCallbacks {
    pub on_auto_compact_change: Box<dyn Fn(bool)>,
    pub on_show_images_change: Box<dyn Fn(bool)>,
    pub on_image_width_cells_change: Box<dyn Fn(u32)>,
    pub on_auto_resize_images_change: Box<dyn Fn(bool)>,
    pub on_block_images_change: Box<dyn Fn(bool)>,
    pub on_enable_skill_commands_change: Box<dyn Fn(bool)>,
    pub on_steering_mode_change: Box<dyn Fn(&str)>,
    pub on_follow_up_mode_change: Box<dyn Fn(&str)>,
    pub on_transport_change: Box<dyn Fn(Transport)>,
    pub on_http_idle_timeout_ms_change: Box<dyn Fn(u64)>,
    pub on_thinking_level_change: Box<dyn Fn(ModelThinkingLevel)>,
    pub on_theme_change: Box<dyn Fn(&str)>,
    pub on_theme_preview: Option<PreviewFn>,
    pub on_hide_thinking_block_change: Box<dyn Fn(bool)>,
    pub on_show_cache_miss_notices_change: Box<dyn Fn(bool)>,
    pub on_collapse_changelog_change: Box<dyn Fn(bool)>,
    pub on_enable_install_telemetry_change: Box<dyn Fn(bool)>,
    pub on_double_escape_action_change: Box<dyn Fn(&str)>,
    pub on_tree_filter_mode_change: Box<dyn Fn(&str)>,
    pub on_show_hardware_cursor_change: Box<dyn Fn(bool)>,
    pub on_editor_padding_x_change: Box<dyn Fn(u32)>,
    pub on_output_pad_change: Box<dyn Fn(u8)>,
    pub on_autocomplete_max_visible_change: Box<dyn Fn(u32)>,
    pub on_quiet_startup_change: Box<dyn Fn(bool)>,
    pub on_default_project_trust_change: Box<dyn Fn(&str)>,
    pub on_clear_on_shrink_change: Box<dyn Fn(bool)>,
    pub on_show_terminal_progress_change: Box<dyn Fn(bool)>,
    pub on_warnings_change: Box<dyn Fn(WarningSettings)>,
    pub on_cancel: Box<dyn Fn()>,
}

impl SettingsCallbacks {
    fn preview(&self, value: &str) {
        if let Some(preview) = &self.on_theme_preview {
            preview(value);
        }
    }
}

/// Oracle `WarningSettingsSubmenu` (settings-selector.ts:120-160): a
/// `SettingsList` over the individual warning toggles.
struct WarningSettingsSubmenu {
    settings_list: SettingsList,
}

impl WarningSettingsSubmenu {
    fn new(
        warnings: WarningSettings,
        on_change: Box<dyn Fn(WarningSettings)>,
        on_cancel: Box<dyn FnMut()>,
    ) -> Self {
        let state = Rc::new(RefCell::new(warnings));
        let items = vec![SettingItem {
            id: "anthropic-extra-usage".to_owned(),
            label: "Anthropic extra usage".to_owned(),
            description: Some(
                "Warn when Anthropic subscription auth may use paid extra usage".to_owned(),
            ),
            current_value: if state.borrow().anthropic_extra_usage.unwrap_or(true) {
                "true".to_owned()
            } else {
                "false".to_owned()
            },
            values: Some(vec!["true".to_owned(), "false".to_owned()]),
            submenu: None,
        }];
        let max_visible = items.len().min(10);
        let change_state = Rc::clone(&state);
        let settings_list = SettingsList::new(
            items,
            max_visible,
            get_settings_list_theme(),
            Box::new(move |id, new_value| {
                if id == "anthropic-extra-usage" {
                    change_state.borrow_mut().anthropic_extra_usage = Some(new_value == "true");
                    on_change(change_state.borrow().clone());
                }
            }),
            on_cancel,
            SettingsListOptions {
                enable_search: true,
            },
        );
        Self { settings_list }
    }
}

impl Component for WarningSettingsSubmenu {
    fn render(&mut self, width: u16) -> &[Line] {
        self.settings_list.render(width)
    }

    fn invalidate(&mut self) {
        self.settings_list.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        self.settings_list.handle_input(data);
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

/// A submenu component for selecting from a list of options (oracle
/// `SelectSubmenu`).
struct SelectSubmenu {
    title: Text,
    description: Option<Text>,
    select_list: SelectList,
    hint: Text,
    cached: Vec<Line>,
}

impl SelectSubmenu {
    fn new(
        title: &str,
        description: &str,
        options: Vec<SelectItem>,
        current_value: &str,
        mut on_select: StrCallback,
        on_cancel: Box<dyn FnMut()>,
        mut on_selection_change: Option<StrCallback>,
    ) -> Self {
        let title_text = Text::new(
            theme().bold(&theme().fg(ThemeColor::Accent, title)),
            0,
            0,
            None,
        );
        let description_text = if description.is_empty() {
            None
        } else {
            Some(Text::new(
                theme().fg(ThemeColor::Muted, description),
                0,
                0,
                None,
            ))
        };

        let max_visible = options.len().min(10);
        // Pre-select current value (index computed before the list takes
        // ownership of the items).
        let current_index = options.iter().position(|o| o.value == current_value);
        let mut select_list = SelectList::new(
            options,
            max_visible,
            get_select_list_theme(),
            settings_submenu_select_list_layout(),
        );
        if let Some(current_index) = current_index {
            select_list.set_selected_index(current_index);
        }

        select_list.on_select = Some(Box::new(move |item| on_select(&item.value)));
        select_list.on_cancel = Some(on_cancel);
        if on_selection_change.is_some() {
            select_list.on_selection_change = Some(Box::new(move |item| {
                if let Some(cb) = &mut on_selection_change {
                    cb(&item.value);
                }
            }));
        }

        Self {
            title: title_text,
            description: description_text,
            select_list,
            hint: Text::new(
                theme().fg(ThemeColor::Dim, "  Enter to select · Esc to go back"),
                0,
                0,
                None,
            ),
            cached: Vec::new(),
        }
    }
}

impl Component for SelectSubmenu {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.extend_from_slice(self.title.render(width));
        if let Some(desc) = &mut self.description {
            self.cached.push(Line::empty());
            self.cached.extend_from_slice(desc.render(width));
        }
        self.cached.push(Line::empty());
        self.cached
            .extend_from_slice(self.select_list.render(width));
        self.cached.push(Line::empty());
        self.cached.extend_from_slice(self.hint.render(width));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.title.invalidate();
        if let Some(desc) = &mut self.description {
            desc.invalidate();
        }
        self.select_list.invalidate();
        self.hint.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        self.select_list.handle_input(data);
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

fn theme_items(available_themes: &[String]) -> Vec<SelectItem> {
    available_themes
        .iter()
        .map(|name| SelectItem::new(name, name))
        .collect()
}

const AUTOMATIC_THEME_VALUE: &str = "/";

fn single_mode_theme_items(available_themes: &[String]) -> Vec<SelectItem> {
    let mut items = vec![
        SelectItem::new(AUTOMATIC_THEME_VALUE, "Automatic")
            .with_description("Use separate themes for light and dark terminal appearance"),
    ];
    items.extend(theme_items(available_themes));
    items
}

fn preferred_theme(available_themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    if let Some(preferred) = preferred
        && available_themes.iter().any(|t| t == preferred)
    {
        return preferred.to_owned();
    }
    if available_themes.iter().any(|t| t == fallback) {
        return fallback.to_owned();
    }
    available_themes
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

fn default_automatic_themes(
    current_theme_setting: &str,
    available_themes: &[String],
) -> (String, String) {
    if let Some(auto_theme) = parse_auto_theme_setting(Some(current_theme_setting)) {
        return auto_theme;
    }
    let current_fixed_theme = if current_theme_setting.contains('/') {
        None
    } else {
        Some(current_theme_setting)
    };
    let theme_name = preferred_theme(available_themes, current_fixed_theme, "dark");
    (theme_name.clone(), theme_name)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Single,
    Automatic,
}

/// Mutable theme-submenu state shared with menu closures.
struct ThemeState {
    mode: ThemeMode,
    single_theme: String,
    light_theme: String,
    dark_theme: String,
    /// Menu rebuild requested by a closure (drained in `handle_input`).
    rebuild: Option<ThemeMode>,
    /// `done` result requested by a closure: `Some(selected)` / cancel.
    finished: Option<Option<String>>,
}

impl ThemeState {
    fn automatic_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }

    fn active_automatic_theme(&self, terminal_theme: TerminalTheme) -> String {
        if terminal_theme == TerminalTheme::Light {
            self.light_theme.clone()
        } else {
            self.dark_theme.clone()
        }
    }

    fn theme_setting(&self, terminal_theme: TerminalTheme) -> String {
        let _ = terminal_theme;
        if self.mode == ThemeMode::Automatic {
            self.automatic_setting()
        } else {
            self.single_theme.clone()
        }
    }
}

enum ThemeContent {
    Single(SelectSubmenu),
    Automatic {
        header: Vec<Text>,
        list: SettingsList,
    },
}

/// Oracle `ThemeSubmenu` — single/automatic theme picker with previews.
struct ThemeSubmenu {
    state: Rc<RefCell<ThemeState>>,
    callbacks: Rc<SettingsCallbacks>,
    available_themes: Rc<Vec<String>>,
    terminal_theme: TerminalTheme,
    original_theme_setting: String,
    done: Box<dyn FnMut(Option<String>)>,
    content: ThemeContent,
    cached: Vec<Line>,
}

impl ThemeSubmenu {
    fn new(
        current_theme_setting: &str,
        terminal_theme: TerminalTheme,
        available_themes: Rc<Vec<String>>,
        callbacks: Rc<SettingsCallbacks>,
        done: Box<dyn FnMut(Option<String>)>,
    ) -> Self {
        let auto_theme = parse_auto_theme_setting(Some(current_theme_setting));
        let (light_theme, dark_theme) =
            default_automatic_themes(current_theme_setting, &available_themes);
        // JS: `autoTheme || currentThemeSetting.includes("/") ? undefined : currentThemeSetting`
        let fixed_theme = if auto_theme.is_some() || current_theme_setting.contains('/') {
            None
        } else {
            Some(current_theme_setting.to_owned())
        };
        let mode = if auto_theme.is_some() {
            ThemeMode::Automatic
        } else {
            ThemeMode::Single
        };
        let state = ThemeState {
            mode,
            single_theme: String::new(),
            light_theme,
            dark_theme,
            rebuild: None,
            finished: None,
        };
        let active_automatic = if auto_theme.is_some() {
            Some(state.active_automatic_theme(terminal_theme))
        } else {
            None
        };
        let single_theme = preferred_theme(
            &available_themes,
            fixed_theme.as_deref().or(active_automatic.as_deref()),
            "dark",
        );
        let state = Rc::new(RefCell::new(ThemeState {
            single_theme,
            ..state
        }));

        let mut submenu = Self {
            state,
            callbacks,
            available_themes,
            terminal_theme,
            original_theme_setting: current_theme_setting.to_owned(),
            done,
            content: ThemeContent::Single(placeholder_select_submenu()),
            cached: Vec::new(),
        };
        submenu.content = match mode {
            ThemeMode::Automatic => submenu.build_automatic_menu(),
            ThemeMode::Single => ThemeContent::Single(submenu.build_single_menu()),
        };
        submenu
    }

    fn build_single_menu(&self) -> SelectSubmenu {
        self.state.borrow_mut().mode = ThemeMode::Single;
        let state = Rc::clone(&self.state);
        let callbacks = Rc::clone(&self.callbacks);
        let terminal_theme = self.terminal_theme;
        let on_select: Box<dyn FnMut(&str)> = Box::new(move |value| {
            if value == AUTOMATIC_THEME_VALUE {
                let mut st = state.borrow_mut();
                st.mode = ThemeMode::Automatic;
                let setting = st.theme_setting(terminal_theme);
                st.rebuild = Some(ThemeMode::Automatic);
                drop(st);
                callbacks.preview(&setting);
                return;
            }
            let mut st = state.borrow_mut();
            st.single_theme = value.to_owned();
            st.finished = Some(Some(value.to_owned()));
        });

        let state = Rc::clone(&self.state);
        let callbacks = Rc::clone(&self.callbacks);
        let original = self.original_theme_setting.clone();
        let on_cancel: Box<dyn FnMut()> = Box::new(move || {
            callbacks.preview(&original);
            state.borrow_mut().finished = Some(None);
        });

        let state = Rc::clone(&self.state);
        let callbacks = Rc::clone(&self.callbacks);
        let on_selection_change: Box<dyn FnMut(&str)> = Box::new(move |value| {
            let preview_value = if value == AUTOMATIC_THEME_VALUE {
                state.borrow().automatic_setting()
            } else {
                value.to_owned()
            };
            callbacks.preview(&preview_value);
        });

        SelectSubmenu::new(
            "Theme",
            "Select a theme, or choose Automatic to follow terminal appearance.",
            single_mode_theme_items(&self.available_themes),
            &self.state.borrow().single_theme.clone(),
            on_select,
            on_cancel,
            Some(on_selection_change),
        )
    }

    fn build_automatic_menu(&self) -> ThemeContent {
        self.state.borrow_mut().mode = ThemeMode::Automatic;
        let header = vec![
            Text::new(
                theme().bold(&theme().fg(ThemeColor::Accent, "Automatic Theme")),
                0,
                0,
                None,
            ),
            Text::new(
                theme().fg(
                    ThemeColor::Muted,
                    "Choose themes for terminal light and dark appearance.",
                ),
                0,
                0,
                None,
            ),
            Text::new(
                theme().fg(
                    ThemeColor::Muted,
                    "Light/dark detection requires terminal support.",
                ),
                0,
                0,
                None,
            ),
        ];

        let light_current = self.state.borrow().light_theme.clone();
        let dark_current = self.state.borrow().dark_theme.clone();

        let light_submenu = self.theme_select_factory(
            "Light Theme",
            "Select the theme to use for light terminal appearance",
            true,
        );
        let dark_submenu = self.theme_select_factory(
            "Dark Theme",
            "Select the theme to use for dark terminal appearance",
            false,
        );

        let items = vec![
            SettingItem {
                id: "light-theme".to_owned(),
                label: "Light theme".to_owned(),
                description: Some(
                    "Theme to use in automatic mode when the terminal is light".to_owned(),
                ),
                current_value: light_current,
                values: None,
                submenu: Some(light_submenu),
            },
            SettingItem {
                id: "dark-theme".to_owned(),
                label: "Dark theme".to_owned(),
                description: Some(
                    "Theme to use in automatic mode when the terminal is dark".to_owned(),
                ),
                current_value: dark_current,
                values: None,
                submenu: Some(dark_submenu),
            },
            SettingItem {
                id: "apply".to_owned(),
                label: "Apply".to_owned(),
                description: Some("Save and go back".to_owned()),
                current_value: "save and go back".to_owned(),
                values: Some(vec!["save and go back".to_owned()]),
                submenu: None,
            },
            SettingItem {
                id: "single-mode".to_owned(),
                label: "Change mode".to_owned(),
                description: Some("Switch to one theme for light and dark".to_owned()),
                current_value: "switch to single theme".to_owned(),
                values: Some(vec!["switch to single theme".to_owned()]),
                submenu: None,
            },
        ];

        let state = Rc::clone(&self.state);
        let callbacks = Rc::clone(&self.callbacks);
        let terminal_theme = self.terminal_theme;
        let max_visible = items.len().min(10);
        let on_change: SettingsChangeFn = Box::new(move |id, _new_value| match id {
            "single-mode" => {
                let mut st = state.borrow_mut();
                st.mode = ThemeMode::Single;
                st.single_theme = st.active_automatic_theme(terminal_theme);
                let preview = st.single_theme.clone();
                st.rebuild = Some(ThemeMode::Single);
                drop(st);
                callbacks.preview(&preview);
            }
            "apply" => {
                let mut st = state.borrow_mut();
                let setting = st.automatic_setting();
                st.finished = Some(Some(setting));
            }
            _ => {}
        });

        let state = Rc::clone(&self.state);
        let callbacks = Rc::clone(&self.callbacks);
        let original = self.original_theme_setting.clone();
        let on_cancel: Box<dyn FnMut()> = Box::new(move || {
            callbacks.preview(&original);
            state.borrow_mut().finished = Some(None);
        });

        let list = SettingsList::new(
            items,
            max_visible,
            get_settings_list_theme(),
            on_change,
            on_cancel,
            SettingsListOptions::default(),
        );
        ThemeContent::Automatic { header, list }
    }

    /// Oracle `createThemeSelect` as a `SettingItem` submenu factory.
    fn theme_select_factory(
        &self,
        title: &'static str,
        description: &'static str,
        is_light: bool,
    ) -> SubmenuFactory {
        let state = Rc::clone(&self.state);
        let callbacks = Rc::clone(&self.callbacks);
        let available_themes = Rc::clone(&self.available_themes);
        Box::new(move |current_value, done| {
            let done = Rc::new(RefCell::new(done));

            let st = Rc::clone(&state);
            let cbs = Rc::clone(&callbacks);
            let done_select = Rc::clone(&done);
            let on_select: Box<dyn FnMut(&str)> = Box::new(move |value| {
                {
                    let mut s = st.borrow_mut();
                    if is_light {
                        s.light_theme = value.to_owned();
                    } else {
                        s.dark_theme = value.to_owned();
                    }
                }
                cbs.preview(&st.borrow().automatic_setting());
                (done_select.borrow_mut())(Some(value.to_owned()));
            });

            let st = Rc::clone(&state);
            let cbs = Rc::clone(&callbacks);
            let done_cancel = Rc::clone(&done);
            let on_cancel: Box<dyn FnMut()> = Box::new(move || {
                cbs.preview(&st.borrow().automatic_setting());
                (done_cancel.borrow_mut())(None);
            });

            let cbs = Rc::clone(&callbacks);
            let on_selection_change: Box<dyn FnMut(&str)> =
                Box::new(move |value| cbs.preview(value));

            Box::new(SelectSubmenu::new(
                title,
                description,
                theme_items(&available_themes),
                current_value,
                on_select,
                on_cancel,
                Some(on_selection_change),
            ))
        })
    }

    /// Drain closure-requested state transitions (mode switch / done).
    fn drain_state(&mut self) {
        let finished = self.state.borrow_mut().finished.take();
        if let Some(result) = finished {
            (self.done)(result);
            return;
        }
        let rebuild = self.state.borrow_mut().rebuild.take();
        if let Some(mode) = rebuild {
            self.content = match mode {
                ThemeMode::Automatic => self.build_automatic_menu(),
                ThemeMode::Single => ThemeContent::Single(self.build_single_menu()),
            };
        }
    }
}

/// Inert placeholder used only during two-phase construction.
fn placeholder_select_submenu() -> SelectSubmenu {
    SelectSubmenu::new(
        "",
        "",
        Vec::new(),
        "",
        Box::new(|_| {}),
        Box::new(|| {}),
        None,
    )
}

impl Component for ThemeSubmenu {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        match &mut self.content {
            ThemeContent::Single(menu) => {
                self.cached.extend_from_slice(menu.render(width));
            }
            ThemeContent::Automatic { header, list } => {
                let mut first = true;
                for text in header.iter_mut() {
                    self.cached.extend_from_slice(text.render(width));
                    if first {
                        // Spacer(1) after the title.
                        self.cached.push(Line::empty());
                        first = false;
                    }
                }
                // Spacer(1) before the list.
                self.cached.push(Line::empty());
                self.cached.extend_from_slice(list.render(width));
            }
        }
        &self.cached
    }

    fn invalidate(&mut self) {
        match &mut self.content {
            ThemeContent::Single(menu) => menu.invalidate(),
            ThemeContent::Automatic { header, list } => {
                for text in header.iter_mut() {
                    text.invalidate();
                }
                list.invalidate();
            }
        }
    }

    fn handle_input(&mut self, data: &str) {
        match &mut self.content {
            ThemeContent::Single(menu) => menu.handle_input(data),
            ThemeContent::Automatic { list, .. } => list.handle_input(data),
        }
        self.drain_state();
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

/// Main settings selector component (oracle `SettingsSelectorComponent`).
pub struct SettingsSelectorComponent {
    top_border: DynamicBorder,
    settings_list: SettingsList,
    bottom_border: DynamicBorder,
    cached: Vec<Line>,
}

impl SettingsSelectorComponent {
    #[must_use]
    pub fn new(config: SettingsConfig, callbacks: SettingsCallbacks) -> Self {
        let callbacks = Rc::new(callbacks);
        let supports_images = get_capabilities().images != ImageProtocol::None;
        let follow_up_key = key_display_text("app.message.followUp");
        let available_themes = Rc::new(config.available_themes.clone());

        let bool_values = || Some(vec!["true".to_owned(), "false".to_owned()]);
        let bool_value = |b: bool| if b { "true" } else { "false" }.to_owned();

        let mut items: Vec<SettingItem> = vec![
            SettingItem {
                id: "autocompact".to_owned(),
                label: "Auto-compact".to_owned(),
                description: Some(
                    "Automatically compact context when it gets too large".to_owned(),
                ),
                current_value: bool_value(config.auto_compact),
                values: bool_values(),
                submenu: None,
            },
            SettingItem {
                id: "steering-mode".to_owned(),
                label: "Steering mode".to_owned(),
                description: Some(
                    "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
                        .to_owned(),
                ),
                current_value: config.steering_mode.clone(),
                values: Some(vec!["one-at-a-time".to_owned(), "all".to_owned()]),
                submenu: None,
            },
            SettingItem {
                id: "follow-up-mode".to_owned(),
                label: "Follow-up mode".to_owned(),
                description: Some(format!(
                    "{follow_up_key} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
                )),
                current_value: config.follow_up_mode.clone(),
                values: Some(vec!["one-at-a-time".to_owned(), "all".to_owned()]),
                submenu: None,
            },
            SettingItem {
                id: "transport".to_owned(),
                label: "Transport".to_owned(),
                description: Some(
                    "Preferred transport for providers that support multiple transports"
                        .to_owned(),
                ),
                current_value: transport_as_str(config.transport).to_owned(),
                values: Some(vec![
                    "sse".to_owned(),
                    "websocket".to_owned(),
                    "websocket-cached".to_owned(),
                    "auto".to_owned(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "http-idle-timeout".to_owned(),
                label: "HTTP idle timeout".to_owned(),
                description: Some(
                    "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes."
                        .to_owned(),
                ),
                current_value: format_http_idle_timeout_ms(config.http_idle_timeout_ms),
                values: Some(
                    HTTP_IDLE_TIMEOUT_CHOICES
                        .iter()
                        .map(|(label, _)| (*label).to_owned())
                        .collect(),
                ),
                submenu: None,
            },
            SettingItem {
                id: "hide-thinking".to_owned(),
                label: "Hide thinking".to_owned(),
                description: Some("Hide thinking blocks in assistant responses".to_owned()),
                current_value: bool_value(config.hide_thinking_block),
                values: bool_values(),
                submenu: None,
            },
            SettingItem {
                id: "cache-miss-notices".to_owned(),
                label: "Cache miss notices".to_owned(),
                description: Some(
                    "Show transcript notices for significant prompt-cache misses".to_owned(),
                ),
                current_value: bool_value(config.show_cache_miss_notices),
                values: bool_values(),
                submenu: None,
            },
            SettingItem {
                id: "collapse-changelog".to_owned(),
                label: "Collapse changelog".to_owned(),
                description: Some("Show condensed changelog after updates".to_owned()),
                current_value: bool_value(config.collapse_changelog),
                values: bool_values(),
                submenu: None,
            },
            SettingItem {
                id: "quiet-startup".to_owned(),
                label: "Quiet startup".to_owned(),
                description: Some("Disable verbose printing at startup".to_owned()),
                current_value: bool_value(config.quiet_startup),
                values: bool_values(),
                submenu: None,
            },
            SettingItem {
                id: "install-telemetry".to_owned(),
                label: "Install telemetry".to_owned(),
                description: Some(
                    "Send an anonymous version/update ping after changelog-detected updates"
                        .to_owned(),
                ),
                current_value: bool_value(config.enable_install_telemetry),
                values: bool_values(),
                submenu: None,
            },
            SettingItem {
                id: "default-project-trust".to_owned(),
                label: "Default project trust".to_owned(),
                description: Some(
                    "Fallback behavior when no extension or saved trust decision decides project trust"
                        .to_owned(),
                ),
                current_value: default_project_trust_label(&config.default_project_trust)
                    .to_owned(),
                values: Some(
                    DEFAULT_PROJECT_TRUST_LABELS
                        .iter()
                        .map(|(_, label)| (*label).to_owned())
                        .collect(),
                ),
                submenu: None,
            },
            SettingItem {
                id: "double-escape-action".to_owned(),
                label: "Double-escape action".to_owned(),
                description: Some(
                    "Action when pressing Escape twice with empty editor".to_owned(),
                ),
                current_value: config.double_escape_action.clone(),
                values: Some(vec![
                    "tree".to_owned(),
                    "fork".to_owned(),
                    "none".to_owned(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "tree-filter-mode".to_owned(),
                label: "Tree filter mode".to_owned(),
                description: Some("Default filter when opening /tree".to_owned()),
                current_value: config.tree_filter_mode.clone(),
                values: Some(vec![
                    "default".to_owned(),
                    "no-tools".to_owned(),
                    "user-only".to_owned(),
                    "labeled-only".to_owned(),
                    "all".to_owned(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "warnings".to_owned(),
                label: "Warnings".to_owned(),
                description: Some("Enable or disable individual warnings".to_owned()),
                current_value: "configure".to_owned(),
                values: None,
                submenu: Some({
                    let callbacks = Rc::clone(&callbacks);
                    let current_warnings = Rc::new(RefCell::new(config.warnings.clone()));
                    Box::new(move |_current_value, mut done| {
                        let cbs = Rc::clone(&callbacks);
                        let shared = Rc::clone(&current_warnings);
                        let change_shared = Rc::clone(&current_warnings);
                        Box::new(WarningSettingsSubmenu::new(
                            shared.borrow().clone(),
                            Box::new(move |warnings| {
                                *change_shared.borrow_mut() = warnings.clone();
                                (cbs.on_warnings_change)(warnings);
                            }),
                            Box::new(move || done(None)),
                        ))
                    })
                }),
            },
            SettingItem {
                id: "thinking".to_owned(),
                label: "Thinking level".to_owned(),
                description: Some("Reasoning depth for thinking-capable models".to_owned()),
                current_value: thinking_level_name(config.thinking_level).to_owned(),
                values: None,
                submenu: Some({
                    let callbacks = Rc::clone(&callbacks);
                    let available_levels = config.available_thinking_levels.clone();
                    Box::new(move |current_value, done| {
                        let done = Rc::new(RefCell::new(done));
                        let cbs = Rc::clone(&callbacks);
                        let done_select = Rc::clone(&done);
                        let on_select: Box<dyn FnMut(&str)> = Box::new(move |value| {
                            if let Some(level) = thinking_level_from_str(value) {
                                (cbs.on_thinking_level_change)(level);
                            }
                            (done_select.borrow_mut())(Some(value.to_owned()));
                        });
                        let done_cancel = Rc::clone(&done);
                        let on_cancel: Box<dyn FnMut()> =
                            Box::new(move || (done_cancel.borrow_mut())(None));
                        Box::new(SelectSubmenu::new(
                            "Thinking Level",
                            "Select reasoning depth for thinking-capable models",
                            available_levels
                                .iter()
                                .map(|&level| {
                                    SelectItem::new(
                                        thinking_level_name(level),
                                        thinking_level_name(level),
                                    )
                                    .with_description(thinking_description(level))
                                })
                                .collect(),
                            current_value,
                            on_select,
                            on_cancel,
                            None,
                        ))
                    })
                }),
            },
            SettingItem {
                id: "theme".to_owned(),
                label: "Theme".to_owned(),
                description: Some("Color theme for the interface".to_owned()),
                current_value: config.current_theme.clone(),
                values: None,
                submenu: Some({
                    let callbacks = Rc::clone(&callbacks);
                    let available_themes = Rc::clone(&available_themes);
                    let terminal_theme = config.terminal_theme;
                    Box::new(move |current_value, done| {
                        Box::new(ThemeSubmenu::new(
                            current_value,
                            terminal_theme,
                            Rc::clone(&available_themes),
                            Rc::clone(&callbacks),
                            done,
                        ))
                    })
                }),
            },
        ];

        // Only show image toggle if terminal supports it.
        if supports_images {
            // Insert after autocompact.
            items.insert(
                1,
                SettingItem {
                    id: "show-images".to_owned(),
                    label: "Show images".to_owned(),
                    description: Some("Render images inline in terminal".to_owned()),
                    current_value: bool_value(config.show_images),
                    values: bool_values(),
                    submenu: None,
                },
            );
            items.insert(
                2,
                SettingItem {
                    id: "image-width-cells".to_owned(),
                    label: "Image width".to_owned(),
                    description: Some("Preferred inline image width in terminal cells".to_owned()),
                    current_value: config.image_width_cells.to_string(),
                    values: Some(vec!["60".to_owned(), "80".to_owned(), "120".to_owned()]),
                    submenu: None,
                },
            );
        }

        // Image auto-resize toggle (always available, affects both attached and read images).
        items.insert(
            if supports_images { 3 } else { 1 },
            SettingItem {
                id: "auto-resize-images".to_owned(),
                label: "Auto-resize images".to_owned(),
                description: Some(
                    "Resize large images to 2000x2000 max for better model compatibility"
                        .to_owned(),
                ),
                current_value: bool_value(config.auto_resize_images),
                values: bool_values(),
                submenu: None,
            },
        );

        let insert_after = |items: &mut Vec<SettingItem>, anchor: &str, item: SettingItem| {
            let idx = items
                .iter()
                .position(|i| i.id == anchor)
                .map_or(items.len(), |i| i + 1);
            items.insert(idx, item);
        };

        // Block images toggle (always available, insert after auto-resize-images).
        insert_after(
            &mut items,
            "auto-resize-images",
            SettingItem {
                id: "block-images".to_owned(),
                label: "Block images".to_owned(),
                description: Some("Prevent images from being sent to LLM providers".to_owned()),
                current_value: bool_value(config.block_images),
                values: bool_values(),
                submenu: None,
            },
        );

        // Skill commands toggle (insert after block-images).
        insert_after(
            &mut items,
            "block-images",
            SettingItem {
                id: "skill-commands".to_owned(),
                label: "Skill commands".to_owned(),
                description: Some("Register skills as /skill:name commands".to_owned()),
                current_value: bool_value(config.enable_skill_commands),
                values: bool_values(),
                submenu: None,
            },
        );

        // Hardware cursor toggle (insert after skill-commands).
        insert_after(
            &mut items,
            "skill-commands",
            SettingItem {
                id: "show-hardware-cursor".to_owned(),
                label: "Show hardware cursor".to_owned(),
                description: Some(
                    "Show the terminal cursor while still positioning it for IME support"
                        .to_owned(),
                ),
                current_value: bool_value(config.show_hardware_cursor),
                values: bool_values(),
                submenu: None,
            },
        );

        // Editor padding toggle (insert after show-hardware-cursor).
        insert_after(
            &mut items,
            "show-hardware-cursor",
            SettingItem {
                id: "editor-padding".to_owned(),
                label: "Editor padding".to_owned(),
                description: Some("Horizontal padding for input editor (0-3)".to_owned()),
                current_value: config.editor_padding_x.to_string(),
                values: Some(vec![
                    "0".to_owned(),
                    "1".to_owned(),
                    "2".to_owned(),
                    "3".to_owned(),
                ]),
                submenu: None,
            },
        );

        // Output padding toggle (insert after editor-padding).
        insert_after(
            &mut items,
            "editor-padding",
            SettingItem {
                id: "output-padding".to_owned(),
                label: "Output padding".to_owned(),
                description: Some(
                    "Horizontal padding for user messages, assistant messages, and thinking"
                        .to_owned(),
                ),
                current_value: config.output_pad.to_string(),
                values: Some(vec!["0".to_owned(), "1".to_owned()]),
                submenu: None,
            },
        );

        // Autocomplete max visible toggle (insert after output-padding).
        insert_after(
            &mut items,
            "output-padding",
            SettingItem {
                id: "autocomplete-max-visible".to_owned(),
                label: "Autocomplete max items".to_owned(),
                description: Some("Max visible items in autocomplete dropdown (3-20)".to_owned()),
                current_value: config.autocomplete_max_visible.to_string(),
                values: Some(vec![
                    "3".to_owned(),
                    "5".to_owned(),
                    "7".to_owned(),
                    "10".to_owned(),
                    "15".to_owned(),
                    "20".to_owned(),
                ]),
                submenu: None,
            },
        );

        // Clear on shrink toggle (insert after autocomplete-max-visible).
        insert_after(
            &mut items,
            "autocomplete-max-visible",
            SettingItem {
                id: "clear-on-shrink".to_owned(),
                label: "Clear on shrink".to_owned(),
                description: Some(
                    "Clear empty rows when content shrinks (may cause flicker)".to_owned(),
                ),
                current_value: bool_value(config.clear_on_shrink),
                values: bool_values(),
                submenu: None,
            },
        );

        // Terminal progress toggle (insert after clear-on-shrink).
        insert_after(
            &mut items,
            "clear-on-shrink",
            SettingItem {
                id: "terminal-progress".to_owned(),
                label: "Terminal progress".to_owned(),
                description: Some(
                    "Show OSC 9;4 progress indicators in the terminal tab bar".to_owned(),
                ),
                current_value: bool_value(config.show_terminal_progress),
                values: bool_values(),
                submenu: None,
            },
        );

        let cbs = Rc::clone(&callbacks);
        let on_change: SettingsChangeFn = Box::new(move |id, new_value| match id {
            "autocompact" => (cbs.on_auto_compact_change)(new_value == "true"),
            "show-images" => (cbs.on_show_images_change)(new_value == "true"),
            "image-width-cells" => {
                if let Ok(width) = new_value.parse::<u32>() {
                    (cbs.on_image_width_cells_change)(width);
                }
            }
            "auto-resize-images" => (cbs.on_auto_resize_images_change)(new_value == "true"),
            "block-images" => (cbs.on_block_images_change)(new_value == "true"),
            "skill-commands" => (cbs.on_enable_skill_commands_change)(new_value == "true"),
            "steering-mode" => (cbs.on_steering_mode_change)(new_value),
            "follow-up-mode" => (cbs.on_follow_up_mode_change)(new_value),
            "transport" => {
                if let Some(transport) = transport_from_str(new_value) {
                    (cbs.on_transport_change)(transport);
                }
            }
            "http-idle-timeout" => {
                if let Some((_, timeout_ms)) = HTTP_IDLE_TIMEOUT_CHOICES
                    .iter()
                    .find(|(label, _)| *label == new_value)
                {
                    (cbs.on_http_idle_timeout_ms_change)(*timeout_ms);
                }
            }
            "hide-thinking" => (cbs.on_hide_thinking_block_change)(new_value == "true"),
            "cache-miss-notices" => (cbs.on_show_cache_miss_notices_change)(new_value == "true"),
            "collapse-changelog" => (cbs.on_collapse_changelog_change)(new_value == "true"),
            "quiet-startup" => (cbs.on_quiet_startup_change)(new_value == "true"),
            "install-telemetry" => {
                (cbs.on_enable_install_telemetry_change)(new_value == "true");
            }
            "default-project-trust" => {
                if let Some(trust) = default_project_trust_by_label(new_value) {
                    (cbs.on_default_project_trust_change)(trust);
                }
            }
            "double-escape-action" => (cbs.on_double_escape_action_change)(new_value),
            "tree-filter-mode" => (cbs.on_tree_filter_mode_change)(new_value),
            "show-hardware-cursor" => (cbs.on_show_hardware_cursor_change)(new_value == "true"),
            "editor-padding" => {
                if let Ok(padding) = new_value.parse::<u32>() {
                    (cbs.on_editor_padding_x_change)(padding);
                }
            }
            "output-padding" => {
                (cbs.on_output_pad_change)(if new_value == "0" { 0 } else { 1 });
            }
            "autocomplete-max-visible" => {
                if let Ok(max_visible) = new_value.parse::<u32>() {
                    (cbs.on_autocomplete_max_visible_change)(max_visible);
                }
            }
            "clear-on-shrink" => (cbs.on_clear_on_shrink_change)(new_value == "true"),
            "terminal-progress" => {
                (cbs.on_show_terminal_progress_change)(new_value == "true");
            }
            "theme" => (cbs.on_theme_change)(new_value),
            _ => {}
        });

        let cbs = Rc::clone(&callbacks);
        let on_cancel: Box<dyn FnMut()> = Box::new(move || (cbs.on_cancel)());

        let settings_list = SettingsList::new(
            items,
            10,
            get_settings_list_theme(),
            on_change,
            on_cancel,
            SettingsListOptions {
                enable_search: true,
            },
        );

        Self {
            top_border: DynamicBorder::default(),
            settings_list,
            bottom_border: DynamicBorder::default(),
            cached: Vec::new(),
        }
    }

    pub fn get_settings_list(&mut self) -> &mut SettingsList {
        &mut self.settings_list
    }
}

impl Component for SettingsSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.extend_from_slice(self.top_border.render(width));
        self.cached
            .extend_from_slice(self.settings_list.render(width));
        self.cached
            .extend_from_slice(self.bottom_border.render(width));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.settings_list.invalidate();
        self.bottom_border.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        self.settings_list.handle_input(data);
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_callbacks() -> SettingsCallbacks {
        SettingsCallbacks {
            on_auto_compact_change: Box::new(|_| {}),
            on_show_images_change: Box::new(|_| {}),
            on_image_width_cells_change: Box::new(|_| {}),
            on_auto_resize_images_change: Box::new(|_| {}),
            on_block_images_change: Box::new(|_| {}),
            on_enable_skill_commands_change: Box::new(|_| {}),
            on_steering_mode_change: Box::new(|_| {}),
            on_follow_up_mode_change: Box::new(|_| {}),
            on_transport_change: Box::new(|_| {}),
            on_http_idle_timeout_ms_change: Box::new(|_| {}),
            on_thinking_level_change: Box::new(|_| {}),
            on_theme_change: Box::new(|_| {}),
            on_theme_preview: None,
            on_hide_thinking_block_change: Box::new(|_| {}),
            on_show_cache_miss_notices_change: Box::new(|_| {}),
            on_collapse_changelog_change: Box::new(|_| {}),
            on_enable_install_telemetry_change: Box::new(|_| {}),
            on_double_escape_action_change: Box::new(|_| {}),
            on_tree_filter_mode_change: Box::new(|_| {}),
            on_show_hardware_cursor_change: Box::new(|_| {}),
            on_editor_padding_x_change: Box::new(|_| {}),
            on_output_pad_change: Box::new(|_| {}),
            on_autocomplete_max_visible_change: Box::new(|_| {}),
            on_quiet_startup_change: Box::new(|_| {}),
            on_default_project_trust_change: Box::new(|_| {}),
            on_clear_on_shrink_change: Box::new(|_| {}),
            on_show_terminal_progress_change: Box::new(|_| {}),
            on_warnings_change: Box::new(|_| {}),
            on_cancel: Box::new(|| {}),
        }
    }

    fn test_config() -> SettingsConfig {
        SettingsConfig {
            auto_compact: true,
            show_images: false,
            image_width_cells: 80,
            auto_resize_images: true,
            block_images: false,
            enable_skill_commands: true,
            steering_mode: "one-at-a-time".to_owned(),
            follow_up_mode: "all".to_owned(),
            transport: Transport::Auto,
            http_idle_timeout_ms: 300_000,
            thinking_level: ModelThinkingLevel::Medium,
            available_thinking_levels: vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ],
            current_theme: "dark".to_owned(),
            terminal_theme: TerminalTheme::Dark,
            available_themes: vec!["dark".to_owned(), "light".to_owned()],
            hide_thinking_block: false,
            show_cache_miss_notices: false,
            collapse_changelog: true,
            enable_install_telemetry: false,
            double_escape_action: "tree".to_owned(),
            tree_filter_mode: "default".to_owned(),
            show_hardware_cursor: true,
            editor_padding_x: 1,
            output_pad: 1,
            autocomplete_max_visible: 7,
            quiet_startup: false,
            default_project_trust: "ask".to_owned(),
            clear_on_shrink: false,
            show_terminal_progress: true,
            warnings: WarningSettings::default(),
        }
    }

    #[test]
    fn http_idle_timeout_labels_match_oracle() {
        assert_eq!(format_http_idle_timeout_ms(30_000), "30 sec");
        assert_eq!(format_http_idle_timeout_ms(60_000), "1 min");
        assert_eq!(format_http_idle_timeout_ms(120_000), "2 min");
        assert_eq!(format_http_idle_timeout_ms(300_000), "5 min");
        assert_eq!(format_http_idle_timeout_ms(0), "disabled");
        // JS `${timeoutMs / 1000} sec` fallback.
        assert_eq!(format_http_idle_timeout_ms(45_000), "45 sec");
        assert_eq!(format_http_idle_timeout_ms(1_500), "1.5 sec");
    }

    #[test]
    fn default_project_trust_label_round_trip() {
        assert_eq!(default_project_trust_label("ask"), "Ask");
        assert_eq!(default_project_trust_label("always"), "Always trust");
        assert_eq!(default_project_trust_label("never"), "Never trust");
        assert_eq!(
            default_project_trust_by_label("Always trust"),
            Some("always")
        );
        assert_eq!(default_project_trust_by_label("Never trust"), Some("never"));
        assert_eq!(default_project_trust_by_label("Ask"), Some("ask"));
        assert_eq!(default_project_trust_by_label("bogus"), None);
    }

    #[test]
    fn transport_round_trip() {
        for transport in [
            Transport::Sse,
            Transport::Websocket,
            Transport::WebsocketCached,
            Transport::Auto,
        ] {
            assert_eq!(
                transport_from_str(transport_as_str(transport)),
                Some(transport)
            );
        }
    }

    #[test]
    fn automatic_theme_helpers() {
        let themes = vec!["dark".to_owned(), "light".to_owned()];
        assert_eq!(preferred_theme(&themes, Some("light"), "dark"), "light");
        assert_eq!(preferred_theme(&themes, Some("missing"), "dark"), "dark");
        assert_eq!(
            default_automatic_themes("light/dark", &themes),
            ("light".to_owned(), "dark".to_owned())
        );
        assert_eq!(
            default_automatic_themes("light", &themes),
            ("light".to_owned(), "light".to_owned())
        );
    }

    #[test]
    fn renders_settings_rows_and_cycles_values() {
        let changed = Rc::new(RefCell::new(Vec::<(String, bool)>::new()));
        let mut callbacks = noop_callbacks();
        let sink = Rc::clone(&changed);
        callbacks.on_auto_compact_change =
            Box::new(move |v| sink.borrow_mut().push(("autocompact".to_owned(), v)));
        let mut component = SettingsSelectorComponent::new(test_config(), callbacks);

        let text: Vec<String> = component
            .render(100)
            .iter()
            .map(pi_tui::Line::plain_text)
            .collect();
        assert!(
            text.iter().any(|l| l.contains("Auto-compact")),
            "settings list should show the Auto-compact row: {text:?}"
        );

        // First row is autocompact (true) — Enter cycles true -> false.
        component.handle_input("\r");
        assert_eq!(
            changed.borrow().as_slice(),
            &[("autocompact".to_owned(), false)]
        );
    }
}

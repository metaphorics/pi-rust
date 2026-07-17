//! First-time theme and analytics setup ported from `first-time-setup.ts`.

use pi_tui::component::{Component, RenderStatus};
use pi_tui::components::Text;
use pi_tui::line::Line;

use super::keybinding_hints::{key_hint, raw_key_hint};
use crate::config::APP_NAME;
use crate::modes::interactive::theme::{
    TerminalTheme, ThemeColor, current_theme_name, set_theme, theme,
};

const THEME_OPTIONS: [(TerminalTheme, &str); 2] = [
    (TerminalTheme::Dark, "Dark"),
    (TerminalTheme::Light, "Light"),
];
const ANALYTICS_OPTIONS: [(bool, &str); 2] =
    [(true, "Share anonymous usage data"), (false, "Don't share")];
const SETUP_LOGO_LINES: [&str; 4] = ["██████", "██  ██", "████  ██", "██    ██"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirstTimeSetupResult {
    pub theme: TerminalTheme,
    pub share_analytics: bool,
}

pub struct FirstTimeSetupOptions {
    pub detected_theme: TerminalTheme,
    pub on_theme_preview: Box<dyn FnMut(TerminalTheme)>,
    pub on_submit: Box<dyn FnMut(FirstTimeSetupResult)>,
    pub on_cancel: Box<dyn FnMut()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupStep {
    Theme,
    Analytics,
}

pub struct FirstTimeSetup {
    step: SetupStep,
    detected_theme: TerminalTheme,
    theme_index: usize,
    analytics_index: usize,
    original_theme: Option<String>,
    on_theme_preview: Box<dyn FnMut(TerminalTheme)>,
    on_submit: Box<dyn FnMut(FirstTimeSetupResult)>,
    on_cancel: Box<dyn FnMut()>,
    cached: Vec<Line>,
}

impl FirstTimeSetup {
    #[must_use]
    pub fn new(options: FirstTimeSetupOptions) -> Self {
        let theme_index = THEME_OPTIONS
            .iter()
            .position(|(value, _)| *value == options.detected_theme)
            .unwrap_or(0);
        Self {
            step: SetupStep::Theme,
            detected_theme: options.detected_theme,
            theme_index,
            analytics_index: 0,
            original_theme: current_theme_name(),
            on_theme_preview: options.on_theme_preview,
            on_submit: options.on_submit,
            on_cancel: options.on_cancel,
            cached: Vec::new(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.step {
            SetupStep::Theme => {
                let next = self.theme_index.saturating_add_signed(delta).min(1);
                if next != self.theme_index {
                    self.theme_index = next;
                    let selected = THEME_OPTIONS[self.theme_index].0;
                    let _ = set_theme(selected.as_str(), false);
                    (self.on_theme_preview)(selected);
                }
            }
            SetupStep::Analytics => {
                self.analytics_index = self.analytics_index.saturating_add_signed(delta).min(1);
            }
        }
    }

    fn cancel(&mut self) {
        if let Some(original) = self.original_theme.as_deref() {
            let _ = set_theme(original, false);
        }
        (self.on_cancel)();
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    if text.is_empty() {
        lines.push(Line::plain(" ".repeat(usize::from(width))));
    } else {
        let mut component = Text::new(text, 1, 0, None);
        lines.extend_from_slice(component.render(width));
    }
}

impl Component for FirstTimeSetup {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Accent, &SETUP_LOGO_LINES.join("\n")),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &theme().fg(
                ThemeColor::Accent,
                &theme().bold(&format!("Welcome to {APP_NAME}, the minimal coding agent.")),
            ),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));

        let (prompt, detail, labels, selected) = match self.step {
            SetupStep::Theme => (
                "Pick a theme.",
                format!(
                    "Detected system appearance: {}",
                    self.detected_theme.as_str()
                ),
                THEME_OPTIONS.map(|(_, label)| label),
                self.theme_index,
            ),
            SetupStep::Analytics => (
                "Opt-in to anonymous usage data sharing?",
                "Opting in stores a tracking identifier in settings.json and enables anonymous\nusage analytics. This helps us to better debug, reproduce, and resolve issues\nand bugs within Pi. You can observe what is shared using /privacy and make\nchanges anytime in settings.json."
                    .to_owned(),
                ANALYTICS_OPTIONS.map(|(_, label)| label),
                self.analytics_index,
            ),
        };
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Text, prompt),
            width,
        );
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Muted, &detail),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        for (index, label) in labels.iter().enumerate() {
            let selected_here = index == selected;
            let prefix = if selected_here {
                theme().fg(ThemeColor::Accent, "→ ")
            } else {
                "  ".to_owned()
            };
            let label = if selected_here {
                theme().fg(ThemeColor::Accent, label)
            } else {
                theme().fg(ThemeColor::Text, label)
            };
            append_text(&mut self.cached, &format!("{prefix}{label}"), width);
        }
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &format!(
                "{}  {}  {}",
                raw_key_hint("↑↓", "navigate"),
                key_hint(
                    "tui.select.confirm",
                    if self.step == SetupStep::Theme {
                        "continue"
                    } else {
                        "finish"
                    }
                ),
                key_hint("tui.select.cancel", "skip setup")
            ),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, data: &str) {
        let keybindings = pi_tui::keybindings::get_keybindings();
        if keybindings.matches(data, "tui.select.up") || data == "k" {
            self.move_selection(-1);
        } else if keybindings.matches(data, "tui.select.down") || data == "j" {
            self.move_selection(1);
        } else if keybindings.matches(data, "tui.select.confirm") || data == "\n" {
            if self.step == SetupStep::Theme {
                self.step = SetupStep::Analytics;
            } else {
                (self.on_submit)(FirstTimeSetupResult {
                    theme: THEME_OPTIONS[self.theme_index].0,
                    share_analytics: ANALYTICS_OPTIONS[self.analytics_index].0,
                });
            }
        } else if keybindings.matches(data, "tui.select.cancel") {
            self.cancel();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

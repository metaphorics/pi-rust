//! Generic extension selector with keyboard navigation.

use std::time::{Duration, Instant};

use pi_tui::component::{Component, RenderStatus};
use pi_tui::components::Text;
use pi_tui::line::Line;

use super::keybinding_hints::{key_hint, raw_key_hint};
use crate::modes::interactive::theme::{ThemeColor, theme};

pub struct ExtensionSelector {
    pub title: String,
    pub options: Vec<String>,
    pub selected: usize,
    deadline: Option<Instant>,
    expired: bool,
    pub on_submit: Option<Box<dyn FnMut(String)>>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_toggle_tools_expanded: Option<Box<dyn FnMut()>>,
    cached: Vec<Line>,
}

impl ExtensionSelector {
    #[must_use]
    pub fn new(title: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            title: title.into(),
            options,
            selected: 0,
            deadline: None,
            expired: false,
            on_submit: None,
            on_cancel: None,
            on_toggle_tools_expanded: None,
            cached: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        if !timeout.is_zero() {
            self.deadline = Some(Instant::now() + timeout);
        }
        self
    }

    fn tick_countdown(&mut self) -> Option<u64> {
        let deadline = self.deadline?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let seconds = remaining.as_millis().div_ceil(1_000) as u64;
        if remaining.is_zero() && !self.expired {
            self.expired = true;
            self.deadline = None;
            if let Some(on_cancel) = &mut self.on_cancel {
                on_cancel();
            }
        }
        Some(seconds)
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    let mut component = Text::new(text, 1, 0, None);
    lines.extend_from_slice(component.render(width));
}

impl Component for ExtensionSelector {
    fn render(&mut self, width: u16) -> &[Line] {
        let countdown = self.tick_countdown();
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        let title = countdown.map_or_else(
            || self.title.clone(),
            |seconds| format!("{} ({seconds}s)", self.title),
        );
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Accent, &theme().bold(&title)),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        for (index, option) in self.options.iter().enumerate() {
            let selected = index == self.selected;
            let text = if selected {
                format!(
                    "{}{}",
                    theme().fg(ThemeColor::Accent, "→ "),
                    theme().fg(ThemeColor::Accent, option)
                )
            } else {
                format!("  {}", theme().fg(ThemeColor::Text, option))
            };
            append_text(&mut self.cached, &text, width);
        }
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &format!(
                "{}  {}  {}",
                raw_key_hint("↑↓", "navigate"),
                key_hint("tui.select.confirm", "select"),
                key_hint("tui.select.cancel", "cancel")
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
        if keybindings.matches(data, "app.tools.expand") {
            if let Some(toggle) = &mut self.on_toggle_tools_expanded {
                toggle();
            }
        } else if keybindings.matches(data, "tui.select.up") || data == "k" {
            self.selected = self.selected.saturating_sub(1);
        } else if keybindings.matches(data, "tui.select.down") || data == "j" {
            self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
        } else if keybindings.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(option) = self.options.get(self.selected).cloned()
                && let Some(on_submit) = &mut self.on_submit
            {
                on_submit(option);
            }
        } else if keybindings.matches(data, "tui.select.cancel")
            && let Some(on_cancel) = &mut self.on_cancel
        {
            on_cancel();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

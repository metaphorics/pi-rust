//! Simple text-input dialog for extensions.

use std::time::{Duration, Instant};

use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::components::{Input, Text};
use pi_tui::line::Line;

use super::keybinding_hints::key_hint;
use crate::modes::interactive::theme::{ThemeColor, theme};

pub struct ExtensionInput {
    pub title: String,
    input: Input,
    focused: bool,
    deadline: Option<Instant>,
    expired: bool,
    pub on_submit: Option<Box<dyn FnMut(String)>>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    cached: Vec<Line>,
}

impl ExtensionInput {
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            input: Input::default(),
            focused: false,
            deadline: None,
            expired: false,
            on_submit: None,
            on_cancel: None,
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

    #[must_use]
    pub fn value(&self) -> &str {
        self.input.get_value()
    }

    pub fn set_value(&mut self, value: &str) {
        self.input.set_value(value);
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

impl Component for ExtensionInput {
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
            &theme().fg(ThemeColor::Accent, &title),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.extend_from_slice(self.input.render(width));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &format!(
                "{}  {}",
                key_hint("tui.select.confirm", "submit"),
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

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let (confirm, cancel) = {
            let keybindings = pi_tui::keybindings::get_keybindings();
            (
                keybindings.matches(data, "tui.select.confirm"),
                keybindings.matches(data, "tui.select.cancel"),
            )
        };
        if confirm || data == "\n" {
            if let Some(on_submit) = &mut self.on_submit {
                on_submit(self.input.get_value().to_owned());
            }
        } else if cancel {
            if let Some(on_cancel) = &mut self.on_cancel {
                on_cancel();
            }
        } else {
            self.input.handle_input(data);
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ExtensionInput {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

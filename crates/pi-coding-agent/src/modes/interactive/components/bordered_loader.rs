//! Loader wrapped with borders for extension UI.

use pi_tui::component::{Component, RenderStatus};
use pi_tui::components::Loader;
use pi_tui::line::Line;

use super::keybinding_hints::key_hint;
use crate::modes::interactive::theme::{ThemeColor, theme};

pub struct BorderedLoader {
    loader: Loader,
    cancellable: bool,
    cancelled: bool,
    on_abort: Option<Box<dyn FnMut()>>,
    cached: Vec<Line>,
}

impl BorderedLoader {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_cancellable(message, true)
    }

    #[must_use]
    pub fn with_cancellable(message: impl Into<String>, cancellable: bool) -> Self {
        Self {
            loader: Loader::new(
                Box::new(|spinner| theme().fg(ThemeColor::Accent, spinner)),
                Box::new(|text| theme().fg(ThemeColor::Muted, text)),
                message,
                None,
                None,
            ),
            cancellable,
            cancelled: false,
            on_abort: None,
            cached: Vec::new(),
        }
    }

    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn set_on_abort(&mut self, on_abort: Option<Box<dyn FnMut()>>) {
        self.on_abort = on_abort;
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.loader.set_message(message);
    }

    pub fn dispose(&mut self) {
        self.loader.stop();
    }
}

impl Component for BorderedLoader {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        self.cached.extend_from_slice(self.loader.render(width));
        if self.cancellable {
            self.cached
                .push(Line::plain(" ".repeat(usize::from(width))));
            self.cached
                .push(Line::from_ansi(&key_hint("tui.select.cancel", "cancel")));
        }
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        if self.cancellable
            && pi_tui::keybindings::get_keybindings().matches(data, "tui.select.cancel")
            && !self.cancelled
        {
            self.cancelled = true;
            if let Some(on_abort) = &mut self.on_abort {
                on_abort();
            }
            self.loader.stop();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

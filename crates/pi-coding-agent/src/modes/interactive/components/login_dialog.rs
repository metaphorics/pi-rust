//! OAuth and credential-entry dialog ported from `login-dialog.ts`.

use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::components::{Input, Text};
use pi_tui::line::Line;
use pi_tui::util::visible_width;

use super::keybinding_hints::key_hint;
use crate::modes::interactive::theme::{ThemeColor, theme};

pub struct LoginDialogComponent {
    title: String,
    content: Vec<String>,
    input: Input,
    input_visible: bool,
    input_hint: Option<String>,
    masked: bool,
    focused: bool,
    cancelled: bool,
    cached: Vec<Line>,
    pub on_complete: Box<dyn FnMut(bool, Option<String>)>,
    pub on_submit: Option<Box<dyn FnMut(String)>>,
}

impl LoginDialogComponent {
    #[must_use]
    pub fn new(
        provider_id: &str,
        on_complete: impl FnMut(bool, Option<String>) + 'static,
        provider_name: Option<&str>,
        title: Option<&str>,
    ) -> Self {
        let provider_name = provider_name.unwrap_or(provider_id);
        Self {
            title: title
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Login to {provider_name}")),
            content: Vec::new(),
            input: Input::default(),
            input_visible: false,
            input_hint: None,
            masked: false,
            focused: false,
            cancelled: false,
            cached: Vec::new(),
            on_complete: Box::new(on_complete),
            on_submit: None,
        }
    }

    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    pub fn show_auth(&mut self, url: &str, instructions: Option<&str>) {
        let linked_url = format!("\x1b]8;;{url}\x07{url}\x1b]8;;\x07");
        let click_hint = if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        };
        let hyperlink = format!("\x1b]8;;{url}\x07{click_hint}\x1b]8;;\x07");
        self.content = vec![
            String::new(),
            theme().fg(ThemeColor::Accent, &linked_url),
            theme().fg(ThemeColor::Dim, &hyperlink),
        ];
        if let Some(instructions) = instructions {
            self.content.push(String::new());
            self.content
                .push(theme().fg(ThemeColor::Warning, instructions));
        }
        self.input_visible = false;
        self.input_hint = None;
    }

    pub fn show_device_code(&mut self, url: &str, code: &str) {
        let linked_url = format!("\x1b]8;;{url}\x07{url}\x1b]8;;\x07");
        let click_hint = if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        };
        let hyperlink = format!("\x1b]8;;{url}\x07{click_hint}\x1b]8;;\x07");
        self.content = vec![
            String::new(),
            theme().fg(ThemeColor::Accent, &linked_url),
            theme().fg(ThemeColor::Dim, &hyperlink),
            String::new(),
            theme().fg(ThemeColor::Warning, &format!("Enter code: {code}")),
        ];
        self.input_visible = false;
        self.input_hint = None;
    }

    pub fn show_manual_input(&mut self, prompt: &str) {
        self.input.set_value("");
        self.content.push(String::new());
        self.content.push(theme().fg(ThemeColor::Dim, prompt));
        self.input_hint = Some(format!("({})", key_hint("tui.select.cancel", "to cancel")));
        self.input_visible = true;
    }

    pub fn show_prompt(&mut self, message: &str, placeholder: Option<&str>) {
        self.content.push(String::new());
        self.content.push(theme().fg(ThemeColor::Text, message));
        if let Some(placeholder) = placeholder {
            self.content
                .push(theme().fg(ThemeColor::Dim, &format!("e.g., {placeholder}")));
        }
        self.input_hint = Some(format!(
            "({} {})",
            key_hint("tui.select.cancel", "to cancel,"),
            key_hint("tui.select.confirm", "to submit")
        ));
        self.input.set_value("");
        self.input_visible = true;
    }

    pub fn show_details(&mut self, lines: &[String]) {
        self.content.clear();
        self.content.push(String::new());
        self.content.extend_from_slice(lines);
        self.input_visible = false;
        self.input_hint = None;
    }

    pub fn show_info(&mut self, lines: &[String]) {
        self.show_details(lines);
        self.content.push(String::new());
        self.content
            .push(format!("({})", key_hint("tui.select.cancel", "to close")));
    }

    pub fn show_waiting(&mut self, message: &str) {
        self.content.push(String::new());
        self.content.push(theme().fg(ThemeColor::Dim, message));
        self.content
            .push(format!("({})", key_hint("tui.select.cancel", "to cancel")));
        self.input_visible = false;
        self.input_hint = None;
    }

    pub fn show_progress(&mut self, message: &str) {
        self.content.push(theme().fg(ThemeColor::Dim, message));
    }

    pub fn complete(&mut self, success: bool, message: Option<String>) {
        (self.on_complete)(success, message);
    }

    fn cancel(&mut self) {
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        (self.on_complete)(false, Some("Login cancelled".to_owned()));
    }

    fn submit(&mut self) {
        if !self.input_visible {
            return;
        }
        let value = self.input.get_value().to_owned();
        let displayed = if self.masked {
            "•".repeat(value.chars().count())
        } else {
            value.clone()
        };
        self.content.push(format!("> {displayed}"));
        self.input_visible = false;
        if let Some(on_submit) = &mut self.on_submit {
            on_submit(value);
        }
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    if text.is_empty() {
        lines.push(Line::plain(" ".repeat(usize::from(width))));
        return;
    }
    let mut component = Text::new(text, 1, 0, None);
    lines.extend_from_slice(component.render(width));
}

impl Component for LoginDialogComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        let title = theme().fg(ThemeColor::Accent, &theme().bold(&self.title));
        append_text(&mut self.cached, &title, width);
        for content in &self.content {
            append_text(&mut self.cached, content, width);
        }
        if self.input_visible {
            if self.masked {
                let value = "•".repeat(self.input.get_value().chars().count());
                let fill = " ".repeat(
                    usize::from(width)
                        .saturating_sub(visible_width(&value))
                        .saturating_sub(2),
                );
                self.cached.push(Line::plain(format!(" {value}{fill} ")));
            } else {
                self.cached.extend_from_slice(self.input.render(width));
            }
        }
        if let Some(hint) = &self.input_hint {
            append_text(&mut self.cached, hint, width);
        }
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let cancel = pi_tui::keybindings::get_keybindings().matches(data, "tui.select.cancel");
        let confirm = pi_tui::keybindings::get_keybindings().matches(data, "tui.select.confirm");
        if cancel {
            self.cancel();
        } else if self.input_visible && (confirm || data == "\n") {
            self.submit();
        } else if self.input_visible {
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

impl Focusable for LoginDialogComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

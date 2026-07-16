//! Streaming bash execution transcript block.
use super::dynamic_border::DynamicBorder;
use crate::modes::interactive::theme::{ThemeColor, theme};
use pi_tui::components::{loader::Loader, spacer::Spacer, text::Text};
use pi_tui::{Component, Container, Line, RenderStatus};
const PREVIEW_LINES: usize = 20;
pub struct BashExecutionComponent {
    command: String,
    output: Vec<String>,
    running: bool,
    cancelled: bool,
    exit_code: Option<i32>,
    expanded: bool,
    exclude_from_context: bool,
    inner: Container,
    status: RenderStatus,
}
impl BashExecutionComponent {
    #[must_use]
    pub fn new(command: impl Into<String>, exclude_from_context: bool) -> Self {
        let mut s = Self {
            command: command.into(),
            output: Vec::new(),
            running: true,
            cancelled: false,
            exit_code: None,
            expanded: false,
            exclude_from_context,
            inner: Container::new(),
            status: RenderStatus::Changed,
        };
        s.rebuild();
        s
    }
    pub fn append_output(&mut self, chunk: &str) {
        let clean = chunk.replace("\r\n", "\n").replace('\r', "\n");
        let mut lines = clean.split('\n');
        if let Some(first) = lines.next() {
            if let Some(last) = self.output.last_mut() {
                last.push_str(first)
            } else {
                self.output.push(first.into())
            }
        }
        self.output.extend(lines.map(str::to_owned));
        self.rebuild()
    }
    pub fn set_complete(&mut self, exit_code: Option<i32>, cancelled: bool) {
        self.exit_code = exit_code;
        self.cancelled = cancelled;
        self.running = false;
        self.rebuild()
    }
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild()
        }
    }
    #[must_use]
    pub fn output(&self) -> String {
        self.output.join("\n")
    }
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }
    fn rebuild(&mut self) {
        let color = if self.exclude_from_context {
            ThemeColor::Dim
        } else {
            ThemeColor::BashMode
        };
        let mut inner = Container::new();
        inner.add_child(Spacer::new(1));
        inner.add_child(DynamicBorder::new(Some(Box::new(move |s| {
            theme().fg(color, s)
        }))));
        inner.add_child(Text::new(
            theme().fg(color, &theme().bold(&format!("$ {}", self.command))),
            1,
            0,
            None,
        ));
        let total = self.output.len();
        let start = if self.expanded {
            0
        } else {
            total.saturating_sub(PREVIEW_LINES)
        };
        let display = self.output[start..].join("\n");
        if !display.is_empty() {
            inner.add_child(Text::new(
                format!("\n{}", theme().fg(ThemeColor::Muted, &display)),
                1,
                0,
                None,
            ));
        }
        if self.running {
            let mut loader = Loader::new(
                Box::new(move |s| theme().fg(color, s)),
                Box::new(|s| theme().fg(ThemeColor::Muted, s)),
                "Running... (Ctrl+C to cancel)",
                None,
                None,
            );
            loader.start();
            inner.add_child(loader);
        } else {
            let mut status = Vec::new();
            if total > PREVIEW_LINES && !self.expanded {
                status.push(format!(
                    "... {} more lines (expand to expand)",
                    total - PREVIEW_LINES
                ));
            }
            if self.cancelled {
                status.push("(cancelled)".into())
            } else if self.exit_code.is_some_and(|code| code != 0) {
                status.push(format!("(exit {})", self.exit_code.unwrap_or_default()));
            }
            if !status.is_empty() {
                inner.add_child(Text::new(format!("\n{}", status.join("\n")), 1, 0, None));
            }
        }
        inner.add_child(DynamicBorder::new(Some(Box::new(move |s| {
            theme().fg(color, s)
        }))));
        self.inner = inner;
        self.status = RenderStatus::Changed
    }
}
impl Component for BashExecutionComponent {
    fn render(&mut self, w: u16) -> &[Line] {
        self.status = self.inner.last_render_status();
        self.inner.render(w)
    }
    fn invalidate(&mut self) {
        self.rebuild()
    }
    fn last_render_status(&self) -> RenderStatus {
        self.inner.last_render_status()
    }
}

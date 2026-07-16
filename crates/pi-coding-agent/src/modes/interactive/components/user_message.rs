//! User transcript message.

use crate::modes::interactive::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use pi_tui::components::{
    box_widget::BoxWidget,
    markdown::{DefaultTextStyle, Markdown, MarkdownOptions},
};
use pi_tui::{Component, Line, RenderStatus};

pub struct UserMessageComponent {
    text: String,
    output_pad: usize,
    inner: BoxWidget,
    status: RenderStatus,
}
impl UserMessageComponent {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let mut value = Self {
            text: text.into(),
            output_pad: 1,
            inner: BoxWidget::new(
                1,
                1,
                Some(Box::new(|s| theme().bg(ThemeBg::UserMessageBg, s))),
            ),
            status: RenderStatus::Changed,
        };
        value.rebuild();
        value
    }
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        self.rebuild();
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    fn rebuild(&mut self) {
        self.inner = BoxWidget::new(
            self.output_pad,
            1,
            Some(Box::new(|s| theme().bg(ThemeBg::UserMessageBg, s))),
        );
        let style = DefaultTextStyle {
            color: Some(std::sync::Arc::new(|s| {
                theme().fg(ThemeColor::UserMessageText, s)
            })),
            ..Default::default()
        };
        self.inner.add_child(Box::new(Markdown::new(
            &self.text,
            0,
            0,
            get_markdown_theme(),
            Some(style),
            Some(MarkdownOptions {
                preserve_ordered_list_markers: true,
                preserve_backslash_escapes: true,
            }),
        )));
        self.status = RenderStatus::Changed;
    }
}
impl Component for UserMessageComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.status = self.inner.last_render_status();
        self.inner.render(width)
    }
    fn invalidate(&mut self) {
        self.inner.invalidate();
        self.status = RenderStatus::Changed;
    }
    fn last_render_status(&self) -> RenderStatus {
        self.inner.last_render_status()
    }
}

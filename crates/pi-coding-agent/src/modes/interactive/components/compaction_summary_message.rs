//! Compaction summary transcript entry.
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use pi_tui::components::{
    box_widget::BoxWidget,
    markdown::{DefaultTextStyle, Markdown},
    spacer::Spacer,
    text::Text,
};
use pi_tui::{Component, Line, RenderStatus};
pub struct CompactionSummaryMessageComponent {
    summary: String,
    tokens_before: u64,
    expanded: bool,
    inner: BoxWidget,
    status: RenderStatus,
}
impl CompactionSummaryMessageComponent {
    #[must_use]
    pub fn new(summary: impl Into<String>, tokens_before: u64) -> Self {
        let mut s = Self {
            summary: summary.into(),
            tokens_before,
            expanded: false,
            inner: BoxWidget::new(1, 1, None),
            status: RenderStatus::Changed,
        };
        s.rebuild();
        s
    }
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild()
        }
    }
    fn rebuild(&mut self) {
        let mut b = BoxWidget::new(
            1,
            1,
            Some(Box::new(|s| theme().bg(ThemeBg::CustomMessageBg, s))),
        );
        b.add_child(Box::new(Text::new(
            theme().fg(
                ThemeColor::CustomMessageLabel,
                "\x1b[1m[compaction]\x1b[22m",
            ),
            0,
            0,
            None,
        )));
        b.add_child(Box::new(Spacer::new(1)));
        let style = DefaultTextStyle {
            color: Some(std::sync::Arc::new(|s| {
                theme().fg(ThemeColor::CustomMessageText, s)
            })),
            ..Default::default()
        };
        if self.expanded {
            b.add_child(Box::new(Markdown::new(
                format!(
                    "**Compacted from {} tokens**\n\n{}",
                    self.tokens_before, self.summary
                ),
                0,
                0,
                get_markdown_theme(),
                Some(style),
                None,
            )));
        } else {
            b.add_child(Box::new(Text::new(
                theme().fg(
                    ThemeColor::CustomMessageText,
                    &format!(
                        "Compacted from {} tokens (expand to expand)",
                        self.tokens_before
                    ),
                ),
                0,
                0,
                None,
            )));
        }
        self.inner = b;
        self.status = RenderStatus::Changed
    }
}
impl Component for CompactionSummaryMessageComponent {
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

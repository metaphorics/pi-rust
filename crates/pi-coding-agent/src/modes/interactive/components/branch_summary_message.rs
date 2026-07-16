//! Branch summary transcript entry.
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use pi_tui::components::{
    box_widget::BoxWidget,
    markdown::{DefaultTextStyle, Markdown},
    spacer::Spacer,
    text::Text,
};
use pi_tui::{Component, Line, RenderStatus};
pub struct BranchSummaryMessageComponent {
    summary: String,
    expanded: bool,
    inner: BoxWidget,
    status: RenderStatus,
}
impl BranchSummaryMessageComponent {
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        let mut v = Self {
            summary: summary.into(),
            expanded: false,
            inner: BoxWidget::new(1, 1, None),
            status: RenderStatus::Changed,
        };
        v.rebuild();
        v
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
            theme().fg(ThemeColor::CustomMessageLabel, "\x1b[1m[branch]\x1b[22m"),
            0,
            0,
            None,
        )));
        b.add_child(Box::new(Spacer::new(1)));
        if self.expanded {
            b.add_child(Box::new(Markdown::new(
                format!("**Branch Summary**\n\n{}", self.summary),
                0,
                0,
                get_markdown_theme(),
                Some(DefaultTextStyle {
                    color: Some(std::sync::Arc::new(|s| {
                        theme().fg(ThemeColor::CustomMessageText, s)
                    })),
                    ..Default::default()
                }),
                None,
            )));
        } else {
            b.add_child(Box::new(Text::new(
                theme().fg(
                    ThemeColor::CustomMessageText,
                    "Branch summary (expand to expand)",
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
impl Component for BranchSummaryMessageComponent {
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

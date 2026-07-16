//! Skill invocation transcript entry.
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use pi_tui::components::{
    box_widget::BoxWidget,
    markdown::{DefaultTextStyle, Markdown},
    text::Text,
};
use pi_tui::{Component, Line, RenderStatus};
pub struct SkillInvocationMessageComponent {
    name: String,
    content: String,
    expanded: bool,
    inner: BoxWidget,
    status: RenderStatus,
}
impl SkillInvocationMessageComponent {
    #[must_use]
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Self {
        let mut v = Self {
            name: name.into(),
            content: content.into(),
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
        if self.expanded {
            b.add_child(Box::new(Text::new(
                theme().fg(ThemeColor::CustomMessageLabel, "\x1b[1m[skill]\x1b[22m"),
                0,
                0,
                None,
            )));
            b.add_child(Box::new(Markdown::new(
                format!("**{}**\n\n{}", self.name, self.content),
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
                format!(
                    "{}{}{}",
                    theme().fg(ThemeColor::CustomMessageLabel, "\x1b[1m[skill]\x1b[22m "),
                    theme().fg(ThemeColor::CustomMessageText, &self.name),
                    theme().fg(ThemeColor::Dim, " (expand to expand)")
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
impl Component for SkillInvocationMessageComponent {
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

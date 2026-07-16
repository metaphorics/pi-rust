//! Extension custom message transcript entry.
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use pi_ai::Content;
use pi_tui::components::{
    box_widget::BoxWidget,
    markdown::{DefaultTextStyle, Markdown},
    spacer::Spacer,
    text::Text,
};
use pi_tui::{Component, Line, RenderStatus};
pub struct CustomMessageComponent {
    custom_type: String,
    content: Vec<Content>,
    expanded: bool,
    inner: BoxWidget,
    status: RenderStatus,
}
impl CustomMessageComponent {
    #[must_use]
    pub fn new(custom_type: impl Into<String>, content: Vec<Content>) -> Self {
        let mut v = Self {
            custom_type: custom_type.into(),
            content,
            expanded: false,
            inner: BoxWidget::new(1, 1, None),
            status: RenderStatus::Changed,
        };
        v.rebuild();
        v
    }
    #[must_use]
    pub fn from_text(custom_type: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(
            custom_type,
            vec![Content::Text(pi_ai::TextContent {
                text: text.into().into(),
                text_signature: None,
            })],
        )
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
                &format!("\x1b[1m[{}]\x1b[22m", self.custom_type),
            ),
            0,
            0,
            None,
        )));
        b.add_child(Box::new(Spacer::new(1)));
        let text = self
            .content
            .iter()
            .filter_map(|c| {
                if let Content::Text(t) = c {
                    Some(t.text.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        b.add_child(Box::new(Markdown::new(
            text,
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
        self.inner = b;
        self.status = RenderStatus::Changed
    }
}
impl Component for CustomMessageComponent {
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

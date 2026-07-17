//! Streaming assistant transcript message.
use crate::modes::interactive::theme::{ThemeColor, get_markdown_theme, theme};
use pi_ai::{AssistantMessage, Content, StopReason};
use pi_tui::components::{
    markdown::{DefaultTextStyle, Markdown},
    spacer::Spacer,
    text::Text,
};
use pi_tui::{Component, Container, Line, RenderStatus};

pub struct AssistantMessageComponent {
    inner: Container,
    message: Option<AssistantMessage>,
    hide_thinking: bool,
    hidden_thinking_label: String,
    output_pad: usize,
    status: RenderStatus,
}
impl AssistantMessageComponent {
    #[must_use]
    pub fn new(message: Option<AssistantMessage>) -> Self {
        let mut s = Self {
            inner: Container::new(),
            message: None,
            hide_thinking: false,
            hidden_thinking_label: "Thinking...".into(),
            output_pad: 1,
            status: RenderStatus::Changed,
        };
        if let Some(m) = message {
            s.update_message(m)
        }
        s
    }
    pub fn update_message(&mut self, message: AssistantMessage) {
        self.message = Some(message);
        self.rebuild()
    }
    pub fn update_content(&mut self, message: AssistantMessage) {
        self.update_message(message)
    }
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking = hide;
        self.rebuild()
    }
    pub fn set_hidden_thinking_label(&mut self, label: impl Into<String>) {
        self.hidden_thinking_label = label.into();
        self.rebuild()
    }
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        self.rebuild()
    }
    fn style(color: ThemeColor, italic: bool) -> DefaultTextStyle {
        DefaultTextStyle {
            color: Some(std::sync::Arc::new(move |s| theme().fg(color, s))),
            italic,
            ..Default::default()
        }
    }
    fn rebuild(&mut self) {
        let mut inner = Container::new();
        let Some(message) = self.message.as_ref() else {
            self.inner = inner;
            return;
        };
        let visible = message.content.iter().any(|c| match c {
            Content::Text(t) => !t.text.is_empty(),
            Content::Thinking(t) => !t.thinking.is_empty(),
            _ => false,
        });
        if visible {
            inner.add_child(Spacer::new(1));
        }
        for (index, content) in message.content.iter().enumerate() {
            match content {
                Content::Text(text) if !text.text.is_empty() => inner.add_child(Markdown::new(
                    text.text.as_string(),
                    self.output_pad,
                    0,
                    get_markdown_theme(),
                    None,
                    None,
                )),
                Content::Thinking(thinking) if !thinking.thinking.is_empty() => {
                    if self.hide_thinking {
                        inner.add_child(Text::new(
                            theme().italic(
                                &theme().fg(ThemeColor::ThinkingText, &self.hidden_thinking_label),
                            ),
                            self.output_pad,
                            0,
                            None,
                        ));
                    } else {
                        inner.add_child(Markdown::new(
                            thinking.thinking.as_string(),
                            self.output_pad,
                            0,
                            get_markdown_theme(),
                            Some(Self::style(ThemeColor::ThinkingText, true)),
                            None,
                        ));
                    }
                    if message.content[index + 1..].iter().any(|c| {
                        matches!(c,Content::Text(t) if !t.text.is_empty())
                            || matches!(c,Content::Thinking(t) if !t.thinking.is_empty())
                    }) {
                        inner.add_child(Spacer::new(1));
                    }
                }
                _ => {}
            }
        }
        let calls = message
            .content
            .iter()
            .any(|c| matches!(c, Content::ToolCall(_)));
        let error=match message.stop_reason {StopReason::Length=>Some("Error: Model stopped because it reached the maximum output token limit. The response may be incomplete.".to_owned()),StopReason::Aborted if !calls=>Some(message.error_message.as_deref().filter(|s|*s!="Request was aborted").unwrap_or("Operation aborted").to_owned()),StopReason::Error if !calls=>Some(format!("Error: {}",message.error_message.as_deref().unwrap_or("Unknown error"))),_=>None};
        if let Some(error) = error {
            inner.add_child(Spacer::new(1));
            inner.add_child(Text::new(
                theme().fg(ThemeColor::Error, &error),
                self.output_pad,
                0,
                None,
            ));
        }
        self.inner = inner;
        self.status = RenderStatus::Changed;
    }
}
impl Component for AssistantMessageComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.status = self.inner.last_render_status();
        self.inner.render(width)
    }
    fn invalidate(&mut self) {
        self.rebuild()
    }
    fn last_render_status(&self) -> RenderStatus {
        self.inner.last_render_status()
    }
}

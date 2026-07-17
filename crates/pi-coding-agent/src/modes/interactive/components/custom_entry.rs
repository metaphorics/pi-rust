//! Extension custom session entry renderer.
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, theme};
use pi_tui::components::{box_widget::BoxWidget, spacer::Spacer, text::Text};
use pi_tui::{Component, ComponentBox, Container, Line, RenderStatus};

pub type EntryRenderer = Box<dyn Fn(&str, bool) -> Result<Option<ComponentBox>, String>>;

pub struct CustomEntryComponent {
    custom_type: String,
    body: String,
    renderer: Option<EntryRenderer>,
    expanded: bool,
    inner: Container,
    has_content: bool,
}
impl CustomEntryComponent {
    #[must_use]
    pub fn new(custom_type: impl Into<String>, body: impl Into<String>) -> Self {
        Self::with_renderer(custom_type, body, None)
    }
    #[must_use]
    pub fn with_renderer(
        custom_type: impl Into<String>,
        body: impl Into<String>,
        renderer: Option<EntryRenderer>,
    ) -> Self {
        let mut value = Self {
            custom_type: custom_type.into(),
            body: body.into(),
            renderer,
            expanded: false,
            inner: Container::new(),
            has_content: false,
        };
        value.rebuild();
        value
    }
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.has_content
    }
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }
    fn error_component(&self, message: String) -> ComponentBox {
        let mut box_widget = BoxWidget::new(
            1,
            1,
            Some(Box::new(|s| theme().bg(ThemeBg::CustomMessageBg, s))),
        );
        box_widget.add_child(Box::new(Text::new(
            theme().fg(
                ThemeColor::Error,
                &format!("[{}] renderer failed: {}", self.custom_type, message),
            ),
            0,
            0,
            None,
        )));
        Box::new(box_widget)
    }
    fn fallback_component(&self) -> ComponentBox {
        let mut box_widget = BoxWidget::new(
            1,
            1,
            Some(Box::new(|s| theme().bg(ThemeBg::CustomMessageBg, s))),
        );
        box_widget.add_child(Box::new(Text::new(
            theme().fg(
                ThemeColor::CustomMessageText,
                &format!("[{}] {}", self.custom_type, self.body),
            ),
            0,
            0,
            None,
        )));
        Box::new(box_widget)
    }
    fn rebuild(&mut self) {
        self.inner = Container::new();
        self.has_content = false;
        let component = match &self.renderer {
            Some(renderer) => match renderer(&self.body, self.expanded) {
                Ok(component) => component,
                Err(message) => Some(self.error_component(message)),
            },
            None if self.body.is_empty() => None,
            None => Some(self.fallback_component()),
        };
        if let Some(component) = component {
            self.inner.add_child(Spacer::new(1));
            self.inner.add_child_box(component);
            self.has_content = true;
        }
    }
}
impl Component for CustomEntryComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.inner.render(width)
    }
    fn invalidate(&mut self) {
        self.rebuild();
    }
    fn last_render_status(&self) -> RenderStatus {
        self.inner.last_render_status()
    }
}

//! Streaming tool execution transcript block.
use super::{diff::render_diff, visual_truncate::truncate_to_visual_lines};
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, theme};
use pi_agent::AgentToolResult;
use pi_ai::Content;
use pi_tui::components::image::{Image, ImageOptions, ImageTheme};
use pi_tui::components::{box_widget::BoxWidget, text::Text};
use pi_tui::{Component, Line, RenderStatus};
use serde_json::Value;
const PREVIEW_LINES: usize = 20;
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Success,
    Error,
}
pub struct ToolExecutionComponent {
    tool_call_id: String,
    name: String,
    args: Value,
    result: Option<AgentToolResult>,
    state: State,
    expanded: bool,
    inner: BoxWidget,
    status: RenderStatus,
    render_width: Option<u16>,
    show_images: bool,
    image_width_cells: u32,
}
impl ToolExecutionComponent {
    #[must_use]
    pub fn new(name: impl Into<String>, args: Value) -> Self {
        let mut v = Self {
            tool_call_id: String::new(),
            name: name.into(),
            args,
            result: None,
            state: State::Pending,
            expanded: false,
            inner: BoxWidget::new(1, 1, None),
            status: RenderStatus::Changed,
            render_width: None,
            show_images: true,
            image_width_cells: 60,
        };
        v.rebuild();
        v
    }
    pub fn update(&mut self, result: AgentToolResult) {
        self.result = Some(result);
        self.render_width = None;
        self.rebuild()
    }
    #[must_use]
    pub fn with_call_id(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        args: Value,
    ) -> Self {
        let mut component = Self::new(name, args);
        component.tool_call_id = tool_call_id.into();
        component
    }
    #[must_use]
    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    pub fn update_args(&mut self, args: Value) {
        self.args = args;
        self.render_width = None;
        self.rebuild();
    }
    pub fn update_partial(&mut self, args: Value, result: AgentToolResult) {
        self.args = args;
        self.update(result);
    }
    pub fn update_result(&mut self, result: AgentToolResult) {
        self.update(result)
    }
    pub fn end(&mut self, result: AgentToolResult, is_error: bool) {
        self.result = Some(result);
        self.state = if is_error {
            State::Error
        } else {
            State::Success
        };
        self.render_width = None;
        self.rebuild()
    }
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.render_width = None;
            self.rebuild()
        }
    }
    pub fn set_show_images(&mut self, show_images: bool) {
        if self.show_images != show_images {
            self.show_images = show_images;
            self.render_width = None;
            self.rebuild();
        }
    }
    pub fn set_image_width_cells(&mut self, width: u32) {
        let width = width.max(1);
        if self.image_width_cells != width {
            self.image_width_cells = width;
            self.render_width = None;
            self.rebuild();
        }
    }
    fn output(&self) -> String {
        self.result
            .as_ref()
            .map(|r| {
                r.content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text(t) => Some(t.text.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
    fn rebuild(&mut self) {
        let bg = match self.state {
            State::Pending => ThemeBg::ToolPendingBg,
            State::Success => ThemeBg::ToolSuccessBg,
            State::Error => ThemeBg::ToolErrorBg,
        };
        let mut b = BoxWidget::new(1, 1, Some(Box::new(move |s| theme().bg(bg, s))));
        let args = match &self.args {
            Value::Object(map) if map.is_empty() => String::new(),
            other => format!(" {}", other),
        };
        b.add_child(Box::new(Text::new(
            theme().fg(
                ThemeColor::ToolTitle,
                &theme().bold(&format!("{}{}", self.name, args)),
            ),
            0,
            0,
            None,
        )));
        let output = self.output();
        if !output.is_empty() {
            let rendered = if matches!(self.name.as_str(), "edit" | "write") {
                render_diff(&output)
            } else {
                theme().fg(ThemeColor::ToolOutput, &output)
            };
            let display = if self.expanded {
                rendered
            } else {
                let lines = truncate_to_visual_lines(
                    &rendered,
                    PREVIEW_LINES,
                    self.render_width.unwrap_or(80),
                    0,
                );
                lines.visual_lines.join("\n")
            };
            b.add_child(Box::new(Text::new(format!("\n{}", display), 0, 0, None)));
            if !self.expanded && output.lines().count() > PREVIEW_LINES {
                b.add_child(Box::new(Text::new(
                    theme().fg(ThemeColor::Muted, "... more lines (expand to expand)"),
                    0,
                    0,
                    None,
                )));
            }
        }
        if self.show_images
            && let Some(result) = &self.result
        {
            for image in result.content.iter().filter_map(|content| match content {
                Content::Image(image) => Some(image),
                _ => None,
            }) {
                b.add_child(Box::new(Image::new(
                    image.data.clone(),
                    image.mime_type.clone(),
                    ImageTheme {
                        fallback_color: std::sync::Arc::new(|s| {
                            theme().fg(ThemeColor::ToolOutput, s)
                        }),
                    },
                    ImageOptions {
                        max_width_cells: Some(self.image_width_cells),
                        ..Default::default()
                    },
                    None,
                )));
            }
        }
        self.inner = b;
        self.status = RenderStatus::Changed
    }
}
impl Component for ToolExecutionComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        let cache_hit = self.render_width == Some(width);
        if !cache_hit {
            self.render_width = Some(width);
            self.rebuild();
        }
        self.status = if cache_hit {
            RenderStatus::Unchanged
        } else {
            RenderStatus::Changed
        };
        self.inner.render(width)
    }
    fn invalidate(&mut self) {
        self.render_width = None;
        self.rebuild()
    }
    fn last_render_status(&self) -> RenderStatus {
        self.status
    }
}

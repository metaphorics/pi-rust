//! Dynamic full-width transcript border.

use crate::modes::interactive::theme::{ThemeColor, theme};
use pi_tui::{Component, Line, RenderStatus};
type ColorFn = Box<dyn Fn(&str) -> String>;

pub struct DynamicBorder {
    color: ColorFn,
    lines: Vec<Line>,
    width: Option<u16>,
    status: RenderStatus,
}

impl DynamicBorder {
    #[must_use]
    pub fn new(color: Option<ColorFn>) -> Self {
        Self {
            color: color.unwrap_or_else(|| Box::new(|s| theme().fg(ThemeColor::Border, s))),
            lines: Vec::new(),
            width: None,
            status: RenderStatus::Changed,
        }
    }
}
impl Default for DynamicBorder {
    fn default() -> Self {
        Self::new(None)
    }
}
impl Component for DynamicBorder {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.width == Some(width) {
            self.status = RenderStatus::Unchanged;
            return &self.lines;
        }
        self.width = Some(width);
        self.lines = vec![Line::from_ansi(&(self.color)(
            &"─".repeat(usize::from(width.max(1))),
        ))];
        self.status = RenderStatus::Changed;
        &self.lines
    }
    fn invalidate(&mut self) {
        self.width = None;
    }
    fn last_render_status(&self) -> RenderStatus {
        self.status
    }
}

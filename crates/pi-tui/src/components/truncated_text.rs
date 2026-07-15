//! TruncatedText — single-line text truncated to viewport width.
//!
//! Port of `packages/tui/src/components/truncated-text.ts`.

use crate::component::{Component, RenderStatus};
use crate::line::Line;
use crate::util::{truncate_to_width, visible_width};

/// First-line-only text, truncated and padded to width.
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
    last_status: RenderStatus,
    cached: Vec<Line>,
}

impl TruncatedText {
    #[must_use]
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            last_status: RenderStatus::Changed,
            cached: Vec::new(),
        }
    }

    /// Defaults: `padding_x = 0`, `padding_y = 0`.
    #[must_use]
    pub fn with_text(text: impl Into<String>) -> Self {
        Self::new(text, 0, 0)
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for TruncatedText {
    fn render(&mut self, width: u16) -> &[Line] {
        let w = width as usize;
        let empty_line = " ".repeat(w);
        let mut result: Vec<Line> = Vec::with_capacity(1 + self.padding_y.saturating_mul(2));

        for _ in 0..self.padding_y {
            result.push(Line::plain(&empty_line));
        }

        let available = w.saturating_sub(self.padding_x.saturating_mul(2)).max(1);

        let single_line = match self.text.find('\n') {
            Some(i) => &self.text[..i],
            None => self.text.as_str(),
        };
        let display = truncate_to_width(single_line, available);

        let left = " ".repeat(self.padding_x);
        let right = " ".repeat(self.padding_x);
        let with_pad = format!("{left}{display}{right}");
        let pad_needed = w.saturating_sub(visible_width(&with_pad));
        let final_line = format!("{with_pad}{}", " ".repeat(pad_needed));
        result.push(Line::from_ansi(&final_line));

        for _ in 0..self.padding_y {
            result.push(Line::plain(&empty_line));
        }

        self.cached = result;
        self.last_status = RenderStatus::Changed;
        &self.cached
    }

    fn invalidate(&mut self) {
        // No durable cache.
    }

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;
    use crate::util::visible_width;

    #[test]
    fn first_line_only_and_pad_to_width() {
        let mut t = TruncatedText::new("hello\nworld", 0, 0);
        let lines = t.render(20);
        assert_eq!(lines.len(), 1);
        let ansi = lines[0].to_ansi();
        assert!(!ansi.contains("world"));
        assert_eq!(visible_width(&ansi), 20);
    }

    #[test]
    fn truncates_long_line() {
        let mut t = TruncatedText::with_text("abcdefghijklmnopqrstuvwxyz");
        let lines = t.render(10);
        assert_eq!(visible_width(&lines[0].to_ansi()), 10);
    }
}

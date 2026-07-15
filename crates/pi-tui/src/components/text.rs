//! Text component — multi-line word-wrapped text with padding/background.
//!
//! Port of `packages/tui/src/components/text.ts`.

use crate::component::{Component, RenderStatus};
use crate::line::Line;
use crate::util::{apply_background_to_line, visible_width, wrap_text_with_ansi};

type BgFn = Box<dyn Fn(&str) -> String>;

/// Multi-line text widget with word wrap, padding, and optional background.
pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    custom_bg_fn: Option<BgFn>,
    cached_text: Option<String>,
    cached_width: Option<u16>,
    cached_lines: Option<Vec<Line>>,
    last_status: RenderStatus,
}

impl Text {
    #[must_use]
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        custom_bg_fn: Option<BgFn>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            custom_bg_fn,
            cached_text: None,
            cached_width: None,
            cached_lines: None,
            last_status: RenderStatus::Changed,
        }
    }

    /// Defaults: `padding_x = 1`, `padding_y = 1`, no background.
    #[must_use]
    pub fn with_text(text: impl Into<String>) -> Self {
        Self::new(text, 1, 1, None)
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.invalidate_cache();
    }

    pub fn set_custom_bg_fn(&mut self, custom_bg_fn: Option<BgFn>) {
        self.custom_bg_fn = custom_bg_fn;
        self.invalidate_cache();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn invalidate_cache(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }

    fn rebuild(&mut self, width: u16) {
        // Empty / whitespace-only → no lines (TS: empty result, cache it).
        if self.text.trim().is_empty() {
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            self.cached_lines = Some(Vec::new());
            self.last_status = RenderStatus::Changed;
            return;
        }

        let normalized = self.text.replace('\t', "   ");
        let content_width = (width as usize)
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let wrapped = wrap_text_with_ansi(&normalized, content_width);

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let mut content_lines: Vec<String> = Vec::with_capacity(wrapped.len());

        for line in wrapped {
            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            if let Some(bg) = &self.custom_bg_fn {
                content_lines.push(apply_background_to_line(
                    &line_with_margins,
                    width as usize,
                    bg.as_ref(),
                ));
            } else {
                let visible_len = visible_width(&line_with_margins);
                let pad = (width as usize).saturating_sub(visible_len);
                content_lines.push(format!("{line_with_margins}{}", " ".repeat(pad)));
            }
        }

        let empty_raw = " ".repeat(width as usize);
        let empty_line = if let Some(bg) = &self.custom_bg_fn {
            apply_background_to_line(&empty_raw, width as usize, bg.as_ref())
        } else {
            empty_raw
        };

        let mut result: Vec<Line> =
            Vec::with_capacity(content_lines.len() + self.padding_y.saturating_mul(2));
        for _ in 0..self.padding_y {
            result.push(Line::from_ansi(&empty_line));
        }
        for line in content_lines {
            result.push(Line::from_ansi(&line));
        }
        for _ in 0..self.padding_y {
            result.push(Line::from_ansi(&empty_line));
        }

        if result.is_empty() {
            result.push(Line::plain(""));
        }

        self.cached_text = Some(self.text.clone());
        self.cached_width = Some(width);
        self.cached_lines = Some(result);
        self.last_status = RenderStatus::Changed;
    }
}

impl Default for Text {
    fn default() -> Self {
        Self::new(String::new(), 1, 1, None)
    }
}

impl Component for Text {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.cached_lines.is_some()
            && self.cached_text.as_deref() == Some(self.text.as_str())
            && self.cached_width == Some(width)
        {
            self.last_status = RenderStatus::Unchanged;
        } else {
            self.rebuild(width);
            self.last_status = RenderStatus::Changed;
        }
        self.cached_lines.as_deref().unwrap_or(&[])
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
    }

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_renders_no_lines_and_caches() {
        let mut t = Text::with_text("");
        let lines = t.render(40);
        assert!(lines.is_empty());
        assert_eq!(t.last_render_status(), RenderStatus::Changed);
        let _ = t.render(40);
        assert_eq!(t.last_render_status(), RenderStatus::Unchanged);
    }

    #[test]
    fn whitespace_only_is_empty() {
        let mut t = Text::with_text("   \t  ");
        assert!(t.render(20).is_empty());
    }

    #[test]
    fn cache_hit_unchanged_miss_on_width_or_set_text() {
        let mut t = Text::with_text("hello world");
        let a = t.render(40).len();
        assert_eq!(t.last_render_status(), RenderStatus::Changed);
        let b = t.render(40).len();
        assert_eq!(a, b);
        assert_eq!(t.last_render_status(), RenderStatus::Unchanged);

        let _ = t.render(20);
        assert_eq!(t.last_render_status(), RenderStatus::Changed);

        t.set_text("other");
        let _ = t.render(20);
        assert_eq!(t.last_render_status(), RenderStatus::Changed);
    }

    #[test]
    fn padding_defaults_add_vertical_empty_rows() {
        let mut t = Text::new("hi", 1, 1, None);
        let lines = t.render(20);
        // top pad + content + bottom pad
        assert!(lines.len() >= 3);
        assert!(
            lines[0].plain_text().chars().all(|c| c == ' ')
                || lines[0].is_empty()
                || lines[0].plain_text().trim().is_empty()
        );
    }
}

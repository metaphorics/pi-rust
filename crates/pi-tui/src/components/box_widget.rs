//! Box container — padding + optional background over children.
//!
//! Port of `packages/tui/src/components/box.ts`. Module is `box_widget` to avoid
//! the Rust `box` keyword.

use crate::component::{Component, ComponentBox, RenderStatus};
use crate::line::Line;
use crate::util::{apply_background_to_line, visible_width};

type BgFn = Box<dyn Fn(&str) -> String>;

struct RenderCache {
    child_lines: Vec<String>,
    width: u16,
    bg_sample: Option<String>,
    lines: Vec<Line>,
}

/// Container that pads children and applies a background function.
pub struct BoxWidget {
    children: Vec<ComponentBox>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<BgFn>,
    cache: Option<RenderCache>,
    last_status: RenderStatus,
}

impl BoxWidget {
    #[must_use]
    pub fn new(padding_x: usize, padding_y: usize, bg_fn: Option<BgFn>) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg_fn,
            cache: None,
            last_status: RenderStatus::Changed,
        }
    }

    /// Defaults: `padding_x = 1`, `padding_y = 1`, no background.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(1, 1, None)
    }

    pub fn add_child(&mut self, component: ComponentBox) {
        self.children.push(component);
        self.invalidate_cache();
    }

    pub fn remove_child_at(&mut self, index: usize) {
        if index < self.children.len() {
            self.children.remove(index);
            self.invalidate_cache();
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
        self.invalidate_cache();
    }

    pub fn set_bg_fn(&mut self, bg_fn: Option<BgFn>) {
        // Don't invalidate — detect bgFn changes by sampling output (TS).
        self.bg_fn = bg_fn;
    }

    pub fn children(&self) -> &[ComponentBox] {
        &self.children
    }

    pub fn children_mut(&mut self) -> &mut [ComponentBox] {
        &mut self.children
    }

    fn invalidate_cache(&mut self) {
        self.cache = None;
    }

    fn match_cache(&self, width: u16, child_lines: &[String], bg_sample: Option<&str>) -> bool {
        let Some(cache) = &self.cache else {
            return false;
        };
        cache.width == width
            && cache.bg_sample.as_deref() == bg_sample
            && cache.child_lines.len() == child_lines.len()
            && cache
                .child_lines
                .iter()
                .zip(child_lines.iter())
                .all(|(a, b)| a == b)
    }

    fn apply_bg(&self, line: &str, width: usize) -> String {
        let pad = width.saturating_sub(visible_width(line));
        let padded = format!("{line}{}", " ".repeat(pad));
        if let Some(bg) = &self.bg_fn {
            apply_background_to_line(&padded, width, bg.as_ref())
        } else {
            padded
        }
    }
}

impl Default for BoxWidget {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl Component for BoxWidget {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.children.is_empty() {
            self.cache = None;
            self.last_status = RenderStatus::Changed;
            // Stable empty slice via empty cache.
            self.cache = Some(RenderCache {
                child_lines: Vec::new(),
                width,
                bg_sample: None,
                lines: Vec::new(),
            });
            return &self.cache.as_ref().expect("just set").lines;
        }

        let content_width = (width as usize)
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1) as u16;
        let left_pad = " ".repeat(self.padding_x);

        let mut child_lines: Vec<String> = Vec::new();
        for child in &mut self.children {
            let lines = child.render(content_width);
            for line in lines {
                child_lines.push(format!("{left_pad}{}", line.to_ansi()));
            }
        }

        if child_lines.is_empty() {
            self.cache = Some(RenderCache {
                child_lines: Vec::new(),
                width,
                bg_sample: None,
                lines: Vec::new(),
            });
            self.last_status = RenderStatus::Changed;
            return &self.cache.as_ref().expect("just set").lines;
        }

        let bg_sample = self.bg_fn.as_ref().map(|bg| bg("test"));

        if self.match_cache(width, &child_lines, bg_sample.as_deref()) {
            self.last_status = RenderStatus::Unchanged;
            return &self.cache.as_ref().expect("matched").lines;
        }

        let mut result: Vec<Line> = Vec::new();
        for _ in 0..self.padding_y {
            result.push(Line::from_ansi(&self.apply_bg("", width as usize)));
        }
        for line in &child_lines {
            result.push(Line::from_ansi(&self.apply_bg(line, width as usize)));
        }
        for _ in 0..self.padding_y {
            result.push(Line::from_ansi(&self.apply_bg("", width as usize)));
        }

        self.cache = Some(RenderCache {
            child_lines,
            width,
            bg_sample,
            lines: result,
        });
        self.last_status = RenderStatus::Changed;
        &self.cache.as_ref().expect("just set").lines
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
        for child in &mut self.children {
            child.invalidate();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}

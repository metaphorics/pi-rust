//! Spacer — N empty lines.
//!
//! Port of `packages/tui/src/components/spacer.ts`.

use crate::component::{Component, RenderStatus};
use crate::line::Line;

/// Renders a fixed number of empty lines.
pub struct Spacer {
    lines: usize,
    cached: Vec<Line>,
    last_status: RenderStatus,
}

impl Spacer {
    #[must_use]
    pub fn new(lines: usize) -> Self {
        Self {
            lines,
            cached: Vec::new(),
            last_status: RenderStatus::Changed,
        }
    }

    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
    }

    pub fn lines(&self) -> usize {
        self.lines
    }
}

impl Default for Spacer {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Component for Spacer {
    fn render(&mut self, _width: u16) -> &[Line] {
        self.cached = (0..self.lines).map(|_| Line::empty()).collect();
        self.last_status = RenderStatus::Changed;
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    #[test]
    fn spacer_n_empty_lines() {
        let mut s = Spacer::new(3);
        let lines = s.render(10);
        assert_eq!(lines.len(), 3);
        assert!(
            lines
                .iter()
                .all(|l| l.is_empty() || l.plain_text().is_empty())
        );
    }
}

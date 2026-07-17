//! Two-state text block — port of the oracle `ExpandableText`
//! (interactive-mode.ts:172-191): a [`Text`] whose content is produced by a
//! collapsed or an expanded closure, switched by `set_expanded` (driven by
//! the tools-expand toggle / `--verbose`).

use pi_tui::components::Text;
use pi_tui::line::Line;
use pi_tui::{Component, RenderStatus};

type TextFn = Box<dyn Fn() -> String>;

pub struct ExpandableText {
    text: Text,
    get_collapsed_text: TextFn,
    get_expanded_text: TextFn,
}

impl ExpandableText {
    #[must_use]
    pub fn new(
        get_collapsed_text: TextFn,
        get_expanded_text: TextFn,
        expanded: bool,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        let initial = if expanded {
            get_expanded_text()
        } else {
            get_collapsed_text()
        };
        Self {
            text: Text::new(initial, padding_x, padding_y, None),
            get_collapsed_text,
            get_expanded_text,
        }
    }

    /// Oracle `setExpanded`: re-evaluates the matching closure (also the
    /// refresh path after a theme change).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.text.set_text(if expanded {
            (self.get_expanded_text)()
        } else {
            (self.get_collapsed_text)()
        });
    }
}

impl Component for ExpandableText {
    fn render(&mut self, width: u16) -> &[Line] {
        self.text.render(width)
    }

    fn invalidate(&mut self) {
        self.text.invalidate();
    }

    fn last_render_status(&self) -> RenderStatus {
        self.text.last_render_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_between_closures() {
        let mut text = ExpandableText::new(
            Box::new(|| "short".to_string()),
            Box::new(|| "long body".to_string()),
            false,
            0,
            0,
        );
        let lines = text.render(40);
        assert!(lines.iter().any(|l| l.plain_text().contains("short")));
        text.set_expanded(true);
        let lines = text.render(40);
        assert!(lines.iter().any(|l| l.plain_text().contains("long body")));
    }
}

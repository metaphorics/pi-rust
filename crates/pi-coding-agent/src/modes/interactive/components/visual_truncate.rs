//! Width-aware transcript output truncation.

use pi_tui::Component;
use pi_tui::components::text::Text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualTruncateResult {
    pub visual_lines: Vec<String>,
    pub skipped_count: usize,
}

/// Port of `truncateToVisualLines`.  Text owns ANSI-aware wrapping, so this does
/// not duplicate terminal width handling.
pub fn truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: u16,
    padding_x: usize,
) -> VisualTruncateResult {
    if text.is_empty() {
        return VisualTruncateResult {
            visual_lines: Vec::new(),
            skipped_count: 0,
        };
    }
    let mut component = Text::new(text, padding_x, 0, None);
    let lines = component.render(width);
    let rendered: Vec<String> = lines.iter().map(pi_tui::Line::to_ansi).collect();
    let skipped_count = rendered.len().saturating_sub(max_visual_lines);
    let start = rendered.len().saturating_sub(max_visual_lines);
    VisualTruncateResult {
        visual_lines: rendered[start..].to_vec(),
        skipped_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_the_last_visual_lines() {
        let result = truncate_to_visual_lines("one two three four", 2, 5, 0);
        assert_eq!(result.visual_lines.len(), 2);
        assert_eq!(result.skipped_count, 2);
    }
}

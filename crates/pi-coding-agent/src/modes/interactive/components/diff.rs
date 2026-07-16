//! Colorized unified-diff renderer.

use crate::modes::interactive::theme::{ThemeColor, theme};

fn parsed(line: &str) -> Option<(char, &str, &str)> {
    let mut chars = line.chars();
    let prefix = chars.next()?;
    if !matches!(prefix, '+' | '-' | ' ') {
        return None;
    }
    let tail = chars.as_str();
    let split = tail.find(char::is_whitespace)?;
    Some((
        prefix,
        &tail[..split],
        tail[split..].trim_start_matches(' '),
    ))
}
fn tabs(s: &str) -> String {
    s.replace('\t', "   ")
}
fn styled(prefix: char, number: &str, content: &str) -> String {
    let text = format!("{prefix}{number} {}", tabs(content));
    let color = match prefix {
        '+' => ThemeColor::ToolDiffAdded,
        '-' => ThemeColor::ToolDiffRemoved,
        _ => ThemeColor::ToolDiffContext,
    };
    theme().fg(color, &text)
}
/// Port of `renderDiff`. Rust intentionally does not include a second diff
/// engine: the shared tool diff generator produces the input; this only colors it.
pub fn render_diff(diff_text: &str) -> String {
    diff_text
        .lines()
        .map(|line| match parsed(line) {
            Some((prefix, number, content)) => styled(prefix, number, content),
            None => theme().fg(ThemeColor::ToolDiffContext, line),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_prefixes() {
        let output = render_diff("-1 old\n+1 new\n 2 same");
        assert!(output.contains("-1 old"));
        assert!(output.contains("+1 new"));
        assert!(output.contains(" 2 same"));
    }
}

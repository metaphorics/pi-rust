//! Styled line type used by the Component model.
//!
//! Internally a line is a sequence of inkferro-core [`StyledChar`] cells (SGR
//! style stacks as `Rc<[AnsiToken]>`). No raw ANSI strings are stored for
//! styled text. ANSI is parsed only at ingestion (`Line::from_ansi`) and
//! serialized only at the renderer boundary (`Line::to_ansi`).
//!
//! Image / control emissions are explicit boundary variants so they never
//! masquerade as SGR style state.

use std::rc::Rc;

use inkferro_core::text::ansi_tokenize::{
    AnsiToken, ControlToken, StyledChar, empty_styles, styled_chars_from_tokens,
    styled_chars_to_string, tokenize,
};

/// Zero-width control embedded in a line (cursor marker, non-link OSC, APC).
///
/// Position is a cell index: the control is emitted *before* that cell when
/// serializing (or at end-of-line when `index == cells.len()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedControl {
    pub index: usize,
    pub token: ControlToken,
}

/// One visual row.
///
/// - [`LineKind::Text`]: styled cells + optional zero-width controls.
/// - [`LineKind::Image`]: opaque kitty/iTerm2 graphics protocol payload
///   (never mixed with SGR cells; emission is passthrough).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    kind: LineKind,
    /// Styled grapheme cells (SGR only). Empty for a blank line or image lines.
    cells: Vec<StyledChar>,
    /// Zero-width controls (cursor marker, etc.) for Text lines.
    controls: Vec<EmbeddedControl>,
    /// Opaque protocol payload for Image lines.
    image_payload: Option<String>,
}

impl Default for Line {
    fn default() -> Self {
        Self::empty()
    }
}

impl Line {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            kind: LineKind::Text,
            cells: Vec::new(),
            controls: Vec::new(),
            image_payload: None,
        }
    }

    /// Plain (unstyled) text line.
    #[must_use]
    pub fn plain(text: impl AsRef<str>) -> Self {
        let text = text.as_ref();
        if text.is_empty() {
            return Self::empty();
        }
        // Fast path: no escape openers → plain styled chars with empty styles.
        if !text.contains(['\u{1b}', '\u{9b}']) {
            return Self {
                kind: LineKind::Text,
                cells: plain_cells(text),
                controls: Vec::new(),
                image_payload: None,
            };
        }
        Self::from_ansi(text)
    }

    /// Opaque image/protocol line (kitty / iTerm2). Emitted verbatim.
    #[must_use]
    pub fn image(payload: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Image,
            cells: Vec::new(),
            controls: Vec::new(),
            image_payload: Some(payload.into()),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self.kind {
            LineKind::Image => self
                .image_payload
                .as_ref()
                .map(|s| s.is_empty())
                .unwrap_or(true),
            LineKind::Text => self.cells.is_empty() && self.controls.is_empty(),
        }
    }

    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(self.kind, LineKind::Image)
    }

    #[must_use]
    pub fn cells(&self) -> &[StyledChar] {
        &self.cells
    }

    #[must_use]
    pub fn controls(&self) -> &[EmbeddedControl] {
        &self.controls
    }

    /// Visible text with styles stripped (controls ignored).
    #[must_use]
    pub fn plain_text(&self) -> String {
        match self.kind {
            LineKind::Image => String::new(),
            LineKind::Text => {
                let mut out = String::with_capacity(self.cells.len());
                for cell in &self.cells {
                    out.push_str(&cell.value);
                }
                out
            }
        }
    }

    /// Insert the hardware cursor marker before cell `col` (grapheme index).
    /// No-op for image lines.
    pub fn insert_cursor_marker(&mut self, col: usize) {
        if matches!(self.kind, LineKind::Image) {
            return;
        }
        let index = col.min(self.cells.len());
        self.controls.push(EmbeddedControl {
            index,
            token: ControlToken {
                code: CURSOR_MARKER.to_owned(),
            },
        });
    }

    /// Find and strip cursor marker; returns visual column (sum of cell widths
    /// before the marker) or None.
    pub fn extract_cursor_marker(&mut self) -> Option<usize> {
        let pos = self
            .controls
            .iter()
            .position(|c| c.token.code == CURSOR_MARKER)?;
        let index = self.controls[pos].index;
        self.controls.remove(pos);
        let mut prefix = String::new();
        for cell in self.cells.iter().take(index) {
            prefix.push_str(&cell.value);
        }
        Some(inkferro_core::text::string_width::string_width(&prefix))
    }

    /// Serialize for terminal / inkferro-rt. Only emission boundary that
    /// produces raw ANSI / protocol bytes.
    #[must_use]
    pub fn to_ansi(&self) -> String {
        match self.kind {
            LineKind::Image => self.image_payload.clone().unwrap_or_default(),
            LineKind::Text => {
                if self.controls.is_empty() {
                    return styled_chars_to_string(&self.cells);
                }
                // Interleave controls at their cell indices.
                let mut out = String::new();
                let mut ctrl_i = 0;
                // Sort-stable: controls are expected in index order; sort a view.
                let mut ordered = self.controls.clone();
                ordered.sort_by_key(|c| c.index);

                for (cell_idx, cell) in self.cells.iter().enumerate() {
                    while ctrl_i < ordered.len() && ordered[ctrl_i].index == cell_idx {
                        out.push_str(&ordered[ctrl_i].token.code);
                        ctrl_i += 1;
                    }
                    // Emit style + grapheme for this single cell via the shared helper.
                    // For simplicity, batch remaining run with same styles later if needed;
                    // single-cell path is correct.
                    let one = std::slice::from_ref(cell);
                    // Append only this cell's contribution by diffing from empty each time
                    // would re-open styles every cell — use styled_chars_to_string on the
                    // whole vector once, then re-insert controls by re-walk.
                    let _ = one;
                    let _ = cell_idx;
                }
                // Cleaner approach: serialize cells, then insert control codes by
                // re-tokenizing is lossy. Instead build from cells with control hooks.
                emit_text_with_controls(&self.cells, &ordered)
            }
        }
    }

    /// Parse an ANSI-bearing string into structured cells at an ingestion boundary.
    #[must_use]
    pub fn from_ansi(input: &str) -> Self {
        if input.is_empty() {
            return Self::empty();
        }
        if is_image_line(input) {
            return Self::image(input);
        }

        // Detect cursor marker and strip before tokenize so it becomes a control.
        let mut working = input.to_owned();
        let mut controls = Vec::new();
        while let Some(pos) = working.find(CURSOR_MARKER) {
            // Approximate cell index: count graphemes in prefix after stripping ANSI.
            let prefix = &working[..pos];
            let tokens = tokenize(prefix, None);
            let styled = styled_chars_from_tokens(&tokens);
            let index = styled.len();
            controls.push(EmbeddedControl {
                index,
                token: ControlToken {
                    code: CURSOR_MARKER.to_owned(),
                },
            });
            working.replace_range(pos..pos + CURSOR_MARKER.len(), "");
        }

        let tokens = tokenize(&working, None);
        // Collect non-link control tokens with their approximate positions.
        let mut cell_count = 0usize;
        for token in &tokens {
            match token {
                inkferro_core::text::ansi_tokenize::Token::Char(_) => cell_count += 1,
                inkferro_core::text::ansi_tokenize::Token::Control(c) => {
                    controls.push(EmbeddedControl {
                        index: cell_count,
                        token: c.clone(),
                    });
                }
                inkferro_core::text::ansi_tokenize::Token::Ansi(_) => {}
            }
        }

        let cells = styled_chars_from_tokens(&tokens);
        Self {
            kind: LineKind::Text,
            cells,
            controls,
            image_payload: None,
        }
    }

    /// Build from already-structured cells (widget internal construction).
    #[must_use]
    pub fn from_cells(cells: Vec<StyledChar>) -> Self {
        Self {
            kind: LineKind::Text,
            cells,
            controls: Vec::new(),
            image_payload: None,
        }
    }

    /// Apply a uniform SGR open stack to every cell (e.g. background fill).
    /// `styles` replaces existing styles when `replace` is true; otherwise merges
    /// by appending tokens then reducing is the caller's job — here we set.
    pub fn set_styles(&mut self, styles: Rc<[AnsiToken]>) {
        for cell in &mut self.cells {
            cell.styles = Rc::clone(&styles);
        }
    }

    /// Append plain text cells (empty styles).
    pub fn push_str(&mut self, text: &str) {
        if matches!(self.kind, LineKind::Image) || text.is_empty() {
            return;
        }
        self.cells.extend(plain_cells(text));
    }

    /// Append a single styled grapheme run sharing one style stack.
    pub fn push_styled(&mut self, text: &str, styles: Rc<[AnsiToken]>) {
        if matches!(self.kind, LineKind::Image) || text.is_empty() {
            return;
        }
        for cell in plain_cells(text) {
            self.cells.push(StyledChar {
                value: cell.value,
                full_width: cell.full_width,
                styles: Rc::clone(&styles),
            });
        }
    }

    /// Pad with spaces to at least `width` visible columns.
    pub fn pad_to_width(&mut self, width: usize) {
        if matches!(self.kind, LineKind::Image) {
            return;
        }
        let current = self.visible_width();
        if current >= width {
            return;
        }
        let pad = width - current;
        let space = StyledChar {
            value: " ".into(),
            full_width: false,
            styles: empty_styles(),
        };
        self.cells.extend(std::iter::repeat_n(space, pad));
    }

    /// Visible column width of cells (controls are zero-width).
    #[must_use]
    pub fn visible_width(&self) -> usize {
        match self.kind {
            LineKind::Image => 0,
            LineKind::Text => {
                let mut w = 0usize;
                for cell in &self.cells {
                    w += if cell.full_width {
                        2
                    } else {
                        // Tabs / controls shouldn't appear; fall back to string_width.
                        let sw = inkferro_core::text::string_width::string_width(&cell.value);
                        if sw == 0 { 0 } else { sw }
                    };
                }
                w
            }
        }
    }
}

impl From<&str> for Line {
    fn from(s: &str) -> Self {
        Self::from_ansi(s)
    }
}

impl From<String> for Line {
    fn from(s: String) -> Self {
        Self::from_ansi(&s)
    }
}

impl From<Line> for String {
    fn from(line: Line) -> Self {
        line.to_ansi()
    }
}

/// Convert a slice of Lines to ANSI strings for inkferro-rt.
#[must_use]
pub fn lines_to_ansi(lines: &[Line]) -> Vec<String> {
    lines.iter().map(Line::to_ansi).collect()
}

/// Convert ANSI strings (e.g. from fixtures) into Lines.
#[must_use]
pub fn lines_from_ansi(lines: &[impl AsRef<str>]) -> Vec<Line> {
    lines.iter().map(|l| Line::from_ansi(l.as_ref())).collect()
}

/// Zero-width cursor marker (APC). Components emit this at the cursor when focused.
pub const CURSOR_MARKER: &str = "\u{001B}_pi:c\u{0007}";

const KITTY_PREFIX: &str = "\u{001B}_G";
const ITERM2_PREFIX: &str = "\u{001B}]1337;File=";

fn is_image_line(line: &str) -> bool {
    line.starts_with(KITTY_PREFIX)
        || line.starts_with(ITERM2_PREFIX)
        || line.contains(KITTY_PREFIX)
        || line.contains(ITERM2_PREFIX)
}

fn plain_cells(text: &str) -> Vec<StyledChar> {
    use unicode_segmentation::UnicodeSegmentation;
    let empty = empty_styles();
    text.graphemes(true)
        .map(|g| {
            let w = inkferro_core::text::string_width::string_width(g);
            StyledChar {
                value: g.into(),
                full_width: w >= 2,
                styles: Rc::clone(&empty),
            }
        })
        .collect()
}

fn emit_text_with_controls(cells: &[StyledChar], controls: &[EmbeddedControl]) -> String {
    if cells.is_empty() && controls.is_empty() {
        return String::new();
    }
    if controls.is_empty() {
        return styled_chars_to_string(cells);
    }

    // Split cells into runs between control insertion points and serialize each
    // run, inserting control codes between runs.
    let mut out = String::new();
    let mut start = 0usize;
    let mut ctrl_i = 0usize;

    while ctrl_i < controls.len() || start < cells.len() {
        let next_ctrl_at = controls.get(ctrl_i).map(|c| c.index);
        let end = next_ctrl_at.unwrap_or(cells.len()).min(cells.len());

        if start < end {
            let slice = &cells[start..end];
            // Serialize this run independently (opens/closes its own styles).
            out.push_str(&styled_chars_to_string(slice));
            start = end;
        }

        if let Some(at) = next_ctrl_at {
            if at <= start || at >= cells.len() && start >= cells.len() {
                // Emit all controls at this index.
                while ctrl_i < controls.len() && controls[ctrl_i].index == at {
                    out.push_str(&controls[ctrl_i].token.code);
                    ctrl_i += 1;
                }
                if at >= cells.len() {
                    break;
                }
                // If at == start we already emitted; advance start only if at was
                // before any remaining cells — already handled.
                if at < cells.len() && start < at {
                    // unreachable given end = at above
                }
                if at == start {
                    // controls at current position already emitted; continue loop
                    continue;
                }
            } else {
                // end was set to at; after serializing start..at, emit controls
                while ctrl_i < controls.len() && controls[ctrl_i].index == at {
                    out.push_str(&controls[ctrl_i].token.code);
                    ctrl_i += 1;
                }
            }
        } else {
            break;
        }
    }

    // Trailing controls at end-of-line
    while ctrl_i < controls.len() {
        out.push_str(&controls[ctrl_i].token.code);
        ctrl_i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_roundtrip() {
        let line = Line::plain("hello");
        assert_eq!(line.plain_text(), "hello");
        assert_eq!(line.to_ansi(), "hello");
    }

    #[test]
    fn ansi_ingestion_preserves_text() {
        let line = Line::from_ansi("\x1b[31mred\x1b[0m");
        assert_eq!(line.plain_text(), "red");
        let out = line.to_ansi();
        assert!(out.contains("red"));
    }

    #[test]
    fn cursor_marker_roundtrip() {
        let mut line = Line::plain("ab");
        line.insert_cursor_marker(1);
        let ansi = line.to_ansi();
        assert!(ansi.contains(CURSOR_MARKER));
        let mut parsed = Line::from_ansi(&ansi);
        let col = parsed.extract_cursor_marker();
        assert_eq!(col, Some(1));
        assert_eq!(parsed.plain_text(), "ab");
    }

    #[test]
    fn image_line_passthrough() {
        let payload = format!("{KITTY_PREFIX}a=T,f=100;abc\x1b\\");
        let line = Line::from_ansi(&payload);
        assert!(line.is_image());
        assert_eq!(line.to_ansi(), payload);
    }
}

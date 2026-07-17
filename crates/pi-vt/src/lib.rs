//! pi-vt — cell-grid terminal screen emulator for tests.
//!
//! Rust replacement for pi's `@xterm/headless` conformance harness
//! (`inkferro/conformance/lib/screen-grid.mjs`, `helpers/vt-screen.ts`): feed
//! raw terminal bytes, assert on the FINAL SCREEN as a grid of styled cells —
//! never on raw write logs or retained-buffer internals.
//!
//! Parsing is [`vte`] (alacritty's parser); this crate only implements the
//! `Perform` grid semantics for the vocabulary pi's renderers emit
//! (inkferro-rt FrameWriter/LineDiff + pi-tui ProcessTerminal): print with
//! wide-char cells, CR/LF/BS/TAB, CUU/CUD/CUF/CUB/CNL/CPL/CHA/CUP, EL, ED
//! (with scrollback-erase 3J), SU/SD, SGR (16/256/truecolor), DECSET/DECRST
//! 25 (cursor), 2004 (bracketed paste), 2026 (synchronized update), 1049
//! (alt screen), OSC 0/2 title.
//!
//! # Flicker accounting
//!
//! The no-flicker global constraint is observable here: every screen-mutating
//! byte burst must land inside a `CSI ? 2026 h … CSI ? 2026 l` frame.
//! [`VtScreen::cells_mutated_outside_sync`] counts cell writes/clears that
//! happen while synchronized-update is inactive.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

/// Cell color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// SGR pen state / per-cell style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

/// One grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// `'\0'` marks the spacer cell behind a wide character.
    pub ch: char,
    pub style: Style,
}

impl Cell {
    const BLANK: Cell = Cell {
        ch: ' ',
        style: Style {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            strikethrough: false,
        },
    };
}

#[derive(Clone)]
struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<Cell>>,
}

impl Grid {
    fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![vec![Cell::BLANK; cols]; rows],
        }
    }
}

struct Screen {
    grid: Grid,
    scrollback: VecDeque<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    pending_wrap: bool,
    pen: Style,
    cursor_visible: bool,
    sync_active: bool,
    sync_frames_completed: usize,
    cells_mutated_outside_sync: usize,
    bracketed_paste: bool,
    alt_screen: bool,
    saved_main_grid: Option<Grid>,
    title: Option<String>,
    max_scrollback: usize,
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Self {
        Screen {
            grid: Grid::new(cols, rows),
            scrollback: VecDeque::new(),
            cursor_row: 0,
            cursor_col: 0,
            pending_wrap: false,
            pen: Style::default(),
            cursor_visible: true,
            sync_active: false,
            sync_frames_completed: 0,
            cells_mutated_outside_sync: 0,
            bracketed_paste: false,
            alt_screen: false,
            saved_main_grid: None,
            title: None,
            max_scrollback: 10_000,
        }
    }

    fn note_mutation(&mut self) {
        if !self.sync_active {
            self.cells_mutated_outside_sync += 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            let row = self.grid.cells.remove(0);
            if !self.alt_screen {
                self.scrollback.push_back(row);
                if self.scrollback.len() > self.max_scrollback {
                    self.scrollback.pop_front();
                }
            }
            self.grid.cells.push(vec![Cell::BLANK; self.grid.cols]);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            self.grid.cells.pop();
            self.grid.cells.insert(0, vec![Cell::BLANK; self.grid.cols]);
        }
    }

    fn linefeed(&mut self) {
        self.pending_wrap = false;
        if self.cursor_row + 1 >= self.grid.rows {
            self.scroll_up(1);
        } else {
            self.cursor_row += 1;
        }
    }

    fn put_char(&mut self, ch: char) {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            // Combining mark: append to the previous cell's char is out of
            // scope for pi's renderers (they pre-compose); ignore.
            return;
        }
        if self.pending_wrap {
            self.pending_wrap = false;
            self.cursor_col = 0;
            self.linefeed();
        }
        if width == 2 && self.cursor_col + 1 >= self.grid.cols {
            // Wide char at last column: wrap first.
            self.cursor_col = 0;
            self.linefeed();
        }
        self.note_mutation();
        let row = self.cursor_row;
        let col = self.cursor_col;
        self.grid.cells[row][col] = Cell {
            ch,
            style: self.pen,
        };
        if width == 2 {
            self.grid.cells[row][col + 1] = Cell {
                ch: '\0',
                style: self.pen,
            };
        }
        self.cursor_col += width;
        if self.cursor_col >= self.grid.cols {
            self.cursor_col = self.grid.cols - 1;
            self.pending_wrap = true;
        }
    }

    fn erase_line(&mut self, mode: u16) {
        self.note_mutation();
        let row = &mut self.grid.cells[self.cursor_row];
        let range = match mode {
            0 => self.cursor_col..self.grid.cols,
            1 => 0..(self.cursor_col + 1).min(self.grid.cols),
            _ => 0..self.grid.cols,
        };
        for cell in &mut row[range] {
            *cell = Cell {
                ch: ' ',
                style: Style {
                    bg: self.pen.bg,
                    ..Style::default()
                },
            };
        }
    }

    fn erase_display(&mut self, mode: u16) {
        self.note_mutation();
        match mode {
            0 => {
                self.erase_line(0);
                for r in (self.cursor_row + 1)..self.grid.rows {
                    self.grid.cells[r] = vec![Cell::BLANK; self.grid.cols];
                }
            }
            1 => {
                self.erase_line(1);
                for r in 0..self.cursor_row {
                    self.grid.cells[r] = vec![Cell::BLANK; self.grid.cols];
                }
            }
            2 => {
                self.grid.cells = vec![vec![Cell::BLANK; self.grid.cols]; self.grid.rows];
            }
            3 => {
                self.scrollback.clear();
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        let flat: Vec<Vec<u16>> = params.iter().map(<[u16]>::to_vec).collect();
        if flat.is_empty() {
            self.pen = Style::default();
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            let sub = &flat[i];
            let code = sub.first().copied().unwrap_or(0);
            match code {
                0 => self.pen = Style::default(),
                1 => self.pen.bold = true,
                2 => self.pen.dim = true,
                3 => self.pen.italic = true,
                4 => self.pen.underline = true,
                7 => self.pen.inverse = true,
                9 => self.pen.strikethrough = true,
                22 => {
                    self.pen.bold = false;
                    self.pen.dim = false;
                }
                23 => self.pen.italic = false,
                24 => self.pen.underline = false,
                27 => self.pen.inverse = false,
                29 => self.pen.strikethrough = false,
                30..=37 => self.pen.fg = Color::Indexed((code - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((code - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    // Extended color: either colon subparams (38:2::r:g:b) in
                    // one item, or semicolon params spread across items.
                    let (color, consumed) = if sub.len() > 1 {
                        (parse_extended_color(&sub[1..]), 0)
                    } else {
                        let rest: Vec<u16> = flat[i + 1..]
                            .iter()
                            .map(|s| s.first().copied().unwrap_or(0))
                            .collect();
                        let (c, used) = parse_extended_color_counted(&rest);
                        (c, used)
                    };
                    if let Some(color) = color {
                        if code == 38 {
                            self.pen.fg = color;
                        } else {
                            self.pen.bg = color;
                        }
                    }
                    i += consumed;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn parse_extended_color(rest: &[u16]) -> Option<Color> {
    parse_extended_color_counted(rest).0
}

fn parse_extended_color_counted(rest: &[u16]) -> (Option<Color>, usize) {
    match rest.first() {
        Some(5) => {
            let idx = rest.get(1).copied().unwrap_or(0);
            (Some(Color::Indexed(idx as u8)), 2)
        }
        Some(2) => {
            // Skip optional colorspace id when colon-form carries an empty slot;
            // vte gives 0 for empty subparams. The renderer emits `2;r;g;b`.
            let (r, g, b) = (
                rest.get(1).copied().unwrap_or(0),
                rest.get(2).copied().unwrap_or(0),
                rest.get(3).copied().unwrap_or(0),
            );
            (Some(Color::Rgb(r as u8, g as u8, b as u8)), 4)
        }
        _ => (None, 0),
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.linefeed(),
            b'\r' => {
                self.cursor_col = 0;
                self.pending_wrap = false;
            }
            0x08 => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
                self.pending_wrap = false;
            }
            b'\t' => {
                let next = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next.min(self.grid.cols - 1);
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let first = params.iter().next().and_then(|p| p.first().copied());
        let n = first.filter(|&v| v != 0).unwrap_or(1) as usize;
        let private = intermediates.first() == Some(&b'?');
        match action {
            'A' => {
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.pending_wrap = false;
            }
            'B' => {
                self.cursor_row = (self.cursor_row + n).min(self.grid.rows - 1);
                self.pending_wrap = false;
            }
            'C' => {
                self.cursor_col = (self.cursor_col + n).min(self.grid.cols - 1);
                self.pending_wrap = false;
            }
            'D' => {
                self.cursor_col = self.cursor_col.saturating_sub(n);
                self.pending_wrap = false;
            }
            'E' => {
                // CNL: cursor to column 1 of the n-th following line (scrolls).
                self.cursor_col = 0;
                for _ in 0..n {
                    self.linefeed();
                }
            }
            'F' => {
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.cursor_col = 0;
                self.pending_wrap = false;
            }
            'G' => {
                self.cursor_col = (n - 1).min(self.grid.cols - 1);
                self.pending_wrap = false;
            }
            'H' | 'f' => {
                let mut it = params.iter();
                let row = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .filter(|&v| v != 0)
                    .unwrap_or(1) as usize;
                let col = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .filter(|&v| v != 0)
                    .unwrap_or(1) as usize;
                self.cursor_row = (row - 1).min(self.grid.rows - 1);
                self.cursor_col = (col - 1).min(self.grid.cols - 1);
                self.pending_wrap = false;
            }
            'J' => self.erase_display(first.unwrap_or(0)),
            'K' => self.erase_line(first.unwrap_or(0)),
            'S' => self.scroll_up(n),
            'T' => self.scroll_down(n),
            'm' => {
                if !private {
                    self.apply_sgr(params);
                }
            }
            'h' | 'l' if private => {
                let set = action == 'h';
                for param in params.iter() {
                    match param.first().copied().unwrap_or(0) {
                        25 => self.cursor_visible = set,
                        2004 => self.bracketed_paste = set,
                        2026 => {
                            if set {
                                self.sync_active = true;
                            } else {
                                if self.sync_active {
                                    self.sync_frames_completed += 1;
                                }
                                self.sync_active = false;
                            }
                        }
                        1049 => {
                            if set && !self.alt_screen {
                                self.alt_screen = true;
                                self.saved_main_grid = Some(self.grid.clone());
                                self.grid = Grid::new(self.grid.cols, self.grid.rows);
                                self.cursor_row = 0;
                                self.cursor_col = 0;
                            } else if !set && self.alt_screen {
                                self.alt_screen = false;
                                if let Some(main) = self.saved_main_grid.take() {
                                    self.grid = main;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let [code, rest @ ..] = params
            && (*code == b"0" || *code == b"2")
            && let Some(title) = rest.first()
        {
            self.title = Some(String::from_utf8_lossy(title).into_owned());
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

/// Cell-grid screen emulator. Feed bytes; assert on the grid.
pub struct VtScreen {
    parser: Parser,
    screen: Screen,
}

impl VtScreen {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        assert!(cols > 0 && rows > 0, "screen dimensions must be non-zero");
        VtScreen {
            parser: Parser::new(),
            screen: Screen::new(usize::from(cols), usize::from(rows)),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    pub fn feed_str(&mut self, s: &str) {
        self.feed(s.as_bytes());
    }

    /// Resize the viewport (SIGWINCH analog). Content is clipped/padded like a
    /// simple terminal; rows scrolled off the top on shrink go to scrollback.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        assert!(cols > 0 && rows > 0, "screen dimensions must be non-zero");
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        let s = &mut self.screen;
        for row in &mut s.grid.cells {
            row.resize(cols, Cell::BLANK);
        }
        while s.grid.cells.len() > rows {
            // xterm-like shrink: drop trailing blank rows below the cursor
            // first, then scroll top rows into scrollback.
            let last_is_blank = s
                .grid
                .cells
                .last()
                .is_some_and(|r| r.iter().all(|c| c.ch == ' ' || c.ch == '\0'));
            if last_is_blank && s.cursor_row < s.grid.cells.len() - 1 {
                s.grid.cells.pop();
            } else if s.cursor_row > 0 {
                let top = s.grid.cells.remove(0);
                if !s.alt_screen {
                    s.scrollback.push_back(top);
                }
                s.cursor_row -= 1;
            } else {
                s.grid.cells.pop();
            }
        }
        while s.grid.cells.len() < rows {
            s.grid.cells.push(vec![Cell::BLANK; cols]);
        }
        s.grid.cols = cols;
        s.grid.rows = rows;
        s.cursor_col = s.cursor_col.min(cols - 1);
        s.cursor_row = s.cursor_row.min(rows - 1);
        s.pending_wrap = false;
    }

    #[must_use]
    pub fn dimensions(&self) -> (u16, u16) {
        (self.screen.grid.cols as u16, self.screen.grid.rows as u16)
    }

    /// Text of one viewport row (0-based), trailing whitespace trimmed, wide
    /// spacer cells skipped.
    #[must_use]
    pub fn row_text(&self, row: usize) -> String {
        let Some(cells) = self.screen.grid.cells.get(row) else {
            return String::new();
        };
        Self::cells_to_string(cells)
    }

    fn cells_to_string(cells: &[Cell]) -> String {
        let mut out = String::new();
        for cell in cells {
            if cell.ch != '\0' {
                out.push(cell.ch);
            }
        }
        out.trim_end().to_owned()
    }

    /// All viewport rows as trimmed text.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        (0..self.screen.grid.rows)
            .map(|r| self.row_text(r))
            .collect()
    }

    /// Scrollback rows (oldest first) as trimmed text.
    #[must_use]
    pub fn scrollback(&self) -> Vec<String> {
        self.screen
            .scrollback
            .iter()
            .map(|r| Self::cells_to_string(r))
            .collect()
    }

    /// Scrollback + viewport rows.
    #[must_use]
    pub fn all_rows(&self) -> Vec<String> {
        let mut out = self.scrollback();
        out.extend(self.rows());
        out
    }

    /// Number of viewport rows up to and including the last non-blank row.
    #[must_use]
    pub fn used_height(&self) -> usize {
        let rows = self.rows();
        rows.iter()
            .rposition(|r| !r.is_empty())
            .map_or(0, |i| i + 1)
    }

    /// Full screen as newline-joined trimmed rows, trailing blank rows dropped
    /// (mirrors `screen-grid.mjs` normalization).
    #[must_use]
    pub fn serialize(&self) -> String {
        let mut rows = self.rows();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// Styled cell at (col, row) in the viewport.
    #[must_use]
    pub fn cell(&self, col: usize, row: usize) -> Option<Cell> {
        self.screen.grid.cells.get(row)?.get(col).copied()
    }

    /// (col, row) cursor position.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.screen.cursor_col, self.screen.cursor_row)
    }

    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        self.screen.cursor_visible
    }

    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.screen.bracketed_paste
    }

    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.screen.alt_screen
    }

    /// Window title from OSC 0/2, if any.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.screen.title.as_deref()
    }

    /// Completed `CSI ?2026h … CSI ?2026l` frames.
    #[must_use]
    pub fn sync_frames_completed(&self) -> usize {
        self.screen.sync_frames_completed
    }

    /// Whether a synchronized update is currently open.
    #[must_use]
    pub fn in_sync_update(&self) -> bool {
        self.screen.sync_active
    }

    /// Cell mutations (prints/erases) performed OUTSIDE synchronized-update
    /// framing — the flicker counter. Frame-emitting renderers must keep this
    /// at zero after start-up.
    #[must_use]
    pub fn cells_mutated_outside_sync(&self) -> usize {
        self.screen.cells_mutated_outside_sync
    }

    /// True if any viewport row contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.rows().iter().any(|r| r.contains(needle))
    }

    /// Index of the first viewport row containing `needle`.
    #[must_use]
    pub fn find_row(&self, needle: &str) -> Option<usize> {
        self.rows().iter().position(|r| r.contains(needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_and_wrap() {
        let mut vt = VtScreen::new(10, 3);
        vt.feed_str("hello worldX");
        assert_eq!(vt.row_text(0), "hello worl");
        assert_eq!(vt.row_text(1), "dX");
    }

    #[test]
    fn crlf_and_scroll() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed_str("a\r\nb\r\nc");
        assert_eq!(vt.rows(), vec!["b", "c"]);
        assert_eq!(vt.scrollback(), vec!["a"]);
        assert_eq!(vt.all_rows(), vec!["a", "b", "c"]);
    }

    #[test]
    fn cursor_moves_and_erase() {
        let mut vt = VtScreen::new(10, 3);
        vt.feed_str("aaaa\r\nbbbb\r\ncccc");
        vt.feed_str("\x1b[2A\x1b[1G\x1b[K"); // up 2, col 1, erase line
        assert_eq!(vt.rows(), vec!["", "bbbb", "cccc"]);
        vt.feed_str("\x1b[2;3H\x1b[0K"); // row 2 col 3, erase to end
        assert_eq!(vt.row_text(1), "bb");
    }

    #[test]
    fn erase_display_and_scrollback() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed_str("a\r\nb\r\nc");
        vt.feed_str("\x1b[2J\x1b[H");
        assert_eq!(vt.serialize(), "");
        assert_eq!(vt.scrollback().len(), 1);
        vt.feed_str("\x1b[3J");
        assert!(vt.scrollback().is_empty());
    }

    #[test]
    fn sgr_truecolor_and_256() {
        let mut vt = VtScreen::new(20, 2);
        vt.feed_str("\x1b[1;38;2;10;20;30mA\x1b[0m\x1b[38;5;196mB\x1b[39mC");
        let a = vt.cell(0, 0).unwrap();
        assert!(a.style.bold);
        assert_eq!(a.style.fg, Color::Rgb(10, 20, 30));
        let b = vt.cell(1, 0).unwrap();
        assert_eq!(b.style.fg, Color::Indexed(196));
        assert!(!b.style.bold);
        let c = vt.cell(2, 0).unwrap();
        assert_eq!(c.style.fg, Color::Default);
    }

    #[test]
    fn sgr_fg_bg_reset_independent() {
        let mut vt = VtScreen::new(20, 1);
        vt.feed_str("\x1b[38;2;1;2;3m\x1b[48;5;100mX\x1b[49mY\x1b[39mZ");
        assert_eq!(vt.cell(0, 0).unwrap().style.bg, Color::Indexed(100));
        let y = vt.cell(1, 0).unwrap().style;
        assert_eq!(y.bg, Color::Default);
        assert_eq!(y.fg, Color::Rgb(1, 2, 3));
        assert_eq!(vt.cell(2, 0).unwrap().style.fg, Color::Default);
    }

    #[test]
    fn wide_chars_occupy_two_cells() {
        let mut vt = VtScreen::new(6, 1);
        vt.feed_str("你好");
        assert_eq!(vt.row_text(0), "你好");
        assert_eq!(vt.cell(1, 0).unwrap().ch, '\0');
        assert_eq!(vt.cursor(), (4, 0));
    }

    #[test]
    fn sync_update_accounting() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed_str("x"); // outside sync
        assert_eq!(vt.cells_mutated_outside_sync(), 1);
        vt.feed_str("\x1b[?2026h frame \x1b[?2026l");
        assert_eq!(vt.sync_frames_completed(), 1);
        assert_eq!(vt.cells_mutated_outside_sync(), 1);
        assert!(!vt.in_sync_update());
    }

    #[test]
    fn cursor_visibility_and_title() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed_str("\x1b[?25l");
        assert!(!vt.cursor_visible());
        vt.feed_str("\x1b[?25h");
        assert!(vt.cursor_visible());
        vt.feed_str("\x1b]0;pi session\x07");
        assert_eq!(vt.title(), Some("pi session"));
    }

    #[test]
    fn cnl_scrolls_at_bottom() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed_str("a\r\nb");
        vt.feed_str("\x1b[E"); // CNL at bottom row scrolls
        vt.feed_str("c");
        assert_eq!(vt.rows(), vec!["b", "c"]);
        assert_eq!(vt.scrollback(), vec!["a"]);
    }

    #[test]
    fn alt_screen_saves_and_restores() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed_str("main");
        vt.feed_str("\x1b[?1049h");
        assert!(vt.alt_screen());
        vt.feed_str("alt");
        assert_eq!(vt.row_text(0), "alt");
        vt.feed_str("\x1b[?1049l");
        assert_eq!(vt.row_text(0), "main");
    }

    #[test]
    fn resize_clips_and_pads() {
        let mut vt = VtScreen::new(10, 4);
        vt.feed_str("aaaa\r\nbbbb\r\ncccc");
        vt.resize(6, 2);
        assert_eq!(vt.dimensions(), (6, 2));
        assert_eq!(vt.rows(), vec!["bbbb", "cccc"]);
        assert_eq!(vt.scrollback(), vec!["aaaa"]);
        vt.resize(6, 3);
        assert_eq!(vt.rows().len(), 3);
    }

    #[test]
    fn chunk_boundary_safe() {
        let mut vt = VtScreen::new(10, 2);
        // split an SGR sequence across feeds
        vt.feed_str("\x1b[38;2;9");
        vt.feed_str(";8;7mZ");
        assert_eq!(vt.cell(0, 0).unwrap().style.fg, Color::Rgb(9, 8, 7));
        assert_eq!(vt.row_text(0), "Z");
    }
}

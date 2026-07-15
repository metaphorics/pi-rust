//! Markdown component — port of `packages/tui/src/components/markdown.ts`.
//!
//! Parses with `pulldown-cmark` and applies theme style functions for
//! heading/link/code/quote/hr/list/bold/italic (plus strikethrough/underline).
//! Caches by `(width, content)`; returns [`RenderStatus::Unchanged`] on hit.

use std::sync::{Arc, LazyLock};

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{LinesWithEndings, as_24_bit_terminal_escaped};

use crate::component::{Component, RenderStatus};
use crate::line::Line;
use crate::terminal_image::{get_capabilities, hyperlink, is_image_line};
use crate::util::{apply_background_to_line, visible_width, wrap_text_with_ansi};

/// Style function: plain text → ANSI-styled text.
pub type StyleFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Optional code highlighter: `(code, lang) → styled lines`.
pub type HighlightCodeFn = Arc<dyn Fn(&str, Option<&str>) -> Vec<String> + Send + Sync>;

/// Default text styling for markdown content (foreground / decorations).
/// Background is applied at the padding stage so it spans full line width.
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    pub color: Option<StyleFn>,
    pub bg_color: Option<StyleFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

/// Theme functions for markdown elements.
#[derive(Clone)]
pub struct MarkdownTheme {
    pub heading: StyleFn,
    pub link: StyleFn,
    pub link_url: StyleFn,
    pub code: StyleFn,
    pub code_block: StyleFn,
    pub code_block_border: StyleFn,
    pub quote: StyleFn,
    pub quote_border: StyleFn,
    pub hr: StyleFn,
    pub list_bullet: StyleFn,
    pub bold: StyleFn,
    pub italic: StyleFn,
    pub strikethrough: StyleFn,
    pub underline: StyleFn,
    pub highlight_code: Option<HighlightCodeFn>,
    /// Prefix applied to each rendered code block line (default: `"  "`).
    pub code_block_indent: String,
}

fn ansi_wrap(open: &str, close: &str) -> StyleFn {
    let open = open.to_owned();
    let close = close.to_owned();
    Arc::new(move |text: &str| format!("{open}{text}{close}"))
}

/// Default theme (ANSI equivalents of the pi test theme).
#[must_use]
pub fn default_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        // bold cyan
        heading: ansi_wrap("\x1b[1;36m", "\x1b[22;39m"),
        // blue
        link: ansi_wrap("\x1b[34m", "\x1b[39m"),
        // dim
        link_url: ansi_wrap("\x1b[2m", "\x1b[22m"),
        // yellow
        code: ansi_wrap("\x1b[33m", "\x1b[39m"),
        // green
        code_block: ansi_wrap("\x1b[32m", "\x1b[39m"),
        code_block_border: ansi_wrap("\x1b[2m", "\x1b[22m"),
        // italic
        quote: ansi_wrap("\x1b[3m", "\x1b[23m"),
        quote_border: ansi_wrap("\x1b[2m", "\x1b[22m"),
        hr: ansi_wrap("\x1b[2m", "\x1b[22m"),
        // cyan
        list_bullet: ansi_wrap("\x1b[36m", "\x1b[39m"),
        bold: ansi_wrap("\x1b[1m", "\x1b[22m"),
        italic: ansi_wrap("\x1b[3m", "\x1b[23m"),
        strikethrough: ansi_wrap("\x1b[9m", "\x1b[29m"),
        underline: ansi_wrap("\x1b[4m", "\x1b[24m"),
        highlight_code: None,
        code_block_indent: "  ".to_owned(),
    }
}

/// Optional syntect-backed highlighter (lazy syntax/theme sets).
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Build a syntect highlighter for use as [`MarkdownTheme::highlight_code`].
#[must_use]
pub fn syntect_highlight_code() -> HighlightCodeFn {
    Arc::new(|code: &str, lang: Option<&str>| {
        let ss = &*SYNTAX_SET;
        let ts = &*THEME_SET;
        let syntax = lang
            .and_then(|l| ss.find_syntax_by_token(l))
            .unwrap_or_else(|| ss.find_syntax_plain_text());
        let theme = ts
            .themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .expect("syntect ships at least one theme");
        let mut h = HighlightLines::new(syntax, theme);
        let mut out = Vec::new();
        for line in LinesWithEndings::from(code) {
            let ranges: Vec<(Style, &str)> = h.highlight_line(line, ss).unwrap_or_default();
            let mut escaped = as_24_bit_terminal_escaped(&ranges[..], false);
            // strip trailing newline kept by LinesWithEndings
            if escaped.ends_with('\n') {
                escaped.pop();
                if escaped.ends_with('\r') {
                    escaped.pop();
                }
            }
            out.push(escaped);
        }
        if out.is_empty() {
            out.push(String::new());
        }
        out
    })
}

/// Markdown parse/render options.
#[derive(Debug, Clone, Default)]
pub struct MarkdownOptions {
    /// Preserve source ordered-list markers instead of renumbering.
    pub preserve_ordered_list_markers: bool,
    /// Preserve source backslash escapes instead of normalizing.
    pub preserve_backslash_escapes: bool,
}

struct InlineStyleContext {
    apply_text: StyleFn,
    style_prefix: String,
}

/// Markdown widget with theme, padding, and width/content cache.
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    default_text_style: Option<DefaultTextStyle>,
    theme: MarkdownTheme,
    options: MarkdownOptions,
    default_style_prefix: Option<String>,
    cached_text: Option<String>,
    cached_width: Option<u16>,
    cached_lines: Option<Vec<Line>>,
    last_status: RenderStatus,
}

impl Markdown {
    #[must_use]
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: Option<MarkdownOptions>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            default_text_style,
            theme,
            options: options.unwrap_or_default(),
            default_style_prefix: None,
            cached_text: None,
            cached_width: None,
            cached_lines: None,
            last_status: RenderStatus::Changed,
        }
    }

    /// Convenience: padding 1/1, default theme.
    #[must_use]
    pub fn with_text(text: impl Into<String>) -> Self {
        Self::new(text, 1, 1, default_markdown_theme(), None, None)
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.invalidate_cache();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_theme(&mut self, theme: MarkdownTheme) {
        self.theme = theme;
        self.default_style_prefix = None;
        self.invalidate_cache();
    }

    fn invalidate_cache(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }

    fn apply_default_style(&self, text: &str) -> String {
        let Some(style) = &self.default_text_style else {
            return text.to_owned();
        };
        let mut styled = text.to_owned();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    fn get_style_prefix(style_fn: &StyleFn) -> String {
        let sentinel = '\u{0000}';
        let styled = style_fn(&sentinel.to_string());
        match styled.find(sentinel) {
            Some(i) => styled[..i].to_owned(),
            None => String::new(),
        }
    }

    fn get_default_style_prefix(&mut self) -> String {
        if self.default_text_style.is_none() {
            return String::new();
        }
        if let Some(p) = &self.default_style_prefix {
            return p.clone();
        }
        let sentinel = "\u{0000}";
        let mut styled = sentinel.to_owned();
        if let Some(style) = &self.default_text_style {
            if let Some(color) = &style.color {
                styled = color(&styled);
            }
            if style.bold {
                styled = (self.theme.bold)(&styled);
            }
            if style.italic {
                styled = (self.theme.italic)(&styled);
            }
            if style.strikethrough {
                styled = (self.theme.strikethrough)(&styled);
            }
            if style.underline {
                styled = (self.theme.underline)(&styled);
            }
        }
        let prefix = match styled.find('\u{0000}') {
            Some(i) => styled[..i].to_owned(),
            None => String::new(),
        };
        self.default_style_prefix = Some(prefix.clone());
        prefix
    }

    fn default_inline_ctx(&mut self) -> InlineStyleContext {
        let style_prefix = self.get_default_style_prefix();
        let theme_bold = self.theme.bold.clone();
        let theme_italic = self.theme.italic.clone();
        let theme_strike = self.theme.strikethrough.clone();
        let theme_underline = self.theme.underline.clone();
        let color = self
            .default_text_style
            .as_ref()
            .and_then(|s| s.color.clone());
        let bold = self
            .default_text_style
            .as_ref()
            .map(|s| s.bold)
            .unwrap_or(false);
        let italic = self
            .default_text_style
            .as_ref()
            .map(|s| s.italic)
            .unwrap_or(false);
        let strike = self
            .default_text_style
            .as_ref()
            .map(|s| s.strikethrough)
            .unwrap_or(false);
        let underline = self
            .default_text_style
            .as_ref()
            .map(|s| s.underline)
            .unwrap_or(false);
        let apply_text: StyleFn = Arc::new(move |text: &str| {
            let mut styled = text.to_owned();
            if let Some(c) = &color {
                styled = c(&styled);
            }
            if bold {
                styled = theme_bold(&styled);
            }
            if italic {
                styled = theme_italic(&styled);
            }
            if strike {
                styled = theme_strike(&styled);
            }
            if underline {
                styled = theme_underline(&styled);
            }
            styled
        });
        InlineStyleContext {
            apply_text,
            style_prefix,
        }
    }

    fn rebuild(&mut self, width: u16) {
        if self.text.trim().is_empty() {
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            self.cached_lines = Some(Vec::new());
            self.last_status = RenderStatus::Changed;
            return;
        }

        let content_width = (width as usize)
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let normalized = self.text.replace('\t', "   ");

        let rendered = self.render_document(&normalized, content_width);

        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in rendered {
            if is_image_line(&line) {
                wrapped_lines.push(line);
            } else {
                for w in wrap_text_with_ansi(&line, content_width) {
                    wrapped_lines.push(w);
                }
            }
        }

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let bg_fn = self
            .default_text_style
            .as_ref()
            .and_then(|s| s.bg_color.clone());

        let mut content_lines: Vec<String> = Vec::with_capacity(wrapped_lines.len());
        for line in wrapped_lines {
            if is_image_line(&line) {
                content_lines.push(line);
                continue;
            }
            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            if let Some(bg) = &bg_fn {
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
        let empty_line = if let Some(bg) = &bg_fn {
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

    fn parser_options() -> Options {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_TABLES);
        opts
    }

    fn render_document(&mut self, text: &str, width: usize) -> Vec<String> {
        let events: Vec<Event<'_>> = Parser::new_ext(text, Self::parser_options()).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < events.len() {
            let (lines, consumed) = self.render_block(&events, i, width, None);
            out.extend(lines);
            i += consumed.max(1);
        }
        // Drop trailing blank that block spacing may add at EOF (TS keeps space tokens).
        while out.last().is_some_and(|l| l.is_empty()) {
            // Keep a single trailing blank only if source had trailing blank lines via space tokens.
            // Mimic TS: space tokens push ""; block spacing also pushes "" before next block.
            // Leaving trailing blanks is fine for layout; strip only pure trailing empties from
            // our synthetic "next != space" spacing when no more content.
            out.pop();
        }
        out
    }

    /// Render one top-level (or nested) block starting at `start`. Returns lines + events consumed.
    fn render_block(
        &mut self,
        events: &[Event<'_>],
        start: usize,
        width: usize,
        style_ctx: Option<&InlineStyleContext>,
    ) -> (Vec<String>, usize) {
        if start >= events.len() {
            return (Vec::new(), 0);
        }

        match &events[start] {
            Event::Start(Tag::Heading { level, .. }) => {
                let level_n = *level as usize;
                let end = find_end(events, start, TagEnd::Heading(*level));
                let inner = &events[start + 1..end];
                let heading_style = self.make_heading_style(level_n);
                let heading_ctx = InlineStyleContext {
                    style_prefix: Self::get_style_prefix(&heading_style),
                    apply_text: heading_style.clone(),
                };
                let heading_text = self.render_inlines(inner, &heading_ctx);
                let heading_prefix = format!("{} ", "#".repeat(level_n));
                let styled = if level_n >= 3 {
                    format!("{}{heading_text}", heading_style(&heading_prefix))
                } else {
                    heading_text
                };
                let mut lines = vec![styled];
                if self.next_block_needs_spacing(events, end) {
                    lines.push(String::new());
                }
                (lines, end - start + 1)
            }
            Event::Start(Tag::Paragraph) => {
                let end = find_end(events, start, TagEnd::Paragraph);
                let inner = &events[start + 1..end];
                let ctx;
                let ctx_ref = if let Some(c) = style_ctx {
                    c
                } else {
                    ctx = self.default_inline_ctx();
                    &ctx
                };
                let text = self.render_inlines(inner, ctx_ref);
                let mut lines = vec![text];
                // Don't add spacing if next is list or soft blank (we approximate space as Rule/blank).
                if self.next_block_needs_para_spacing(events, end) {
                    lines.push(String::new());
                }
                (lines, end - start + 1)
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.as_ref().to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                let end = find_end(events, start, TagEnd::CodeBlock);
                let mut code = String::new();
                for e in &events[start + 1..end] {
                    if let Event::Text(t) = e {
                        code.push_str(t);
                    }
                }
                // Trim trailing newline that fenced blocks include.
                if code.ends_with('\n') {
                    code.pop();
                }
                let indent = self.theme.code_block_indent.clone();
                let mut lines = Vec::new();
                lines.push((self.theme.code_block_border)(&format!("```{lang}")));
                if let Some(hl) = &self.theme.highlight_code {
                    for hl_line in hl(
                        &code,
                        if lang.is_empty() {
                            None
                        } else {
                            Some(lang.as_str())
                        },
                    ) {
                        lines.push(format!("{indent}{hl_line}"));
                    }
                } else {
                    for code_line in code.split('\n') {
                        lines.push(format!("{indent}{}", (self.theme.code_block)(code_line)));
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                if self.next_block_needs_spacing(events, end) {
                    lines.push(String::new());
                }
                (lines, end - start + 1)
            }
            Event::Start(Tag::List(start_num)) => {
                let ordered = start_num.is_some();
                let start_number = start_num.unwrap_or(1);
                let end = find_end(events, start, TagEnd::List(ordered));
                let lines = self.render_list(
                    &events[start + 1..end],
                    0,
                    width,
                    ordered,
                    start_number,
                    style_ctx,
                );
                (lines, end - start + 1)
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let end = find_end(events, start, TagEnd::BlockQuote(None));
                // TagEnd::BlockQuote carries Option; match any via scan.
                let end = find_blockquote_end(events, start).unwrap_or(end);
                let quote_style = {
                    let q = self.theme.quote.clone();
                    let it = self.theme.italic.clone();
                    Arc::new(move |text: &str| q(&it(text))) as StyleFn
                };
                let quote_prefix = Self::get_style_prefix(&quote_style);
                let quote_ctx = InlineStyleContext {
                    apply_text: Arc::new(|t: &str| t.to_owned()),
                    style_prefix: quote_prefix.clone(),
                };
                let quote_content_width = width.saturating_sub(2).max(1);
                let mut rendered: Vec<String> = Vec::new();
                let mut j = start + 1;
                while j < end {
                    let (block_lines, consumed) =
                        self.render_block(events, j, quote_content_width, Some(&quote_ctx));
                    rendered.extend(block_lines);
                    j += consumed.max(1);
                }
                while rendered.last().is_some_and(|l| l.is_empty()) {
                    rendered.pop();
                }
                let mut lines = Vec::new();
                for quote_line in rendered {
                    let styled = apply_with_reprefix(&quote_style, &quote_prefix, &quote_line);
                    for wrapped in wrap_text_with_ansi(&styled, quote_content_width) {
                        lines.push(format!("{}{wrapped}", (self.theme.quote_border)("│ ")));
                    }
                }
                if self.next_block_needs_spacing(events, end) {
                    lines.push(String::new());
                }
                (lines, end - start + 1)
            }
            Event::Start(Tag::Table(_alignments)) => {
                let end = find_end(events, start, TagEnd::Table);
                let raw_fallback: String = events[start..=end]
                    .iter()
                    .filter_map(|e| match e {
                        Event::Text(t) => Some(t.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                // Collect header + body rows as cell-event slices.
                let mut header: Vec<String> = Vec::new();
                let mut rows: Vec<Vec<String>> = Vec::new();
                let mut i = start + 1;
                let mut in_head = false;
                let mut current_row: Vec<String> = Vec::new();
                let ctx;
                let ctx_ref = if let Some(c) = style_ctx {
                    c
                } else {
                    ctx = self.default_inline_ctx();
                    &ctx
                };
                while i < end {
                    match &events[i] {
                        Event::Start(Tag::TableHead) => {
                            in_head = true;
                            i += 1;
                        }
                        Event::End(TagEnd::TableHead) => {
                            in_head = false;
                            if !current_row.is_empty() {
                                header = std::mem::take(&mut current_row);
                            }
                            i += 1;
                        }
                        Event::Start(Tag::TableRow) => {
                            current_row.clear();
                            i += 1;
                        }
                        Event::End(TagEnd::TableRow) => {
                            if in_head {
                                header = std::mem::take(&mut current_row);
                            } else if !current_row.is_empty() {
                                rows.push(std::mem::take(&mut current_row));
                            }
                            i += 1;
                        }
                        Event::Start(Tag::TableCell) => {
                            let cell_end = find_end(events, i, TagEnd::TableCell);
                            let text = self.render_inlines(&events[i + 1..cell_end], ctx_ref);
                            current_row.push(text);
                            i = cell_end + 1;
                        }
                        _ => {
                            i += 1;
                        }
                    }
                }
                let mut lines =
                    self.render_table(&header, &rows, width, &raw_fallback, style_ctx);
                if self.next_block_needs_spacing(events, end) {
                    lines.push(String::new());
                }
                (lines, end - start + 1)
            }
            Event::Rule => {
                let rule = "─".repeat(width.min(80));
                let mut lines = vec![(self.theme.hr)(&rule)];
                if self.next_block_needs_spacing(events, start) {
                    lines.push(String::new());
                }
                (lines, 1)
            }
            Event::Start(Tag::HtmlBlock) => {
                let end = find_end(events, start, TagEnd::HtmlBlock);
                let mut raw = String::new();
                for e in &events[start + 1..end] {
                    if let Event::Html(t) | Event::Text(t) = e {
                        raw.push_str(t);
                    }
                }
                let styled = self.apply_default_style(raw.trim());
                (vec![styled], end - start + 1)
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let styled = self.apply_default_style(html.trim());
                (vec![styled], 1)
            }
            Event::Text(t) => {
                // Loose text (rare at block level).
                let ctx;
                let ctx_ref = if let Some(c) = style_ctx {
                    c
                } else {
                    ctx = self.default_inline_ctx();
                    &ctx
                };
                (vec![(ctx_ref.apply_text)(t)], 1)
            }
            Event::SoftBreak | Event::HardBreak => (vec![String::new()], 1),
            Event::Start(Tag::Item) => {
                // Orphan item — treat as list of one.
                let end = find_end(events, start, TagEnd::Item);
                let lines = self.render_list(&events[start..=end], 0, width, false, 1, style_ctx);
                (lines, end - start + 1)
            }
            Event::End(_) => (Vec::new(), 1),
            other => {
                // Skip unknown starts by consuming to matching end if possible.
                if let Event::Start(tag) = other {
                    let end_tag = tag.to_end();
                    let end = find_end(events, start, end_tag);
                    (Vec::new(), end - start + 1)
                } else {
                    (Vec::new(), 1)
                }
            }
        }
    }

    fn make_heading_style(&self, level: usize) -> StyleFn {
        let heading = self.theme.heading.clone();
        let bold = self.theme.bold.clone();
        let underline = self.theme.underline.clone();
        if level == 1 {
            Arc::new(move |text: &str| heading(&bold(&underline(text))))
        } else {
            Arc::new(move |text: &str| heading(&bold(text)))
        }
    }

    fn get_longest_word_width(text: &str, max_width: usize) -> usize {
        let mut max_w = 1usize;
        for word in text.split_whitespace() {
            let w = visible_width(word).min(max_width);
            max_w = max_w.max(w.max(1));
        }
        max_w
    }

    fn wrap_cell_text(text: &str, max_width: usize) -> Vec<String> {
        wrap_text_with_ansi(text, max_width.max(1))
    }

    /// Port of markdown.ts renderTable — box-drawing borders + width-aware wrap.
    fn render_table(
        &self,
        header: &[String],
        rows: &[Vec<String>],
        available_width: usize,
        raw_fallback: &str,
        _style_ctx: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let num_cols = header.len();
        if num_cols == 0 {
            return lines;
        }

        // Border overhead: "│ " + (n-1)*" │ " + " │" = 3n + 1
        let border_overhead = 3 * num_cols + 1;
        let available_for_cells = available_width.saturating_sub(border_overhead);
        if available_for_cells < num_cols {
            let mut fallback = wrap_text_with_ansi(raw_fallback, available_width.max(1));
            if !fallback.is_empty() {
                lines.append(&mut fallback);
            }
            return lines;
        }

        const MAX_UNBROKEN: usize = 30;
        let mut natural_widths = vec![0usize; num_cols];
        let mut min_word_widths = vec![1usize; num_cols];
        for (i, cell) in header.iter().enumerate() {
            natural_widths[i] = visible_width(cell);
            min_word_widths[i] = Self::get_longest_word_width(cell, MAX_UNBROKEN).max(1);
        }
        for row in rows {
            for (i, cell) in row.iter().enumerate().take(num_cols) {
                natural_widths[i] = natural_widths[i].max(visible_width(cell));
                min_word_widths[i] = min_word_widths[i]
                    .max(Self::get_longest_word_width(cell, MAX_UNBROKEN).max(1));
            }
        }

        let mut min_column_widths = min_word_widths.clone();
        let mut min_cells_width: usize = min_column_widths.iter().sum();

        if min_cells_width > available_for_cells {
            min_column_widths = vec![1usize; num_cols];
            let remaining = available_for_cells.saturating_sub(num_cols);
            if remaining > 0 {
                let total_weight: usize = min_word_widths
                    .iter()
                    .map(|w| w.saturating_sub(1))
                    .sum();
                let mut growth = vec![0usize; num_cols];
                for i in 0..num_cols {
                    let weight = min_word_widths[i].saturating_sub(1);
                    growth[i] = weight
                        .checked_mul(remaining)
                        .and_then(|n| n.checked_div(total_weight))
                        .unwrap_or(0);
                }
                for i in 0..num_cols {
                    min_column_widths[i] += growth[i];
                }
                let allocated: usize = growth.iter().sum();
                let mut leftover = remaining.saturating_sub(allocated);
                let mut i = 0;
                while leftover > 0 && i < num_cols {
                    min_column_widths[i] += 1;
                    leftover -= 1;
                    i += 1;
                }
            }
            min_cells_width = min_column_widths.iter().sum();
        }

        let total_natural: usize = natural_widths.iter().sum::<usize>() + border_overhead;
        let column_widths: Vec<usize> = if total_natural <= available_width {
            natural_widths
                .iter()
                .enumerate()
                .map(|(i, w)| (*w).max(min_column_widths[i]))
                .collect()
        } else {
            let total_grow: usize = natural_widths
                .iter()
                .enumerate()
                .map(|(i, w)| w.saturating_sub(min_column_widths[i]))
                .sum();
            let extra = available_for_cells.saturating_sub(min_cells_width);
            let mut column_widths: Vec<usize> = min_column_widths
                .iter()
                .enumerate()
                .map(|(i, min_w)| {
                    let delta = natural_widths[i].saturating_sub(*min_w);
                    let grow = delta
                        .checked_mul(extra)
                        .and_then(|n| n.checked_div(total_grow))
                        .unwrap_or(0);
                    min_w + grow
                })
                .collect();
            let allocated: usize = column_widths.iter().sum();
            let mut remaining = available_for_cells.saturating_sub(allocated);
            while remaining > 0 {
                let mut grew = false;
                for i in 0..num_cols {
                    if remaining == 0 {
                        break;
                    }
                    if column_widths[i] < natural_widths[i] {
                        column_widths[i] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            column_widths
        };

        // Top border
        let top_cells: Vec<String> = column_widths.iter().map(|w| "─".repeat(*w)).collect();
        lines.push(format!("┌─{}─┐", top_cells.join("─┬─")));

        // Header with wrapping
        let header_cell_lines: Vec<Vec<String>> = header
            .iter()
            .enumerate()
            .map(|(i, text)| Self::wrap_cell_text(text, column_widths[i]))
            .collect();
        let header_line_count = header_cell_lines
            .iter()
            .map(|c| c.len())
            .max()
            .unwrap_or(1);
        for line_idx in 0..header_line_count {
            let row_parts: Vec<String> = header_cell_lines
                .iter()
                .enumerate()
                .map(|(col_idx, cell_lines)| {
                    let text = cell_lines.get(line_idx).map(|s| s.as_str()).unwrap_or("");
                    let pad = column_widths[col_idx].saturating_sub(visible_width(text));
                    let padded = format!("{text}{}", " ".repeat(pad));
                    (self.theme.bold)(&padded)
                })
                .collect();
            lines.push(format!("│ {} │", row_parts.join(" │ ")));
        }

        // Separator
        let sep_cells: Vec<String> = column_widths.iter().map(|w| "─".repeat(*w)).collect();
        let separator = format!("├─{}─┤", sep_cells.join("─┼─"));
        lines.push(separator.clone());

        // Body rows
        for (row_index, row) in rows.iter().enumerate() {
            let row_cell_lines: Vec<Vec<String>> = (0..num_cols)
                .map(|i| {
                    let text = row.get(i).map(|s| s.as_str()).unwrap_or("");
                    Self::wrap_cell_text(text, column_widths[i])
                })
                .collect();
            let row_line_count = row_cell_lines.iter().map(|c| c.len()).max().unwrap_or(1);
            for line_idx in 0..row_line_count {
                let row_parts: Vec<String> = row_cell_lines
                    .iter()
                    .enumerate()
                    .map(|(col_idx, cell_lines)| {
                        let text = cell_lines.get(line_idx).map(|s| s.as_str()).unwrap_or("");
                        let pad = column_widths[col_idx].saturating_sub(visible_width(text));
                        format!("{text}{}", " ".repeat(pad))
                    })
                    .collect();
                lines.push(format!("│ {} │", row_parts.join(" │ ")));
            }
            if row_index < rows.len() - 1 {
                lines.push(separator.clone());
            }
        }

        // Bottom border
        let bottom_cells: Vec<String> = column_widths.iter().map(|w| "─".repeat(*w)).collect();
        lines.push(format!("└─{}─┘", bottom_cells.join("─┴─")));
        lines
    }

    fn next_block_needs_spacing(&self, events: &[Event<'_>], end_idx: usize) -> bool {
        // end_idx is the End event index; look at the next event after it.
        let next = end_idx + 1;
        if next >= events.len() {
            return false;
        }
        // TS: if nextTokenType && nextTokenType !== "space"
        // pulldown-cmark has no space tokens; blank lines are just gaps between blocks.
        // We add spacing between adjacent blocks always (except trailing).
        !matches!(&events[next], Event::End(_))
    }

    fn next_block_needs_para_spacing(&self, events: &[Event<'_>], end_idx: usize) -> bool {
        let next = end_idx + 1;
        if next >= events.len() {
            return false;
        }
        // Don't add spacing before lists (TS: next !== "list" && next !== "space")
        matches!(&events[next], Event::Start(Tag::List(_)) | Event::End(_)).then_some(false).unwrap_or(true)
    }

    fn render_list(
        &mut self,
        item_events: &[Event<'_>],
        depth: usize,
        width: usize,
        ordered: bool,
        start_number: u64,
        style_ctx: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let indent = "    ".repeat(depth);
        let mut item_index = 0u64;
        let mut i = 0;
        while i < item_events.len() {
            match &item_events[i] {
                Event::Start(Tag::Item) => {
                    let end = find_end(item_events, i, TagEnd::Item);
                    let body = &item_events[i + 1..end];
                    let mut task_marker = String::new();
                    let mut body_start = 0;
                    if let Some(Event::TaskListMarker(checked)) = body.first() {
                        task_marker = format!("[{}] ", if *checked { "x" } else { " " });
                        body_start = 1;
                    }
                    let bullet = if ordered {
                        if self.options.preserve_ordered_list_markers {
                            // Best-effort: we don't have raw source markers; renumber.
                            format!("{}. ", start_number + item_index)
                        } else {
                            format!("{}. ", start_number + item_index)
                        }
                    } else {
                        "- ".to_owned()
                    };
                    let marker = format!("{bullet}{task_marker}");
                    let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
                    let continuation_prefix =
                        format!("{indent}{}", " ".repeat(visible_width(&marker)));
                    let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
                    let mut rendered_any = false;
                    let body_events = &body[body_start..];
                    let mut j = 0;
                    while j < body_events.len() {
                        if let Event::Start(Tag::List(n)) = &body_events[j] {
                            let nested_ordered = n.is_some();
                            let nested_start = n.unwrap_or(1);
                            let nested_end = find_end(body_events, j, TagEnd::List(nested_ordered));
                            let nested = self.render_list(
                                &body_events[j + 1..nested_end],
                                depth + 1,
                                width,
                                nested_ordered,
                                nested_start,
                                style_ctx,
                            );
                            lines.extend(nested);
                            rendered_any = true;
                            j = nested_end + 1;
                            continue;
                        }
                        let (block_lines, consumed) =
                            self.render_block(body_events, j, item_width, style_ctx);
                        for line in block_lines {
                            for wrapped in wrap_text_with_ansi(&line, item_width) {
                                let prefix = if rendered_any {
                                    &continuation_prefix
                                } else {
                                    &first_prefix
                                };
                                lines.push(format!("{prefix}{wrapped}"));
                                rendered_any = true;
                            }
                        }
                        j += consumed.max(1);
                    }
                    if !rendered_any {
                        lines.push(first_prefix);
                    }
                    item_index += 1;
                    i = end + 1;
                }
                _ => {
                    i += 1;
                }
            }
        }
        lines
    }

    fn render_inlines(&self, events: &[Event<'_>], ctx: &InlineStyleContext) -> String {
        let mut result = String::new();
        let apply_text_with_newlines = |text: &str| -> String {
            text.split('\n')
                .map(|seg| (ctx.apply_text)(seg))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut i = 0;
        while i < events.len() {
            match &events[i] {
                Event::Text(t) => {
                    result.push_str(&apply_text_with_newlines(t));
                    i += 1;
                }
                Event::Code(code) => {
                    result.push_str(&(self.theme.code)(code));
                    result.push_str(&ctx.style_prefix);
                    i += 1;
                }
                Event::SoftBreak => {
                    result.push(' ');
                    i += 1;
                }
                Event::HardBreak => {
                    result.push('\n');
                    i += 1;
                }
                Event::Start(Tag::Strong) => {
                    let end = find_end(events, i, TagEnd::Strong);
                    let inner = self.render_inlines(&events[i + 1..end], ctx);
                    result.push_str(&(self.theme.bold)(&inner));
                    result.push_str(&ctx.style_prefix);
                    i = end + 1;
                }
                Event::Start(Tag::Emphasis) => {
                    let end = find_end(events, i, TagEnd::Emphasis);
                    let inner = self.render_inlines(&events[i + 1..end], ctx);
                    result.push_str(&(self.theme.italic)(&inner));
                    result.push_str(&ctx.style_prefix);
                    i = end + 1;
                }
                Event::Start(Tag::Strikethrough) => {
                    let end = find_end(events, i, TagEnd::Strikethrough);
                    let inner = self.render_inlines(&events[i + 1..end], ctx);
                    result.push_str(&(self.theme.strikethrough)(&inner));
                    result.push_str(&ctx.style_prefix);
                    i = end + 1;
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let end = find_end(events, i, TagEnd::Link);
                    let link_text = self.render_inlines(&events[i + 1..end], ctx);
                    // raw text for equality check
                    let raw_text = plain_text_from_events(&events[i + 1..end]);
                    let styled_link = (self.theme.link)(&(self.theme.underline)(&link_text));
                    let caps = get_capabilities();
                    if caps.hyperlinks {
                        result.push_str(&hyperlink(&styled_link, dest_url));
                        result.push_str(&ctx.style_prefix);
                    } else {
                        let href_for_cmp = dest_url
                            .strip_prefix("mailto:")
                            .unwrap_or(dest_url.as_ref());
                        if raw_text == dest_url.as_ref() || raw_text == href_for_cmp {
                            result.push_str(&styled_link);
                            result.push_str(&ctx.style_prefix);
                        } else {
                            result.push_str(&styled_link);
                            result.push_str(&(self.theme.link_url)(&format!(" ({dest_url})")));
                            result.push_str(&ctx.style_prefix);
                        }
                    }
                    i = end + 1;
                }
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let end = find_end(events, i, TagEnd::Image);
                    let alt = plain_text_from_events(&events[i + 1..end]);
                    let label = if !alt.is_empty() {
                        alt
                    } else if !title.is_empty() {
                        title.to_string()
                    } else {
                        dest_url.to_string()
                    };
                    result.push_str(&apply_text_with_newlines(&format!("[{label}]")));
                    i = end + 1;
                }
                Event::Html(h) | Event::InlineHtml(h) => {
                    result.push_str(&apply_text_with_newlines(h));
                    i += 1;
                }
                Event::Start(tag) => {
                    let end = find_end(events, i, tag.to_end());
                    result.push_str(&self.render_inlines(&events[i + 1..end], ctx));
                    i = end + 1;
                }
                Event::End(_) => {
                    i += 1;
                }
                Event::Rule => {
                    i += 1;
                }
                Event::TaskListMarker(_)
                | Event::FootnoteReference(_)
                | Event::InlineMath(_)
                | Event::DisplayMath(_) => {
                    i += 1;
                }
            }
        }

        while !ctx.style_prefix.is_empty() && result.ends_with(&ctx.style_prefix) {
            result.truncate(result.len() - ctx.style_prefix.len());
        }
        result
    }
}

fn apply_with_reprefix(style: &StyleFn, prefix: &str, line: &str) -> String {
    if prefix.is_empty() {
        return style(line);
    }
    let reapplied = line.replace("\x1b[0m", &format!("\x1b[0m{prefix}"));
    style(&reapplied)
}

fn plain_text_from_events(events: &[Event<'_>]) -> String {
    let mut out = String::new();
    for e in events {
        match e {
            Event::Text(t) | Event::Code(t) => out.push_str(t),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn find_end(events: &[Event<'_>], start: usize, end_tag: TagEnd) -> usize {
    let mut depth = 0i32;
    for (idx, e) in events.iter().enumerate().skip(start) {
        match e {
            Event::Start(t) if tags_match_end(t, end_tag) || idx == start => {
                // Count nested same-kind starts.
                if idx == start {
                    depth = 1;
                } else if t.to_end() == end_tag
                    || matches!((t.to_end(), end_tag), (TagEnd::List(_), TagEnd::List(_)))
                    || matches!((t.to_end(), end_tag), (TagEnd::Heading(_), TagEnd::Heading(_)))
                    || matches!(
                        (t.to_end(), end_tag),
                        (TagEnd::BlockQuote(_), TagEnd::BlockQuote(_))
                    )
                {
                    depth += 1;
                }
            }
            Event::Start(t) if t.to_end() == end_tag => {
                depth += 1;
            }
            Event::End(t) if *t == end_tag => {
                depth -= 1;
                if depth == 0 {
                    return idx;
                }
            }
            Event::End(t)
                if *t == end_tag
                    || matches!((*t, end_tag), (TagEnd::List(a), TagEnd::List(b)) if a == b)
                    || matches!((*t, end_tag), (TagEnd::List(_), TagEnd::List(_)))
                    || matches!((*t, end_tag), (TagEnd::Heading(_), TagEnd::Heading(_)))
                    || matches!(
                        (*t, end_tag),
                        (TagEnd::BlockQuote(_), TagEnd::BlockQuote(_))
                    ) =>
            {
                depth -= 1;
                if depth == 0 {
                    return idx;
                }
            }
            _ => {}
        }
    }
    // Fallback: scan with a simpler depth counter for the start tag family.
    find_end_simple(events, start, end_tag)
}

fn tags_match_end(tag: &Tag<'_>, end: TagEnd) -> bool {
    tag.to_end() == end
}

fn find_end_simple(events: &[Event<'_>], start: usize, end_tag: TagEnd) -> usize {
    let mut depth = 0i32;
    for (idx, e) in events.iter().enumerate().skip(start) {
        match e {
            Event::Start(t) => {
                if (idx == start || same_end_family(t.to_end(), end_tag))
                    && (idx == start || t.to_end() == end_tag || list_family(t.to_end(), end_tag)) {
                        depth += 1;
                    }
            }
            Event::End(t)
                if (*t == end_tag || list_family(*t, end_tag) || heading_family(*t, end_tag)) => {
                    depth -= 1;
                    if depth == 0 {
                        return idx;
                    }
                }
            _ => {}
        }
    }
    events.len().saturating_sub(1)
}

fn list_family(a: TagEnd, b: TagEnd) -> bool {
    matches!((a, b), (TagEnd::List(_), TagEnd::List(_))) && a == b
}

fn heading_family(a: TagEnd, b: TagEnd) -> bool {
    matches!((a, b), (TagEnd::Heading(_), TagEnd::Heading(_))) && a == b
}

fn same_end_family(a: TagEnd, b: TagEnd) -> bool {
    a == b
        || matches!((a, b), (TagEnd::List(_), TagEnd::List(_)))
        || matches!((a, b), (TagEnd::Heading(_), TagEnd::Heading(_)))
        || matches!((a, b), (TagEnd::BlockQuote(_), TagEnd::BlockQuote(_)))
}

fn find_blockquote_end(events: &[Event<'_>], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (idx, e) in events.iter().enumerate().skip(start) {
        match e {
            Event::Start(Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

impl Component for Markdown {
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

    fn plain_lines(md: &mut Markdown, width: u16) -> Vec<String> {
        md.render(width)
            .iter()
            .map(|l| l.plain_text().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn empty_text_renders_no_lines_and_caches() {
        let mut md = Markdown::new("", 0, 0, default_markdown_theme(), None, None);
        let lines = md.render(40);
        assert!(lines.is_empty());
        assert_eq!(md.last_render_status(), RenderStatus::Changed);
        let _ = md.render(40);
        assert_eq!(md.last_render_status(), RenderStatus::Unchanged);
    }

    #[test]
    fn plain_text_wraps_to_width() {
        let mut md = Markdown::new(
            "alpha beta gamma delta epsilon zeta",
            0,
            0,
            default_markdown_theme(),
            None,
            None,
        );
        let lines = plain_lines(&mut md, 20);
        assert!(lines.len() >= 2, "expected wrap, got {lines:?}");
        assert!(lines[0].len() <= 20 || visible_width(&lines[0]) <= 20);
        // content present
        let joined = lines.join(" ");
        assert!(joined.contains("alpha"));
        assert!(joined.contains("zeta"));
    }

    #[test]
    fn cache_hit_unchanged_miss_on_width_or_set_text() {
        let mut md = Markdown::with_text("hello world");
        let a = md.render(40).len();
        assert_eq!(md.last_render_status(), RenderStatus::Changed);
        let b = md.render(40).len();
        assert_eq!(a, b);
        assert_eq!(md.last_render_status(), RenderStatus::Unchanged);

        let _ = md.render(20);
        assert_eq!(md.last_render_status(), RenderStatus::Changed);

        md.set_text("other");
        let _ = md.render(20);
        assert_eq!(md.last_render_status(), RenderStatus::Changed);
    }

    #[test]
    fn heading_and_bold_render() {
        let mut md = Markdown::new(
            "# Title\n\nSome **bold** text",
            0,
            0,
            default_markdown_theme(),
            None,
            None,
        );
        let lines = plain_lines(&mut md, 80);
        let joined = lines.join("\n");
        assert!(joined.contains("Title"), "{joined:?}");
        assert!(joined.contains("bold"), "{joined:?}");
    }

    #[test]
    fn unordered_list_markers() {
        let mut md = Markdown::new("- one\n- two", 0, 0, default_markdown_theme(), None, None);
        let lines = plain_lines(&mut md, 80);
        assert!(lines.iter().any(|l| l.contains("- one")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("- two")), "{lines:?}");
    }

    #[test]
    fn code_block_fences() {
        let mut md = Markdown::new(
            "```rs\nlet x = 1;\n```",
            0,
            0,
            default_markdown_theme(),
            None,
            None,
        );
        let lines = plain_lines(&mut md, 80);
        assert!(lines.iter().any(|l| l.contains("```rs")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("let x = 1;")), "{lines:?}");
        assert!(lines.iter().any(|l| l.trim() == "```"), "{lines:?}");
    }

    #[test]
    fn simple_table_renders_borders_and_cells() {
        let src = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |";
        let mut md = Markdown::new(src, 0, 0, default_markdown_theme(), None, None);
        let lines = plain_lines(&mut md, 80);
        let joined = lines.join("\n");
        assert!(joined.contains("Name"), "{joined}");
        assert!(joined.contains("Age"), "{joined}");
        assert!(joined.contains("Alice"), "{joined}");
        assert!(joined.contains("Bob"), "{joined}");
        assert!(
            lines.iter().any(|l| l.contains('│')),
            "expected vertical borders: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('─')),
            "expected horizontal borders: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('┌') || l.contains('├') || l.contains('└')),
            "expected box corners: {lines:?}"
        );
    }

    #[test]
    fn table_row_dividers_between_data_rows() {
        let src = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |";
        let mut md = Markdown::new(src, 0, 0, default_markdown_theme(), None, None);
        let lines = plain_lines(&mut md, 80);
        let seps: Vec<_> = lines
            .iter()
            .filter(|l| l.contains('├') && l.contains('┼'))
            .collect();
        // header separator + one between data rows
        assert!(
            seps.len() >= 2,
            "expected row dividers, got {seps:?} in {lines:?}"
        );
    }

    #[test]
    fn table_cells_wrap_when_narrow() {
        let src = "| Command | Description |\n| --- | --- |\n| npm install | Install all dependencies for the project |";
        let mut md = Markdown::new(src, 0, 0, default_markdown_theme(), None, None);
        let width = 40u16;
        let lines = plain_lines(&mut md, width);
        for line in &lines {
            assert!(
                visible_width(line) <= width as usize,
                "line exceeds width {width}: {line:?} (vw={})",
                visible_width(line)
            );
        }
        let joined = lines.join(" ");
        assert!(joined.contains("npm") || joined.contains("install"), "{joined}");
    }
}

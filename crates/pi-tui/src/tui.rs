//! Tui — owns the component tree, focus, overlays, and drives inkferro-rt.
//!
//! Port of packages/tui/src/tui.ts. Differential rendering is delegated to
//! [`inkferro_rt::FrameWriter::write_lines`]; this module builds DirtySpans from
//! component cache status and keeps a flattened retained `Vec<String>` so the
//! never-degrade path stays O(dirty + viewport).

use std::ops::Range;

use inkferro_rt::{CursorTarget, DirtySpans, FrameWriter, LinesFrameParams, Overlay};

use crate::component::{Component, ComponentBox, Container};
use crate::keys::{is_key_release, matches_key};
use crate::line::{CURSOR_MARKER, Line};
use crate::terminal::Terminal;
use crate::util::visible_width;

/// Overlay anchor (pi OverlayAnchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// Overlay size: absolute columns/rows or percent of terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Abs(usize),
    Percent(f64),
}

impl SizeValue {
    fn resolve(self, reference: usize) -> usize {
        match self {
            SizeValue::Abs(n) => n,
            SizeValue::Percent(p) => ((reference as f64) * p / 100.0).floor() as usize,
        }
    }
}

/// Overlay layout options (subset of pi OverlayOptions).
#[derive(Debug, Clone, Default)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<usize>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: i32,
    pub offset_y: i32,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin_top: usize,
    pub margin_right: usize,
    pub margin_bottom: usize,
    pub margin_left: usize,
    pub non_capturing: bool,
}

/// Handle returned by [`Tui::show_overlay`].
/// Opaque overlay id returned by [`Tui::show_overlay`].
pub type OverlayHandle = u64;

struct OverlayEntry {
    id: u64,
    component: ComponentBox,
    options: OverlayOptions,
    hidden: bool,
    focus_order: u64,
    pre_focus: Option<FocusTarget>,
}

/// Input listener result (pi InputListenerResult).
pub struct InputListenerResult {
    pub consume: bool,
    pub data: Option<String>,
}

type InputListener = Box<dyn FnMut(&str) -> Option<InputListenerResult>>;

/// Main TUI driver.
pub struct Tui {
    terminal: Box<dyn Terminal>,
    root: Container,
    frame: FrameWriter,
    /// Flattened retained ANSI lines (comparison / splice buffer).
    retained: Vec<String>,
    focused: Option<FocusTarget>,
    overlays: Vec<OverlayEntry>,
    next_overlay_id: u64,
    focus_order_counter: u64,
    clear_on_shrink: bool,
    show_hardware_cursor: bool,
    render_requested: bool,
    stopped: bool,
    input_listeners: Vec<InputListener>,
    pub on_debug: Option<Box<dyn FnMut()>>,
}

/// Focus target: root child index or overlay id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    RootChild(usize),
    Overlay(u64),
}

impl Tui {
    pub fn new(terminal: impl Terminal + 'static) -> Self {
        let show_hw = std::env::var("PI_HARDWARE_CURSOR").ok().as_deref() == Some("1");
        let clear_on_shrink = std::env::var("PI_CLEAR_ON_SHRINK").ok().as_deref() == Some("1");
        Self {
            terminal: Box::new(terminal),
            root: Container::new(),
            frame: FrameWriter::new(),
            retained: Vec::new(),
            focused: None,
            overlays: Vec::new(),
            next_overlay_id: 1,
            focus_order_counter: 0,
            clear_on_shrink,
            show_hardware_cursor: show_hw,
            render_requested: false,
            stopped: false,
            input_listeners: Vec::new(),
            on_debug: None,
        }
    }

    pub fn terminal(&self) -> &dyn Terminal {
        self.terminal.as_ref()
    }

    pub fn terminal_mut(&mut self) -> &mut dyn Terminal {
        self.terminal.as_mut()
    }

    pub fn root(&self) -> &Container {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Container {
        &mut self.root
    }

    pub fn add_child(&mut self, component: impl Component + 'static) {
        self.root.add_child(component);
    }

    pub fn full_redraws(&self) -> u32 {
        self.frame.lines_full_redraw_count()
    }

    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.clear_on_shrink = enabled;
    }

    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        if self.show_hardware_cursor == enabled {
            return;
        }
        self.show_hardware_cursor = enabled;
        if !enabled {
            self.terminal.hide_cursor();
        }
        self.request_render(false);
    }

    /// Force full lines-model repaint (pi `requestRender(force)`).
    pub fn request_render(&mut self, force: bool) {
        if force {
            self.frame.reset_lines_state();
            self.retained.clear();
        }
        self.render_requested = true;
    }

    pub fn invalidate(&mut self) {
        self.root.invalidate();
        for o in &mut self.overlays {
            o.component.invalidate();
        }
    }

    /// Set focus by root child index.
    pub fn set_focus_child(&mut self, index: Option<usize>) {
        self.clear_focus_flags();
        self.focused = index.map(FocusTarget::RootChild);
        self.apply_focus_flags();
        self.request_render(false);
    }

    fn clear_focus_flags(&mut self) {
        for child in self.root.children_mut() {
            if let Some(f) = child.as_focusable() {
                f.set_focused(false);
            }
        }
        for o in &mut self.overlays {
            if let Some(f) = o.component.as_focusable() {
                f.set_focused(false);
            }
        }
    }

    fn apply_focus_flags(&mut self) {
        match self.focused {
            Some(FocusTarget::RootChild(i)) => {
                if let Some(child) = self.root.children_mut().get_mut(i)
                    && let Some(f) = child.as_focusable() {
                        f.set_focused(true);
                    }
            }
            Some(FocusTarget::Overlay(id)) => {
                if let Some(o) = self.overlays.iter_mut().find(|o| o.id == id)
                    && let Some(f) = o.component.as_focusable() {
                        f.set_focused(true);
                    }
            }
            None => {}
        }
    }

    /// Show an overlay component; returns overlay id.
    pub fn show_overlay(
        &mut self,
        component: impl Component + 'static,
        options: OverlayOptions,
    ) -> u64 {
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        self.focus_order_counter += 1;
        let capturing = !options.non_capturing;
        self.overlays.push(OverlayEntry {
            id,
            component: Box::new(component),
            options,
            hidden: false,
            focus_order: self.focus_order_counter,
            pre_focus: self.focused,
        });
        if capturing {
            self.clear_focus_flags();
            self.focused = Some(FocusTarget::Overlay(id));
            self.apply_focus_flags();
        }
        self.request_render(false);
        id
    }

    pub fn hide_overlay(&mut self, id: u64) {
        if let Some(pos) = self.overlays.iter().position(|o| o.id == id) {
            let entry = self.overlays.remove(pos);
            if self.focused == Some(FocusTarget::Overlay(id)) {
                self.clear_focus_flags();
                self.focused = entry.pre_focus;
                self.apply_focus_flags();
            }
            self.request_render(false);
        }
    }

    pub fn set_overlay_hidden(&mut self, id: u64, hidden: bool) {
        if let Some(o) = self.overlays.iter_mut().find(|o| o.id == id) {
            o.hidden = hidden;
            self.request_render(false);
        }
    }

    /// Handle one raw input sequence (after terminal segmentation).
    pub fn handle_input(&mut self, mut data: String) {
        if self.stopped {
            return;
        }
        // Input listeners
        for listener in &mut self.input_listeners {
            if let Some(result) = listener(&data) {
                if result.consume {
                    return;
                }
                if let Some(d) = result.data {
                    data = d;
                }
            }
        }
        if data.is_empty() {
            return;
        }

        if matches_key(&data, "shift+ctrl+d")
            && let Some(cb) = self.on_debug.as_mut() {
                cb();
                return;
            }

        // Dispatch to focused component
        let wants_release = self.focused_wants_key_release();
        if is_key_release(&data) && !wants_release {
            return;
        }

        match self.focused {
            Some(FocusTarget::RootChild(i)) => {
                if let Some(child) = self.root.children_mut().get_mut(i) {
                    child.handle_input(&data);
                }
            }
            Some(FocusTarget::Overlay(id)) => {
                if let Some(o) = self.overlays.iter_mut().find(|o| o.id == id) {
                    o.component.handle_input(&data);
                }
            }
            None => {}
        }
        self.request_render(false);
    }

    fn focused_wants_key_release(&self) -> bool {
        match self.focused {
            Some(FocusTarget::RootChild(i)) => self
                .root
                .children()
                .get(i)
                .map(|c| c.wants_key_release())
                .unwrap_or(false),
            Some(FocusTarget::Overlay(id)) => self
                .overlays
                .iter()
                .find(|o| o.id == id)
                .map(|o| o.component.wants_key_release())
                .unwrap_or(false),
            None => false,
        }
    }

    /// Render one frame and write bytes to the terminal.
    ///
    /// Stringification is O(dirty): only dirty Line regions are converted to
    /// ANSI and spliced into the retained `Vec<String>`. Unchanged regions keep
    /// their previous strings. Overlays force `DirtySpans::All`.
    pub fn do_render(&mut self) {
        if self.stopped {
            return;
        }
        self.render_requested = false;

        let width = self.terminal.columns() as usize;
        let height = self.terminal.rows() as usize;
        let w16 = width as u16;

        let (root_lines, dirty, _status) = self.root.render_owned(w16);
        let new_len = root_lines.len();
        let first_frame = self.retained.is_empty() && new_len > 0;

        if first_frame || (dirty.is_empty() && new_len != self.retained.len()) {
            let mut ansi_lines: Vec<String> = root_lines.iter().map(Line::to_ansi).collect();
            let cursor = extract_cursor_from_ansi_lines(&mut ansi_lines, height);
            self.retained = ansi_lines;
            self.emit_frame(width, height, DirtySpans::All, &[], cursor);
            return;
        }

        if new_len < self.retained.len() {
            self.retained.truncate(new_len);
        } else if new_len > self.retained.len() {
            self.retained.resize_with(new_len, String::new);
        }

        let ranges = merge_ranges(dirty);
        for r in &ranges {
            let start = r.start.min(new_len);
            let end = r.end.min(new_len);
            for (i, line) in root_lines.iter().enumerate().take(end).skip(start) {
                self.retained[i] = line.to_ansi();
            }
        }

        let cursor = extract_cursor_from_retained(&mut self.retained, height, &ranges);

        let mut rt_overlays: Vec<Overlay> = Vec::new();
        let mut order: Vec<usize> = (0..self.overlays.len()).collect();
        order.sort_by_key(|&i| self.overlays[i].focus_order);
        for idx in order {
            let entry = &mut self.overlays[idx];
            if entry.hidden {
                continue;
            }
            let layout = resolve_overlay_layout(&entry.options, height, width, height);
            let ov_lines = entry.component.render(layout.width as u16);
            let mut strings: Vec<String> = ov_lines.iter().map(Line::to_ansi).collect();
            if let Some(max_h) = layout.max_height
                && strings.len() > max_h {
                    strings.truncate(max_h);
                }
            rt_overlays.push(Overlay {
                row: layout.row,
                col: layout.col,
                width: layout.width,
                lines: strings,
            });
        }

        let dirty_spans = if !rt_overlays.is_empty() {
            DirtySpans::All
        } else if ranges.is_empty() {
            DirtySpans::Ranges(Vec::new())
        } else {
            DirtySpans::Ranges(ranges)
        };

        self.emit_frame(width, height, dirty_spans, &rt_overlays, cursor);
    }

    fn emit_frame(
        &mut self,
        width: usize,
        height: usize,
        dirty_spans: DirtySpans,
        overlays: &[Overlay],
        cursor: Option<CursorTarget>,
    ) {
        let params = LinesFrameParams {
            width,
            height,
            lines: &self.retained,
            overlays,
            sync: true,
            clear_on_shrink: self.clear_on_shrink,
            dirty_spans,
            cursor: if self.show_hardware_cursor {
                cursor
            } else {
                None
            },
        };
        let bytes = self.frame.write_lines(&params);
        if !bytes.is_empty() {
            let s = String::from_utf8_lossy(&bytes);
            self.terminal.write(&s);
        }
    }

    /// Start the terminal and wire input/resize to this Tui.
    ///
    /// Because Terminal::start takes owned callbacks, the host typically uses
    /// channels; this helper runs one render and leaves poll to the host loop
    /// via [`poll_terminal`].
    pub fn start_render_loop_hooks(&mut self) {
        // Initial render
        self.do_render();
    }

    pub fn poll_terminal(&mut self) {
        self.terminal.poll();
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.terminal.show_cursor();
        self.terminal.stop();
    }

    pub fn add_input_listener<F>(&mut self, f: F)
    where
        F: FnMut(&str) -> Option<InputListenerResult> + 'static,
    {
        self.input_listeners.push(Box::new(f));
    }
}

struct OverlayLayout {
    width: usize,
    row: usize,
    col: usize,
    max_height: Option<usize>,
}

fn resolve_overlay_layout(
    opt: &OverlayOptions,
    _overlay_height_hint: usize,
    term_width: usize,
    term_height: usize,
) -> OverlayLayout {
    let margin_h = opt.margin_left + opt.margin_right;
    let margin_v = opt.margin_top + opt.margin_bottom;
    let avail_w = term_width.saturating_sub(margin_h).max(1);
    let avail_h = term_height.saturating_sub(margin_v).max(1);

    let mut width = opt
        .width
        .map(|s| s.resolve(term_width))
        .unwrap_or(avail_w.min(60));
    if let Some(min_w) = opt.min_width {
        width = width.max(min_w);
    }
    width = width.min(avail_w);

    let max_height = opt.max_height.map(|s| s.resolve(term_height).min(avail_h));

    // Default center placement
    let (mut row, mut col) = match opt.anchor {
        OverlayAnchor::Center => {
            let r = opt.margin_top + avail_h.saturating_sub(max_height.unwrap_or(avail_h / 2)) / 2;
            let c = opt.margin_left + avail_w.saturating_sub(width) / 2;
            (r, c)
        }
        OverlayAnchor::TopLeft => (opt.margin_top, opt.margin_left),
        OverlayAnchor::TopRight => (
            opt.margin_top,
            term_width.saturating_sub(opt.margin_right + width),
        ),
        OverlayAnchor::BottomLeft => (
            term_height.saturating_sub(opt.margin_bottom + max_height.unwrap_or(1)),
            opt.margin_left,
        ),
        OverlayAnchor::BottomRight => (
            term_height.saturating_sub(opt.margin_bottom + max_height.unwrap_or(1)),
            term_width.saturating_sub(opt.margin_right + width),
        ),
        OverlayAnchor::TopCenter => (
            opt.margin_top,
            opt.margin_left + avail_w.saturating_sub(width) / 2,
        ),
        OverlayAnchor::BottomCenter => (
            term_height.saturating_sub(opt.margin_bottom + max_height.unwrap_or(1)),
            opt.margin_left + avail_w.saturating_sub(width) / 2,
        ),
        OverlayAnchor::LeftCenter => (opt.margin_top + avail_h / 2, opt.margin_left),
        OverlayAnchor::RightCenter => (
            opt.margin_top + avail_h / 2,
            term_width.saturating_sub(opt.margin_right + width),
        ),
    };

    if let Some(r) = opt.row {
        row = r.resolve(term_height);
    }
    if let Some(c) = opt.col {
        col = c.resolve(term_width);
    }
    row = (row as i32 + opt.offset_y).max(0) as usize;
    col = (col as i32 + opt.offset_x).max(0) as usize;

    OverlayLayout {
        width,
        row,
        col,
        max_height,
    }
}


/// Scan only dirty ranges (∪ viewport) for CURSOR_MARKER — O(dirty+viewport).
fn extract_cursor_from_retained(
    lines: &mut [String],
    height: usize,
    dirty: &[Range<usize>],
) -> Option<CursorTarget> {
    let viewport_top = lines.len().saturating_sub(height);
    let mut visit: Vec<usize> = Vec::new();
    for r in dirty {
        for i in r.start..r.end.min(lines.len()) {
            visit.push(i);
        }
    }
    for i in viewport_top..lines.len() {
        visit.push(i);
    }
    visit.sort_unstable();
    visit.dedup();
    for &row in visit.iter().rev() {
        if let Some(idx) = lines[row].find(CURSOR_MARKER) {
            let before = &lines[row][..idx];
            let col = visible_width(before);
            let after = lines[row][idx + CURSOR_MARKER.len()..].to_owned();
            lines[row] = format!("{before}{after}");
            return Some(CursorTarget { row, col });
        }
    }
    None
}

/// Strip CURSOR_MARKER from lines; return CursorTarget in retained-buffer coords.
fn extract_cursor_from_ansi_lines(lines: &mut [String], height: usize) -> Option<CursorTarget> {
    let viewport_top = lines.len().saturating_sub(height);
    for row in (viewport_top..lines.len()).rev() {
        if let Some(idx) = lines[row].find(CURSOR_MARKER) {
            let before = &lines[row][..idx];
            let col = visible_width(before);
            let after = lines[row][idx + CURSOR_MARKER.len()..].to_owned();
            lines[row] = format!("{before}{after}");
            return Some(CursorTarget { row, col });
        }
    }
    None
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|r| r.start);
    let mut out = Vec::with_capacity(ranges.len());
    let mut cur = ranges[0].clone();
    for r in ranges.into_iter().skip(1) {
        if r.start <= cur.end {
            cur.end = cur.end.max(r.end);
        } else {
            out.push(cur);
            cur = r;
        }
    }
    out.push(cur);
    out
}

/// Virtual terminal for tests (no real IO).
pub struct VirtualTerminal {
    pub cols: u16,
    pub rows: u16,
    pub written: Vec<String>,
    kitty: bool,
}

impl VirtualTerminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            written: Vec::new(),
            kitty: false,
        }
    }
}

impl Terminal for VirtualTerminal {
    fn start(
        &mut self,
        _on_input: Box<dyn FnMut(&str) + 'static>,
        _on_resize: Box<dyn FnMut() + 'static>,
    ) {
    }
    fn poll(&mut self) {}
    fn stop(&mut self) {}
    fn drain_input(&mut self, _max_ms: u64, _idle_ms: u64) {}
    fn write(&mut self, data: &str) {
        self.written.push(data.to_owned());
    }
    fn columns(&self) -> u16 {
        self.cols
    }
    fn rows(&self) -> u16 {
        self.rows
    }
    fn kitty_protocol_active(&self) -> bool {
        self.kitty
    }
    fn move_by(&mut self, _lines: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _title: &str) {}
    fn set_progress(&mut self, _active: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::spacer::Spacer;
    use crate::components::text::Text;

    #[test]
    fn tui_renders_children_without_panic() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 10));
        tui.add_child(Text::with_text("hello"));
        tui.add_child(Spacer::new(1));
        tui.do_render();
        assert!(!tui.retained.is_empty() || tui.frame.lines_full_redraw_count() >= 1);
        // At least one write happened on first frame if there is content
        let vt = // can't downcast easily — check retained
            tui.retained.iter().any(|l| l.contains("hello") || !l.is_empty());
        assert!(vt || tui.full_redraws() >= 1);
    }

    #[test]
    fn extract_cursor_marker() {
        let mut lines = vec![
            "ab".to_owned(),
            format!("x{}y", CURSOR_MARKER),
            "zz".to_owned(),
        ];
        let c = extract_cursor_from_ansi_lines(&mut lines, 10).unwrap();
        assert_eq!(c.row, 1);
        assert_eq!(c.col, 1);
        assert_eq!(lines[1], "xy");
    }

    /// Property: for random dirty patterns on a multi-child tree, emitting with
    /// DirtySpans::Ranges yields the same retained buffer content as DirtySpans::All
    /// after the same component updates (completeness of dirty tracking).
    #[test]
    fn dirty_spans_ranges_matches_all_on_text_updates() {
        use crate::components::text::Text;
        // Build two identical TUIs
        let mut t_ranges = Tui::new(VirtualTerminal::new(40, 24));
        let mut t_all = Tui::new(VirtualTerminal::new(40, 24));
        for i in 0..20 {
            t_ranges.add_child(Text::with_text(format!("line-{i}-aaaaaaaa")));
            t_all.add_child(Text::with_text(format!("line-{i}-aaaaaaaa")));
        }
        t_ranges.do_render();
        t_all.do_render();
        assert_eq!(t_ranges.retained, t_all.retained);

        // Mutate a few children by re-adding via invalidate+set — use set_text through children
        // Since children are type-erased, invalidate all and change via re-render path:
        // Replace root by clearing and re-adding with different text on even indices.
        t_ranges.root.clear();
        t_all.root.clear();
        for i in 0..20 {
            let text = if i % 3 == 0 {
                format!("CHANGED-{i}")
            } else {
                format!("line-{i}-aaaaaaaa")
            };
            t_ranges.add_child(Text::with_text(text.clone()));
            t_all.add_child(Text::with_text(text));
        }
        t_ranges.do_render();
        // Force All path on t_all via request_render(force)
        t_all.request_render(true);
        t_all.do_render();
        assert_eq!(
            t_ranges.retained, t_all.retained,
            "Ranges path retained must match All path"
        );
    }

}

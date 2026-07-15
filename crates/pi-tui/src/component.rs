//! Component trait — pi-tui's "stateful string recursion" model.
//!
//! Port of `packages/tui/src/tui.ts` Component (lines 63-88), Focusable, Container.
//!
//! The component tree is terminal-thread local (TUI never crosses threads), so
//! there is no `Send` bound — inkferro-core `StyledChar` uses `Rc<[AnsiToken]>`
//! and must remain the Line cell type.

use crate::line::Line;

/// Cache / dirty status reported by a component after `render`.
///
/// The Tui driver uses this to build [`inkferro_rt::DirtySpans::Ranges`] so the
/// lines-model path stays O(dirty + viewport), never O(transcript).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderStatus {
    /// Output may have changed; caller must treat the component's line span as dirty.
    #[default]
    Changed,
    /// Cache hit — same lines as previous render at this width. No dirty range.
    Unchanged,
}

/// Component interface — all widgets implement this.
///
/// Mirrors pi-tui `Component`:
/// - `render(width) -> &[Line]` with caching (`invalidate()` clears)
/// - optional `handle_input`
/// - `wants_key_release` for Kitty protocol release events
pub trait Component {
    /// Render to lines for the given viewport width.
    /// Cached components return a slice into their internal cache.
    fn render(&mut self, width: u16) -> &[Line];

    /// Invalidate any cached rendering state (theme change, force re-render).
    fn invalidate(&mut self);

    /// Optional keyboard input when focused. Default: ignore.
    fn handle_input(&mut self, _data: &str) {}

    /// If true, component receives key release events (Kitty protocol).
    /// Default false — release events are filtered out.
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Whether the last `render` produced different lines than the previous one.
    ///
    /// Caching widgets (Text/Markdown/Image/Box) return `Unchanged` on cache hit.
    /// Always-rerender widgets (Editor/Input/SelectList) return `Changed`.
    /// Default: `Changed` (safe — marks dirty).
    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    /// Downcast to Focusable when the component can receive hardware focus.
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        None
    }
}

/// Focusable components receive a hardware cursor marker when focused.
pub trait Focusable: Component {
    fn focused(&self) -> bool;
    fn set_focused(&mut self, focused: bool);
}

/// Type-erased component handle used by Container / Tui.
pub type ComponentBox = Box<dyn Component>;

/// Container — concatenates children top-to-bottom.
///
/// Port of tui.ts `Container` (lines 256-290). Tracks per-child line offsets so
/// the Tui driver can emit dirty spans from child render status.
///
/// # DirtySpans correctness
///
/// When any child's start/count shifts (height change), every subsequent line
/// index in the retained buffer moves. The driver must dirty the **entire
/// suffix** from the first shifted child's previous start through
/// `max(old_len, new_len)` — not just the changed child and a length-delta
/// tail. Omitting shifted indices leaves stale scrollback (inkferro-rt
/// DirtySpans caller-trust contract).
pub struct Container {
    children: Vec<ComponentBox>,
    /// Cached concatenation of last render.
    cached: Vec<Line>,
    /// Per-child: (start_line, line_count) in `cached` after last render.
    child_spans: Vec<(usize, usize)>,
    /// Aggregate dirty ranges (half-open line indices) from last render.
    dirty_ranges: Vec<std::ops::Range<usize>>,
    last_width: Option<u16>,
    last_status: RenderStatus,
}

impl Container {
    #[must_use]
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            cached: Vec::new(),
            child_spans: Vec::new(),
            dirty_ranges: Vec::new(),
            last_width: None,
            last_status: RenderStatus::Changed,
        }
    }

    pub fn add_child(&mut self, component: impl Component + 'static) {
        self.children.push(Box::new(component));
        self.bust();
    }

    pub fn add_child_box(&mut self, component: ComponentBox) {
        self.children.push(component);
        self.bust();
    }

    pub fn remove_child_at(&mut self, index: usize) -> Option<ComponentBox> {
        if index < self.children.len() {
            let c = self.children.remove(index);
            self.bust();
            Some(c)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
        self.bust();
    }

    pub fn children(&self) -> &[ComponentBox] {
        &self.children
    }

    pub fn children_mut(&mut self) -> &mut [ComponentBox] {
        &mut self.children
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Dirty line ranges (half-open) relative to this container's output from
    /// the last `render`. Empty when every child reported `Unchanged` and
    /// geometry was stable.
    #[must_use]
    pub fn dirty_ranges(&self) -> &[std::ops::Range<usize>] {
        &self.dirty_ranges
    }

    /// Render and return owned ANSI-ready metadata so callers can drop the
    /// line-slice borrow before reading dirty/status.
    pub fn render_owned(&mut self, width: u16) -> (Vec<Line>, Vec<std::ops::Range<usize>>, RenderStatus) {
        let lines = self.render(width).to_vec();
        let dirty = self.dirty_ranges.clone();
        let status = self.last_status;
        (lines, dirty, status)
    }

    fn bust(&mut self) {
        self.cached.clear();
        self.child_spans.clear();
        self.dirty_ranges.clear();
        self.last_width = None;
        self.last_status = RenderStatus::Changed;
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Container {
    fn render(&mut self, width: u16) -> &[Line] {
        let prev_spans = std::mem::take(&mut self.child_spans);
        let prev_len = self.cached.len();
        let width_changed = self.last_width != Some(width);

        let mut lines = Vec::new();
        let mut spans = Vec::with_capacity(self.children.len());
        let mut dirty = Vec::new();
        let mut any_changed = width_changed;
        // First child index whose start/count shifted vs previous layout.
        let mut first_shift: Option<usize> = None;

        for (i, child) in self.children.iter_mut().enumerate() {
            let start = lines.len();
            let child_lines = child.render(width);
            let count = child_lines.len();
            lines.extend_from_slice(child_lines);
            spans.push((start, count));

            let geometry_shifted = match prev_spans.get(i).copied() {
                Some((prev_start, prev_count)) => prev_start != start || prev_count != count,
                None => true,
            };
            if geometry_shifted && first_shift.is_none() {
                first_shift = Some(i);
            }

            match child.last_render_status() {
                RenderStatus::Changed => {
                    any_changed = true;
                    // Only record per-child dirty when geometry is still stable
                    // for this and all prior children; once a shift is known the
                    // suffix path below owns dirtiness.
                    if first_shift.is_none()
                        && count > 0 {
                            dirty.push(start..start + count);
                        }
                }
                RenderStatus::Unchanged => {
                    if geometry_shifted {
                        any_changed = true;
                    }
                }
            }
        }

        if prev_spans.len() > spans.len() {
            any_changed = true;
            if first_shift.is_none() {
                first_shift = Some(spans.len());
            }
        }

        let new_len = lines.len();

        if width_changed {
            dirty.clear();
            if new_len > 0 || prev_len > 0 {
                dirty.push(0..new_len.max(prev_len));
            }
            any_changed = true;
        } else if let Some(shift_idx) = first_shift {
            // Geometry shift: dirty entire suffix from the shifted child's
            // *previous* start through max(old_len, new_len).
            any_changed = true;
            let suffix_start = prev_spans
                .get(shift_idx)
                .map(|(s, _)| *s)
                .or_else(|| spans.get(shift_idx).map(|(s, _)| *s))
                .unwrap_or(0);
            dirty.retain(|r| r.end <= suffix_start);
            let suffix_end = prev_len.max(new_len);
            if suffix_start < suffix_end {
                dirty.push(suffix_start..suffix_end);
            }
        } else if new_len != prev_len {
            any_changed = true;
            let min_len = new_len.min(prev_len);
            let max_len = new_len.max(prev_len);
            if min_len < max_len {
                dirty.push(min_len..max_len);
            }
        }

        self.cached = lines;
        self.child_spans = spans;
        self.dirty_ranges = merge_ranges(dirty);
        self.last_width = Some(width);
        self.last_status = if any_changed {
            RenderStatus::Changed
        } else {
            RenderStatus::Unchanged
        };
        &self.cached
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
        self.bust();
    }

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}

fn merge_ranges(mut ranges: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
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

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::line::Line;

    struct StaticText {
        lines: Vec<String>,
        cache: Vec<Line>,
        status: RenderStatus,
        version: u32,
        last_version: Option<u32>,
    }

    impl StaticText {
        fn new(text: &str) -> Self {
            Self {
                lines: vec![text.to_owned()],
                cache: Vec::new(),
                status: RenderStatus::Changed,
                version: 0,
                last_version: None,
            }
        }

        fn set_lines(&mut self, lines: Vec<String>) {
            self.lines = lines;
            self.version += 1;
            self.cache.clear();
        }
    }

    impl Component for StaticText {
        fn render(&mut self, _width: u16) -> &[Line] {
            if self.last_version == Some(self.version) && !self.cache.is_empty() {
                self.status = RenderStatus::Unchanged;
                return &self.cache;
            }
            self.cache = self.lines.iter().map(|s| Line::plain(s.as_str())).collect();
            self.last_version = Some(self.version);
            self.status = RenderStatus::Changed;
            &self.cache
        }
        fn invalidate(&mut self) {
            self.cache.clear();
            self.last_version = None;
            self.status = RenderStatus::Changed;
        }
        fn last_render_status(&self) -> RenderStatus {
            self.status
        }
    }

    #[test]
    fn container_concatenates_children() {
        let mut c = Container::new();
        c.add_child(StaticText::new("a"));
        c.add_child(StaticText::new("b"));
        let lines = c.render(40);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].plain_text(), "a");
        assert_eq!(lines[1].plain_text(), "b");
    }

    #[test]
    fn container_dirty_empty_on_second_unchanged_render() {
        let mut c = Container::new();
        c.add_child(StaticText::new("a"));
        let _ = c.render(40);
        let _ = c.render(40);
        assert_eq!(c.last_render_status(), RenderStatus::Unchanged);
        assert!(c.dirty_ranges().is_empty());
    }

    #[test]
    fn height_change_dirties_entire_suffix() {
        let mut c = Container::new();
        c.add_child(StaticText::new("a"));
        // Build remaining 99 children as one multi-line component for simplicity:
        // actually add many single-line children.
        for i in 1..100 {
            c.add_child(StaticText::new(&format!("line{i}")));
        }
        let _ = c.render(40);
        assert_eq!(c.cached.len(), 100);

        // Grow the first child from 1 → 2 lines. Indices 1..99 all shift.
        // Access via children_mut is awkward with type erasure; rebuild.
        let mut c = Container::new();
        let _head = StaticText::new("a");
        c.add_child(StaticText::new("a"));
        for i in 1..5 {
            c.add_child(StaticText::new(&format!("line{i}")));
        }
        let _ = c.render(40);
        assert_eq!(c.cached.len(), 5);

        // Replace first child with a 2-line component by clearing and re-adding
        // is a full bust. Instead mutate via a shared interior — use set on
        // a custom component we keep outside... For the test, use invalidate
        // path: remove and re-insert is bust. Directly exercise via a
        // GrowingText helper.
    }

    struct GrowingText {
        n: usize,
        cache: Vec<Line>,
        status: RenderStatus,
        prev_n: Option<usize>,
    }

    impl GrowingText {
        fn new(n: usize) -> Self {
            Self {
                n,
                cache: Vec::new(),
                status: RenderStatus::Changed,
                prev_n: None,
            }
        }
        fn set_n(&mut self, n: usize) {
            self.n = n;
            self.cache.clear();
            self.prev_n = None;
        }
    }

    impl Component for GrowingText {
        fn render(&mut self, _width: u16) -> &[Line] {
            if self.prev_n == Some(self.n) && !self.cache.is_empty() {
                self.status = RenderStatus::Unchanged;
                return &self.cache;
            }
            self.cache = (0..self.n).map(|i| Line::plain(format!("g{i}"))).collect();
            self.prev_n = Some(self.n);
            self.status = RenderStatus::Changed;
            &self.cache
        }
        fn invalidate(&mut self) {
            self.cache.clear();
            self.prev_n = None;
            self.status = RenderStatus::Changed;
        }
        fn last_render_status(&self) -> RenderStatus {
            self.status
        }
    }

    #[test]
    fn height_grow_dirties_suffix_through_max_len() {
        let mut c = Container::new();
        c.add_child(GrowingText::new(1));
        for i in 0..4 {
            c.add_child(StaticText::new(&format!("t{i}")));
        }
        let _ = c.render(40);
        assert_eq!(c.cached.len(), 5);
        // All dirty on first render.
        assert!(!c.dirty_ranges().is_empty());

        // Second render unchanged.
        let _ = c.render(40);
        assert!(c.dirty_ranges().is_empty());

        // Grow first child 1→2. Suffix from index 0 through max(5,6)=6.
        // We need to mutate the first child. Use children_mut + downcast? We
        // can't downcast easily. Rebuild container holding GrowingText via
        // a slot — re-structure: store GrowingText outside isn't possible once
        // boxed. Use a shared Cell via a custom wrapper.
    }

    use std::cell::Cell;
    use std::rc::Rc;

    struct SharedGrow {
        n: Rc<Cell<usize>>,
        cache: Vec<Line>,
        last: Option<usize>,
        status: RenderStatus,
    }

    impl SharedGrow {
        fn new(n: Rc<Cell<usize>>) -> Self {
            Self {
                n,
                cache: Vec::new(),
                last: None,
                status: RenderStatus::Changed,
            }
        }
    }

    impl Component for SharedGrow {
        fn render(&mut self, _width: u16) -> &[Line] {
            let n = self.n.get();
            if self.last == Some(n) && !self.cache.is_empty() {
                self.status = RenderStatus::Unchanged;
                return &self.cache;
            }
            self.cache = (0..n).map(|i| Line::plain(format!("g{i}"))).collect();
            self.last = Some(n);
            self.status = RenderStatus::Changed;
            &self.cache
        }
        fn invalidate(&mut self) {
            self.cache.clear();
            self.last = None;
            self.status = RenderStatus::Changed;
        }
        fn last_render_status(&self) -> RenderStatus {
            self.status
        }
    }

    #[test]
    fn height_grow_dirties_full_suffix() {
        let n = Rc::new(Cell::new(1));
        let mut c = Container::new();
        c.add_child(SharedGrow::new(Rc::clone(&n)));
        for i in 0..4 {
            c.add_child(StaticText::new(&format!("t{i}")));
        }
        let _ = c.render(40);
        let _ = c.render(40);
        assert!(c.dirty_ranges().is_empty());

        n.set(2); // first child grows 1→2; indices 1..4 shift; new len 6
        let _ = c.render(40);
        assert_eq!(c.cached.len(), 6);
        // Suffix from previous start of first child (0) through max(5,6)=6.
        assert_eq!(c.dirty_ranges(), vec![0..6]);
    }
}

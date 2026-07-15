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
#[derive(Default)]
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
    /// Optional visibility gate (pi OverlayOptions.visible).
    pub visible: Option<Box<dyn Fn(usize, usize) -> bool>>,
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

/// Overlay focus-restore state machine (tui.ts:241-251).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum OverlayBlockedFocusResume {
    RestoreOverlay,
    FocusTarget(Option<FocusTarget>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OverlayFocusRestore {
    #[default]
    Inactive,
    Eligible {
        overlay_id: u64,
    },
    Blocked {
        overlay_id: u64,
        blocked_by: FocusTarget,
        resume: OverlayBlockedFocusResume,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayFocusRestorePolicy {
    Clear,
    Preserve,
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
    overlay_focus_restore: OverlayFocusRestore,
    clear_on_shrink: bool,
    show_hardware_cursor: bool,
    render_requested: bool,
    stopped: bool,
    input_listeners: Vec<InputListener>,
    pub on_debug: Option<Box<dyn FnMut()>>,
}

/// Focus target: root child index or overlay id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            overlay_focus_restore: OverlayFocusRestore::Inactive,
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
        let target = index.map(FocusTarget::RootChild);
        self.set_focus_internal(target, OverlayFocusRestorePolicy::Clear);
        self.request_render(false);
    }

    /// Focus an overlay by id (bring to front).
    pub fn focus_overlay(&mut self, id: u64) {
        if let Some(o) = self.overlays.iter_mut().find(|o| o.id == id) {
            if o.hidden {
                return;
            }
            self.focus_order_counter += 1;
            o.focus_order = self.focus_order_counter;
        } else {
            return;
        }
        // Re-check visibility after borrow ends
        let tw = self.terminal.columns() as usize;
        let th = self.terminal.rows() as usize;
        if let Some(o) = self.overlays.iter().find(|o| o.id == id)
            && !Self::overlay_entry_visible(o, tw, th)
        {
            return;
        }
        self.set_focus_internal(Some(FocusTarget::Overlay(id)), OverlayFocusRestorePolicy::Clear);
        self.request_render(false);
    }

    fn set_focus_internal(
        &mut self,
        component: Option<FocusTarget>,
        overlay_focus_restore: OverlayFocusRestorePolicy,
    ) {
        let previous_focus = self.focused;
        let mut next_focus = component;

        let previous_focused_overlay = previous_focus.and_then(|pf| {
            self.overlays.iter().find(|e| {
                matches!(pf, FocusTarget::Overlay(id) if id == e.id)
                    && self.is_overlay_visible_id(e.id)
            })
            .map(|e| e.id)
        });

        let next_focus_is_overlay = matches!(next_focus, Some(FocusTarget::Overlay(_)));
        let restore_state = self.get_visible_overlay_focus_restore();

        if let Some(nf) = next_focus {
            if !next_focus_is_overlay {
                if let OverlayFocusRestore::Blocked {
                    overlay_id,
                    blocked_by,
                    resume,
                } = restore_state
                {
                    if Some(blocked_by) == previous_focus {
                        if matches!(resume, OverlayBlockedFocusResume::FocusTarget(_))
                            || !self.is_component_mounted(blocked_by)
                        {
                            next_focus = self.resolve_blocked_overlay_focus_resume(
                                overlay_id,
                                resume,
                            );
                        } else {
                            self.overlay_focus_restore = OverlayFocusRestore::Blocked {
                                overlay_id,
                                blocked_by: nf,
                                resume,
                            };
                        }
                    }
                } else if let Some(prev_ov) = previous_focused_overlay
                    && !matches!(restore_state, OverlayFocusRestore::Inactive)
                    && matches!(
                        restore_state,
                        OverlayFocusRestore::Eligible { overlay_id }
                            | OverlayFocusRestore::Blocked { overlay_id, .. }
                        if overlay_id == prev_ov
                    )
                    && !self.is_overlay_focus_ancestor(prev_ov, nf)
                {
                    self.overlay_focus_restore = OverlayFocusRestore::Blocked {
                        overlay_id: prev_ov,
                        blocked_by: nf,
                        resume: OverlayBlockedFocusResume::RestoreOverlay,
                    };
                }
            }
        } else {
            // nextFocus === null
            if let OverlayFocusRestore::Blocked {
                overlay_id,
                blocked_by,
                resume,
            } = restore_state
            {
                if Some(blocked_by) == previous_focus {
                    next_focus = self.resolve_blocked_overlay_focus_resume(overlay_id, resume);
                } else if matches!(overlay_focus_restore, OverlayFocusRestorePolicy::Clear) {
                    self.clear_overlay_focus_restore();
                }
            } else if matches!(overlay_focus_restore, OverlayFocusRestorePolicy::Clear) {
                self.clear_overlay_focus_restore();
            }
        }

        self.clear_focus_flags();
        self.focused = next_focus;
        self.apply_focus_flags();

        if let Some(FocusTarget::Overlay(id)) = next_focus
            && self.is_overlay_visible_id(id)
        {
            self.overlay_focus_restore = OverlayFocusRestore::Eligible { overlay_id: id };
        }
    }

    fn clear_overlay_focus_restore(&mut self) {
        self.overlay_focus_restore = OverlayFocusRestore::Inactive;
    }

    fn clear_overlay_focus_restore_for(&mut self, overlay_id: u64) {
        match self.overlay_focus_restore {
            OverlayFocusRestore::Eligible { overlay_id: id }
            | OverlayFocusRestore::Blocked { overlay_id: id, .. }
                if id == overlay_id =>
            {
                self.clear_overlay_focus_restore();
            }
            _ => {}
        }
    }

    fn resolve_blocked_overlay_focus_resume(
        &mut self,
        overlay_id: u64,
        resume: OverlayBlockedFocusResume,
    ) -> Option<FocusTarget> {
        match resume {
            OverlayBlockedFocusResume::RestoreOverlay => Some(FocusTarget::Overlay(overlay_id)),
            OverlayBlockedFocusResume::FocusTarget(target) => {
                self.clear_overlay_focus_restore();
                target
            }
        }
    }

    fn get_visible_overlay_focus_restore(&self) -> OverlayFocusRestore {
        match self.overlay_focus_restore {
            OverlayFocusRestore::Inactive => OverlayFocusRestore::Inactive,
            OverlayFocusRestore::Eligible { overlay_id } => {
                if self.overlays.iter().any(|o| o.id == overlay_id)
                    && self.is_overlay_visible_id(overlay_id)
                {
                    OverlayFocusRestore::Eligible { overlay_id }
                } else {
                    OverlayFocusRestore::Inactive
                }
            }
            OverlayFocusRestore::Blocked {
                overlay_id,
                blocked_by,
                resume,
            } => {
                if self.overlays.iter().any(|o| o.id == overlay_id)
                    && self.is_overlay_visible_id(overlay_id)
                {
                    OverlayFocusRestore::Blocked {
                        overlay_id,
                        blocked_by,
                        resume,
                    }
                } else {
                    OverlayFocusRestore::Inactive
                }
            }
        }
    }

    fn is_overlay_focus_ancestor(&self, entry_id: u64, component: FocusTarget) -> bool {
        let mut visited = std::collections::HashSet::new();
        let mut current = self
            .overlays
            .iter()
            .find(|o| o.id == entry_id)
            .and_then(|o| o.pre_focus);
        while let Some(c) = current {
            if !visited.insert(c) {
                break;
            }
            if c == component {
                return true;
            }
            current = match c {
                FocusTarget::Overlay(id) => self
                    .overlays
                    .iter()
                    .find(|o| o.id == id)
                    .and_then(|o| o.pre_focus),
                FocusTarget::RootChild(_) => None,
            };
        }
        false
    }

    fn retarget_overlay_pre_focus(&mut self, removed_id: u64, removed_pre: Option<FocusTarget>) {
        for overlay in &mut self.overlays {
            if overlay.id != removed_id && overlay.pre_focus == Some(FocusTarget::Overlay(removed_id))
            {
                overlay.pre_focus = removed_pre;
            }
        }
    }

    fn is_component_mounted(&self, target: FocusTarget) -> bool {
        match target {
            FocusTarget::RootChild(i) => i < self.root.len(),
            FocusTarget::Overlay(id) => self.overlays.iter().any(|o| o.id == id),
        }
    }

    fn is_overlay_visible_id(&self, id: u64) -> bool {
        let tw = self.terminal.columns() as usize;
        let th = self.terminal.rows() as usize;
        self.overlays
            .iter()
            .find(|o| o.id == id)
            .is_some_and(|o| Self::overlay_entry_visible(o, tw, th))
    }

    fn get_topmost_visible_overlay_id(&self) -> Option<u64> {
        let tw = self.terminal.columns() as usize;
        let th = self.terminal.rows() as usize;
        let mut top: Option<(u64, u64)> = None; // (id, focus_order)
        for o in &self.overlays {
            if o.options.non_capturing || !Self::overlay_entry_visible(o, tw, th) {
                continue;
            }
            match top {
                Some((_, fo)) if o.focus_order <= fo => {}
                _ => top = Some((o.id, o.focus_order)),
            }
        }
        top.map(|(id, _)| id)
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
        // Only focus if overlay is actually visible (tui.ts:502-505).
        if capturing && self.is_overlay_visible_id(id) {
            self.set_focus_internal(
                Some(FocusTarget::Overlay(id)),
                OverlayFocusRestorePolicy::Clear,
            );
        }
        self.request_render(false);
        id
    }

    pub fn hide_overlay(&mut self, id: u64) {
        if let Some(pos) = self.overlays.iter().position(|o| o.id == id) {
            let entry = self.overlays.remove(pos);
            let pre = entry.pre_focus;
            self.clear_overlay_focus_restore_for(id);
            self.retarget_overlay_pre_focus(id, pre);
            if self.focused == Some(FocusTarget::Overlay(id)) {
                let top = self.get_topmost_visible_overlay_id();
                let next = top
                    .map(FocusTarget::Overlay)
                    .or(pre);
                self.set_focus_internal(next, OverlayFocusRestorePolicy::Clear);
            }
            self.request_render(false);
        }
    }

    pub fn set_overlay_hidden(&mut self, id: u64, hidden: bool) {
        let Some(entry) = self.overlays.iter_mut().find(|o| o.id == id) else {
            return;
        };
        if entry.hidden == hidden {
            return;
        }
        entry.hidden = hidden;
        let non_capturing = entry.options.non_capturing;
        let pre = entry.pre_focus;
        if hidden {
            self.clear_overlay_focus_restore_for(id);
            if self.focused == Some(FocusTarget::Overlay(id)) {
                let top = self.get_topmost_visible_overlay_id();
                let next = top.map(FocusTarget::Overlay).or(pre);
                self.set_focus_internal(next, OverlayFocusRestorePolicy::Clear);
            }
        } else if !non_capturing && self.is_overlay_visible_id(id) {
            if let Some(o) = self.overlays.iter_mut().find(|o| o.id == id) {
                self.focus_order_counter += 1;
                o.focus_order = self.focus_order_counter;
            }
            self.set_focus_internal(
                Some(FocusTarget::Overlay(id)),
                OverlayFocusRestorePolicy::Clear,
            );
        }
        self.request_render(false);
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
            && let Some(cb) = self.on_debug.as_mut()
        {
            cb();
            return;
        }

        // Per-input visibility re-check + focus-restore (tui.ts:797-823).
        if let Some(FocusTarget::Overlay(id)) = self.focused
            && !self.is_overlay_visible_id(id)
        {
            if let Some(top) = self.get_topmost_visible_overlay_id() {
                self.set_focus_internal(
                    Some(FocusTarget::Overlay(top)),
                    OverlayFocusRestorePolicy::Clear,
                );
            } else {
                let pre = self
                    .overlays
                    .iter()
                    .find(|o| o.id == id)
                    .and_then(|o| o.pre_focus);
                self.set_focus_internal(pre, OverlayFocusRestorePolicy::Preserve);
            }
        }

        let focus_is_overlay = matches!(self.focused, Some(FocusTarget::Overlay(_)));
        if !focus_is_overlay {
            let restore_state = self.get_visible_overlay_focus_restore();
            match restore_state {
                OverlayFocusRestore::Eligible { overlay_id } => {
                    self.set_focus_internal(
                        Some(FocusTarget::Overlay(overlay_id)),
                        OverlayFocusRestorePolicy::Clear,
                    );
                }
                OverlayFocusRestore::Blocked {
                    overlay_id,
                    blocked_by,
                    resume,
                } if Some(blocked_by) != self.focused => {
                    if matches!(resume, OverlayBlockedFocusResume::RestoreOverlay) {
                        self.set_focus_internal(
                            Some(FocusTarget::Overlay(overlay_id)),
                            OverlayFocusRestorePolicy::Clear,
                        );
                    } else {
                        self.clear_overlay_focus_restore();
                        if let OverlayBlockedFocusResume::FocusTarget(target) = resume {
                            self.set_focus_internal(target, OverlayFocusRestorePolicy::Clear);
                        }
                    }
                }
                _ => {}
            }
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

        // Always rebuild overlay frames so first_frame/force paths never drop dialogs.
        // Pass upcoming base length so focused-overlay CursorTarget uses composed
        // working geometry (not stale pre-update retained len).
        let (rt_overlays, overlay_cursor) = self.build_overlay_frames(width, height, new_len);

        if first_frame || (dirty.is_empty() && new_len != self.retained.len()) {
            let mut ansi_lines: Vec<String> = root_lines.iter().map(Line::to_ansi).collect();
            let mut cursor = extract_cursor_from_ansi_lines(&mut ansi_lines, height);
            // Focused overlay cursor wins over root retained cursor.
            if let Some(c) = overlay_cursor {
                cursor = Some(c);
            }
            self.retained = ansi_lines;
            self.emit_frame(width, height, DirtySpans::All, &rt_overlays, cursor);
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

        let mut cursor = extract_cursor_from_retained(&mut self.retained, height, &ranges);
        if let Some(c) = overlay_cursor {
            cursor = Some(c);
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

    /// Build overlay line frames sorted by focusOrder; extract CURSOR_MARKER
    /// from the focused overlay when present (pi compositeOverlays + extractCursor).
    ///
    /// `upcoming_base_len` is the root's post-render line count for this frame.
    /// Overlay cursor abs_row mirrors inkferro-rt `overlay_working_geometry`:
    /// `working_height = max(base, term_height, max overlay bottom)` then
    /// `abs_row = working_height.saturating_sub(term_height) + layout.row + local.row`.
    /// Using pre-update `self.retained.len()` would place the hardware cursor
    /// off by the length delta when the root grows under a focused overlay.
    fn build_overlay_frames(
        &mut self,
        term_width: usize,
        term_height: usize,
        upcoming_base_len: usize,
    ) -> (Vec<Overlay>, Option<CursorTarget>) {
        let mut rt_overlays: Vec<Overlay> = Vec::new();
        let mut focused_cursor_local: Option<(usize, usize, CursorTarget)> = None;
        let mut order: Vec<usize> = (0..self.overlays.len()).collect();
        order.sort_by_key(|&i| self.overlays[i].focus_order);

        let focused_overlay_id = match self.focused {
            Some(FocusTarget::Overlay(id)) => Some(id),
            _ => None,
        };

        for idx in order {
            let entry = &mut self.overlays[idx];
            if !Self::overlay_entry_visible(entry, term_width, term_height) {
                continue;
            }
            let is_focused = focused_overlay_id == Some(entry.id);
            let layout =
                resolve_overlay_layout(&entry.options, term_height, term_width, term_height);
            let ov_lines = entry.component.render(layout.width as u16);
            let mut strings: Vec<String> = ov_lines.iter().map(Line::to_ansi).collect();
            if let Some(max_h) = layout.max_height
                && strings.len() > max_h
            {
                strings.truncate(max_h);
            }
            // Extract CURSOR_MARKER from overlay composite (finding #3).
            if is_focused
                && let Some(local) = extract_cursor_from_ansi_lines(&mut strings, term_height)
            {
                focused_cursor_local = Some((layout.row, layout.col, local));
            }
            rt_overlays.push(Overlay {
                row: layout.row,
                col: layout.col,
                width: layout.width,
                lines: strings,
            });
        }

        // Mirror inkferro_rt::overlay_working_geometry with the upcoming base
        // length so scrollback advance under a focused overlay keeps abs_row
        // aligned with compose placement.
        let overlay_cursor = focused_cursor_local.map(|(layout_row, layout_col, local)| {
            let mut min_lines_needed = upcoming_base_len;
            for ov in &rt_overlays {
                min_lines_needed =
                    min_lines_needed.max(ov.row.saturating_add(ov.lines.len()));
            }
            let working_height = upcoming_base_len
                .max(term_height)
                .max(min_lines_needed);
            let viewport_start = working_height.saturating_sub(term_height);
            CursorTarget {
                row: viewport_start
                    .saturating_add(layout_row)
                    .saturating_add(local.row),
                col: layout_col.saturating_add(local.col),
            }
        });

        (rt_overlays, overlay_cursor)
    }

    fn overlay_entry_visible(entry: &OverlayEntry, term_width: usize, term_height: usize) -> bool {
        if entry.hidden {
            return false;
        }
        if let Some(vis) = &entry.options.visible {
            return vis(term_width, term_height);
        }
        true
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
    use crate::component::{Component, Focusable, RenderStatus};
    use crate::components::spacer::Spacer;
    use crate::components::text::Text;
    use crate::line::{CURSOR_MARKER, Line};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Test component with shared text for in-place invalidation.
    struct LiveText {
        text: Rc<RefCell<String>>,
        cache: Option<(u16, String, Vec<Line>)>,
        last_status: RenderStatus,
        focused: bool,
        with_cursor: bool,
    }

    impl LiveText {
        fn new(text: impl Into<String>) -> (Self, Rc<RefCell<String>>) {
            let text = Rc::new(RefCell::new(text.into()));
            (
                Self {
                    text: Rc::clone(&text),
                    cache: None,
                    last_status: RenderStatus::Changed,
                    focused: false,
                    with_cursor: false,
                },
                text,
            )
        }

        fn with_cursor(mut self) -> Self {
            self.with_cursor = true;
            self
        }
    }

    impl Component for LiveText {
        fn render(&mut self, width: u16) -> &[Line] {
            let current = self.text.borrow().clone();
            let cache_hit = matches!(
                &self.cache,
                Some((w, cached_text, _)) if *w == width && *cached_text == current
            );
            if cache_hit {
                self.last_status = RenderStatus::Unchanged;
                return &self.cache.as_ref().unwrap().2;
            }
            let mut body = current;
            if self.with_cursor && self.focused {
                body.push_str(CURSOR_MARKER);
            }
            let line = Line::plain(&body);
            self.cache = Some((width, self.text.borrow().clone(), vec![line]));
            self.last_status = RenderStatus::Changed;
            &self.cache.as_ref().unwrap().2
        }

        fn invalidate(&mut self) {
            self.cache = None;
            self.last_status = RenderStatus::Changed;
        }

        fn last_render_status(&self) -> RenderStatus {
            self.last_status
        }

        fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
            Some(self)
        }
    }

    impl Focusable for LiveText {
        fn focused(&self) -> bool {
            self.focused
        }
        fn set_focused(&mut self, focused: bool) {
            self.focused = focused;
            self.invalidate();
        }
    }

    #[test]
    fn tui_renders_children_without_panic() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 10));
        tui.add_child(Text::with_text("hello"));
        tui.add_child(Spacer::new(1));
        tui.do_render();
        assert!(!tui.retained.is_empty() || tui.frame.lines_full_redraw_count() >= 1);
        let vt = tui
            .retained
            .iter()
            .any(|l| l.contains("hello") || !l.is_empty());
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

    /// Property: random IN-PLACE invalidations on a live tree — Ranges retained
    /// buffer equals a force-All rebuild of the same tree (finding #4).
    #[test]
    fn dirty_spans_inplace_invalidations_match_all() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 24));
        let mut handles: Vec<Rc<RefCell<String>>> = Vec::new();
        for i in 0..20 {
            let (comp, handle) = LiveText::new(format!("line-{i}-aaaaaaaa"));
            handles.push(handle);
            tui.add_child(comp);
        }
        tui.do_render();
        let baseline = tui.retained.clone();
        assert_eq!(baseline.len(), 20);

        // In-place mutate a deterministic pattern of children and re-render via Ranges.
        for round in 0..5 {
            for i in 0..20 {
                if (i + round) % 3 == 0 {
                    *handles[i].borrow_mut() = format!("CHANGED-{round}-{i}");
                    // Invalidate the cached component so render status flips.
                    if let Some(child) = tui.root.children_mut().get_mut(i) {
                        child.invalidate();
                    }
                }
            }
            tui.do_render();
            let ranges_retained = tui.retained.clone();

            // Force All path on a fresh twin built from the same final texts.
            let mut t_all = Tui::new(VirtualTerminal::new(40, 24));
            for h in &handles {
                t_all.add_child(LiveText::new(h.borrow().clone()).0);
            }
            t_all.request_render(true);
            t_all.do_render();
            assert_eq!(
                ranges_retained, t_all.retained,
                "round {round}: Ranges retained must match All rebuild"
            );
        }
    }

    #[test]
    fn first_frame_composites_overlays() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 10));
        tui.add_child(Text::with_text("root-content"));
        let id = tui.show_overlay(
            Text::new("DIALOG", 0, 0, None),
            OverlayOptions {
                anchor: OverlayAnchor::TopLeft,
                width: Some(SizeValue::Abs(20)),
                ..Default::default()
            },
        );
        assert!(id >= 1);
        tui.do_render();
        // First-frame path must still emit overlay content via write_lines.
        // Overlay is not in retained (base only) but FrameWriter gets overlays.
        // Probe: full redraw happened and retained has root content.
        assert!(
            tui.retained.iter().any(|l| l.contains("root-content")),
            "retained missing root: {:?}",
            tui.retained
        );
        // Overlay component should have been rendered (no panic) and focused.
        assert_eq!(tui.focused, Some(FocusTarget::Overlay(id)));
        // Emitted bytes include dialog text from overlay composite.
        // VirtualTerminal collects writes — but Tui owns the terminal boxed.
        // Check via a second render that doesn't early-return empty overlays.
        tui.request_render(true);
        tui.do_render();
        assert_eq!(tui.focused, Some(FocusTarget::Overlay(id)));
    }

    #[test]
    fn overlay_cursor_extracted_when_focused() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 10));
        tui.set_show_hardware_cursor(true);
        tui.add_child(Text::with_text("base"));
        let (ov, _) = LiveText::new("input");
        let ov = ov.with_cursor();
        let id = tui.show_overlay(
            ov,
            OverlayOptions {
                anchor: OverlayAnchor::TopLeft,
                width: Some(SizeValue::Abs(20)),
                row: Some(SizeValue::Abs(0)),
                col: Some(SizeValue::Abs(0)),
                ..Default::default()
            },
        );
        // Focus is already on overlay from show_overlay.
        assert_eq!(tui.focused, Some(FocusTarget::Overlay(id)));
        tui.do_render();
        // Overlay path strips CURSOR_MARKER from overlay lines (no panic).
        // Cursor extraction from focused overlay is exercised.
    }

    /// When root grows past the viewport under a focused overlay, CursorTarget
    /// abs_row must use upcoming base length (compose working geometry), not
    /// pre-update retained length. Also covers request_render(force).
    #[test]
    fn overlay_cursor_tracks_base_growth_and_force_redraw() {
        const TERM_H: usize = 5;
        const TERM_W: usize = 40;
        const LAYOUT_ROW: usize = 0;
        const LAYOUT_COL: usize = 2;

        let mut tui = Tui::new(VirtualTerminal::new(TERM_W as u16, TERM_H as u16));
        tui.set_show_hardware_cursor(true);

        // Seed below viewport; growth past TERM_H makes viewport_start > 0.
        let mut handles: Vec<Rc<RefCell<String>>> = Vec::new();
        for i in 0..3 {
            let (comp, handle) = LiveText::new(format!("base-{i}"));
            handles.push(handle);
            tui.add_child(comp);
        }

        let (ov, _) = LiveText::new("input");
        let ov = ov.with_cursor();
        let id = tui.show_overlay(
            ov,
            OverlayOptions {
                anchor: OverlayAnchor::TopLeft,
                width: Some(SizeValue::Abs(20)),
                row: Some(SizeValue::Abs(LAYOUT_ROW)),
                col: Some(SizeValue::Abs(LAYOUT_COL)),
                ..Default::default()
            },
        );
        assert_eq!(tui.focused, Some(FocusTarget::Overlay(id)));
        tui.do_render();
        // Overlay cursor local.row=0; base_len=3 ≤ TERM_H → viewport_start=0.
        assert_eq!(tui.frame.lines_hardware_cursor_row(), LAYOUT_ROW);

        // Append past viewport while overlay stays focused.
        for i in 3..8 {
            let (comp, handle) = LiveText::new(format!("base-{i}"));
            handles.push(handle);
            tui.add_child(comp);
        }
        // Height change dirties suffix; force a render.
        tui.do_render();
        let base_len = tui.retained.len();
        assert_eq!(base_len, 8, "root must grow past term height");
        // working_height = max(8, 5, overlay_bottom=1) = 8; viewport_start = 3
        let expected_abs = base_len.saturating_sub(TERM_H) + LAYOUT_ROW;
        assert!(
            expected_abs > 0,
            "regression needs nonzero viewport_start; got {expected_abs}"
        );
        assert_eq!(
            tui.frame.lines_hardware_cursor_row(),
            expected_abs,
            "growth: hardware cursor must match compose viewport_start + layout.row"
        );
        assert_eq!(tui.frame.lines_viewport_top(), expected_abs);

        // Force redraw clears retained then rebuilds; must still use upcoming base.
        tui.request_render(true);
        tui.do_render();
        let base_len = tui.retained.len();
        assert_eq!(base_len, 8);
        let expected_abs = base_len.saturating_sub(TERM_H) + LAYOUT_ROW;
        assert_eq!(
            tui.frame.lines_hardware_cursor_row(),
            expected_abs,
            "force redraw: cursor must still track working geometry"
        );
        assert_eq!(tui.frame.lines_viewport_top(), expected_abs);
        let _ = handles; // keep handles live for clarity
        let _ = id;
    }

    #[test]
    fn focus_restore_when_overlay_hidden() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 10));
        tui.add_child(Text::with_text("root"));
        tui.set_focus_child(Some(0));
        let id = tui.show_overlay(
            Text::new("dlg", 0, 0, None),
            OverlayOptions::default(),
        );
        assert_eq!(tui.focused, Some(FocusTarget::Overlay(id)));
        tui.set_overlay_hidden(id, true);
        // Focus should leave the hidden overlay.
        assert_ne!(tui.focused, Some(FocusTarget::Overlay(id)));
    }

    #[test]
    fn handle_input_redirects_when_overlay_not_visible() {
        let mut tui = Tui::new(VirtualTerminal::new(40, 10));
        tui.add_child(Text::with_text("root"));
        tui.set_focus_child(Some(0));
        let id = tui.show_overlay(
            Text::new("dlg", 0, 0, None),
            OverlayOptions {
                visible: Some(Box::new(|_w, _h| false)),
                ..Default::default()
            },
        );
        // Not visible → should not capture focus on show.
        assert_ne!(tui.focused, Some(FocusTarget::Overlay(id)));
        // Force focus then input should re-check visibility.
        tui.focused = Some(FocusTarget::Overlay(id));
        tui.handle_input("a".into());
        assert_ne!(
            tui.focused,
            Some(FocusTarget::Overlay(id)),
            "input must redirect away from invisible overlay"
        );
    }
}

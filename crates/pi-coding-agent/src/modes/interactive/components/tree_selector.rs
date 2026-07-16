//! Session tree selector for navigation.
//!
//! Port of `modes/interactive/components/tree-selector.ts`.
//!
//! Deviations from the oracle (see slice report):
//! - `sliceByColumn` / grapheme segmentation are ported privately because
//!   pi-tui does not export them; zero-width chars merge into the previous
//!   segment (approximation of `Intl.Segmenter`).
//! - The oracle's `setTimeout(onCancel, 100)` for an empty tree is modelled
//!   as a deadline fired on the next render/input after 100ms (components
//!   have no timer loop).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use pi_tui::components::input::{utf16_len, utf16_to_byte};
use pi_tui::components::{Input, Text};
use pi_tui::keybindings::get_keybindings;
use pi_tui::util::{
    extract_ansi_code, grapheme_width, truncate_to_width, visible_width, wrap_text_with_ansi,
};
use pi_tui::{Component, Focusable, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{format_key_text, key_hint};
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, theme};
use crate::session_manager::SessionTreeNode;
use crate::session_types::SessionEntry;

/// Gutter info: position (displayIndent where connector was) and whether to show │.
#[derive(Clone, Copy, Debug)]
struct GutterInfo {
    /// displayIndent level where the connector was shown.
    position: usize,
    /// true = show │, false = show spaces.
    show: bool,
}

/// Arena node: one session entry plus tree wiring.
struct ArenaNode {
    entry: SessionEntry,
    label: Option<String>,
    label_timestamp: Option<String>,
    children: Vec<usize>,
}

impl ArenaNode {
    fn id(&self) -> &str {
        self.entry.id().unwrap_or("")
    }

    fn parent_id(&self) -> Option<&str> {
        self.entry.parent_id().as_option().map(String::as_str)
    }
}

/// Flattened tree node for navigation.
struct FlatNode {
    arena_idx: usize,
    /// Indentation level (each level = 3 chars).
    indent: usize,
    /// Whether to show connector (├─ or └─) - true if parent has multiple children.
    show_connector: bool,
    /// If showConnector, true = last sibling (└─), false = not last (├─).
    is_last: bool,
    /// Gutter info for each ancestor branch point.
    gutters: Vec<GutterInfo>,
    /// True if this node is a root under a virtual branching root (multiple roots).
    is_virtual_root_child: bool,
}

struct HorizontalViewportRow {
    gutter: String,
    body: String,
    anchor_col: usize,
    body_width: usize,
    is_selected: bool,
}

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

/// Split into char clusters, merging zero-width chars into the previous
/// segment (approximation of `Intl.Segmenter` grapheme clusters).
fn grapheme_like_segments(s: &str) -> Vec<&str> {
    let mut result: Vec<&str> = Vec::new();
    let mut seg_start = 0usize;
    for (idx, ch) in s.char_indices() {
        if idx == 0 {
            continue;
        }
        let ch_str = &s[idx..idx + ch.len_utf8()];
        if grapheme_width(ch_str) > 0 {
            result.push(&s[seg_start..idx]);
            seg_start = idx;
        }
    }
    if seg_start < s.len() {
        result.push(&s[seg_start..]);
    }
    result
}

/// Port of pi-tui `sliceByColumn` (utils.ts) — ANSI-aware column slice.
fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    if length == 0 {
        return String::new();
    }
    let end_col = start_col + length;
    let mut result = String::new();
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi = String::new();

    while i < line.len() {
        if let Some((code, len)) = extract_ansi_code(line, i) {
            if current_col >= start_col && current_col < end_col {
                result.push_str(code);
            } else if current_col < start_col {
                pending_ansi.push_str(code);
            }
            i += len;
            continue;
        }

        let mut text_end = i;
        while text_end < line.len() && extract_ansi_code(line, text_end).is_none() {
            text_end += 1;
        }

        for segment in grapheme_like_segments(&line[i..text_end]) {
            let w = grapheme_width(segment);
            let in_range = current_col >= start_col && current_col < end_col;
            let fits = !strict || current_col + w <= end_col;
            if in_range && fits {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(segment);
            }
            current_col += w;
            if current_col >= end_col {
                break;
            }
        }
        i = text_end;
        if current_col >= end_col {
            break;
        }
    }
    result
}

/// Truncate with `"..."` suffix (TS `truncateToWidth(text, width)` default).
fn truncate_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width <= 3 {
        let clipped = truncate_to_width("...", max_width);
        return format!("\x1b[0m{clipped}");
    }
    let prefix = truncate_to_width(text, max_width - 3);
    format!("{prefix}...\x1b[0m")
}

/// JS `s.slice(0, n)` (UTF-16 units).
fn slice_utf16_prefix(s: &str, n: usize) -> &str {
    &s[..utf16_to_byte(s, n)]
}

/// Render tree rows into a horizontally clipped viewport.
///
/// The tree gutter is always kept visible. The row bodies are shifted left only
/// when the selected row's anchor (the start of its entry text after tree
/// indentation/markers) would otherwise be too far right to see useful content.
fn render_horizontal_viewport(rows: &[HorizontalViewportRow], width: usize) -> Vec<String> {
    let viewport_width = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body_width = rows.iter().map(|row| row.body_width).max().unwrap_or(0);
    let max_horizontal_scroll = max_body_width.saturating_sub(viewport_width);
    let selected_row = rows.iter().find(|row| row.is_selected);

    // Only pan horizontally when needed to keep enough selected-row content visible after its anchor.
    let mut horizontal_scroll = 0usize;
    if let Some(selected_row) = selected_row
        && max_horizontal_scroll > 0
    {
        let min_visible_anchor_content_width = MAX_VISIBLE_ANCHOR_CONTENT_WIDTH
            .min(MIN_VISIBLE_ANCHOR_CONTENT_WIDTH.max(viewport_width / 3));
        if selected_row.anchor_col > viewport_width.saturating_sub(min_visible_anchor_content_width)
        {
            let anchor_context_width =
                MAX_ANCHOR_CONTEXT_WIDTH.min(MIN_ANCHOR_CONTEXT_WIDTH.max(viewport_width / 4));
            horizontal_scroll = max_horizontal_scroll
                .min(selected_row.anchor_col.saturating_sub(anchor_context_width));
        }
    }

    // Clip only the body; the fixed-width gutter remains visible as navigation context.
    rows.iter()
        .map(|row| {
            let line = if horizontal_scroll > 0 {
                format!(
                    "{}{}\x1b[0m",
                    row.gutter,
                    slice_by_column(&row.body, horizontal_scroll, viewport_width, true)
                )
            } else {
                format!("{}{}", row.gutter, row.body)
            };
            truncate_to_width(&line, width)
        })
        .collect()
}

/// Filter mode for tree display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilterMode {
    #[default]
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl FilterMode {
    const CYCLE: [FilterMode; 5] = [
        FilterMode::Default,
        FilterMode::NoTools,
        FilterMode::UserOnly,
        FilterMode::LabeledOnly,
        FilterMode::All,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FilterMode::Default => "default",
            FilterMode::NoTools => "no-tools",
            FilterMode::UserOnly => "user-only",
            FilterMode::LabeledOnly => "labeled-only",
            FilterMode::All => "all",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<FilterMode> {
        Some(match value {
            "default" => FilterMode::Default,
            "no-tools" => FilterMode::NoTools,
            "user-only" => FilterMode::UserOnly,
            "labeled-only" => FilterMode::LabeledOnly,
            "all" => FilterMode::All,
            _ => return None,
        })
    }
}

/// Tool call info for lookup.
struct ToolCallInfo {
    name: String,
    arguments: Value,
}

/// Boxed entry-id callback (clippy `type_complexity`).
type IdCallback = Box<dyn FnMut(&str)>;
/// `(entry_id, label)` callback for label edits/changes.
type LabelCallback = Box<dyn FnMut(&str, Option<&str>)>;
/// Cancel callback shared between the tree list and the empty-tree timer.
type SharedCancel = Rc<RefCell<Box<dyn FnMut()>>>;
/// `(entry_id, current_label)` slot written by the tree list.
type LabelEditRequest = Rc<RefCell<Option<(String, Option<String>)>>>;

/// Tree list component with selection and ASCII art visualization.
pub struct TreeList {
    arena: Vec<ArenaNode>,
    flat_nodes: Vec<FlatNode>,
    /// Indices into `flat_nodes`.
    filtered_nodes: Vec<usize>,
    selected_index: usize,
    current_leaf_id: Option<String>,
    max_visible_lines: usize,
    filter_mode: FilterMode,
    search_query: String,
    tool_call_map: HashMap<String, ToolCallInfo>,
    multiple_roots: bool,
    show_label_timestamps: bool,
    active_path_ids: HashSet<String>,
    visible_parent_map: HashMap<String, Option<String>>,
    visible_children_map: HashMap<Option<String>, Vec<String>>,
    last_selected_id: Option<String>,
    folded_nodes: HashSet<String>,

    pub on_select: Option<IdCallback>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_copy: Option<Box<dyn FnMut(Option<String>)>>,
    pub on_label_edit: Option<LabelCallback>,

    cached: Vec<Line>,
}

impl TreeList {
    #[must_use]
    pub fn new(
        tree: Vec<SessionTreeNode>,
        current_leaf_id: Option<&str>,
        max_visible_lines: usize,
        initial_selected_id: Option<&str>,
        initial_filter_mode: Option<FilterMode>,
    ) -> Self {
        let mut list = Self {
            arena: Vec::new(),
            flat_nodes: Vec::new(),
            filtered_nodes: Vec::new(),
            selected_index: 0,
            current_leaf_id: current_leaf_id.map(str::to_owned),
            max_visible_lines,
            filter_mode: initial_filter_mode.unwrap_or_default(),
            search_query: String::new(),
            tool_call_map: HashMap::new(),
            multiple_roots: tree.len() > 1,
            show_label_timestamps: false,
            active_path_ids: HashSet::new(),
            visible_parent_map: HashMap::new(),
            visible_children_map: HashMap::new(),
            last_selected_id: None,
            folded_nodes: HashSet::new(),
            on_select: None,
            on_cancel: None,
            on_copy: None,
            on_label_edit: None,
            cached: Vec::new(),
        };
        let roots = list.build_arena(tree);
        list.flatten_tree(&roots);
        list.build_active_path();
        list.apply_filter();

        // Start with initialSelectedId if provided, otherwise current leaf.
        let target_id = initial_selected_id
            .map(str::to_owned)
            .or_else(|| list.current_leaf_id.clone());
        list.selected_index = list.find_nearest_visible_index(target_id.as_deref());
        list.last_selected_id = list
            .filtered_nodes
            .get(list.selected_index)
            .map(|&f| list.node_id(f).to_owned());
        list
    }

    /// Consume the owned tree into an arena (iterative — no recursion).
    fn build_arena(&mut self, tree: Vec<SessionTreeNode>) -> Vec<usize> {
        let mut roots = Vec::with_capacity(tree.len());
        // Stack of (node, parent arena index). Children of one parent are
        // pushed in reverse so they are created in original order.
        let mut stack: Vec<(SessionTreeNode, Option<usize>)> = Vec::new();
        for node in tree.into_iter().rev() {
            stack.push((node, None));
        }
        while let Some((node, parent)) = stack.pop() {
            let SessionTreeNode {
                entry,
                children,
                label,
                label_timestamp,
            } = node;
            let idx = self.arena.len();
            self.arena.push(ArenaNode {
                entry,
                label,
                label_timestamp,
                children: Vec::new(),
            });
            match parent {
                Some(parent_idx) => self.arena[parent_idx].children.push(idx),
                None => roots.push(idx),
            }
            for child in children.into_iter().rev() {
                stack.push((child, Some(idx)));
            }
        }
        roots
    }

    fn node_id(&self, flat_idx: usize) -> &str {
        self.arena[self.flat_nodes[flat_idx].arena_idx].id()
    }

    /// id → arena index for parent-chain walks.
    fn arena_index_by_id(&self) -> HashMap<&str, usize> {
        self.arena
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.id(), idx))
            .collect()
    }

    /// Find the index of the nearest visible entry, walking up the parent chain if needed.
    /// Returns the index in `filtered_nodes`, or the last index as fallback.
    fn find_nearest_visible_index(&self, entry_id: Option<&str>) -> usize {
        if self.filtered_nodes.is_empty() {
            return 0;
        }

        let arena_by_id = self.arena_index_by_id();
        let visible_id_to_index: HashMap<&str, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(i, &f)| (self.node_id(f), i))
            .collect();

        // Walk from entryId up to root, looking for a visible entry.
        let mut current_id = entry_id;
        while let Some(id) = current_id {
            if let Some(&index) = visible_id_to_index.get(id) {
                return index;
            }
            let Some(&arena_idx) = arena_by_id.get(id) else {
                break;
            };
            current_id = self.arena[arena_idx].parent_id();
        }

        // Fallback: last visible entry.
        self.filtered_nodes.len() - 1
    }

    /// Build the set of entry IDs on the path from root to current leaf.
    fn build_active_path(&mut self) {
        self.active_path_ids.clear();
        let Some(leaf_id) = self.current_leaf_id.clone() else {
            return;
        };

        let arena_by_id = self.arena_index_by_id();
        let mut path: Vec<String> = Vec::new();
        // Walk from leaf to root.
        let mut current_id: Option<&str> = Some(&leaf_id);
        while let Some(id) = current_id {
            path.push(id.to_owned());
            let Some(&arena_idx) = arena_by_id.get(id) else {
                break;
            };
            current_id = self.arena[arena_idx].parent_id();
        }
        self.active_path_ids = path.into_iter().collect();
    }

    fn flatten_tree(&mut self, roots: &[usize]) {
        self.flat_nodes.clear();
        self.tool_call_map.clear();

        // Indentation rules:
        // - At indent 0: stay at 0 unless parent has >1 children (then +1)
        // - At indent 1: children always go to indent 2 (visual grouping of subtree)
        // - At indent 2+: stay flat for single-child chains, +1 only if parent branches

        // Determine which subtrees contain the active leaf (to sort current branch first).
        // Arena insertion order is pre-order, so a reverse pass is post-order.
        let leaf_id = self.current_leaf_id.as_deref();
        let mut contains_active = vec![false; self.arena.len()];
        for idx in (0..self.arena.len()).rev() {
            let node = &self.arena[idx];
            let mut has = leaf_id.is_some_and(|leaf| node.id() == leaf);
            for &child in &node.children {
                if contains_active[child] {
                    has = true;
                }
            }
            contains_active[idx] = has;
        }

        // Add roots in reverse order, prioritizing the one containing the active leaf.
        // If multiple roots, treat them as children of a virtual root that branches.
        let multiple_roots = roots.len() > 1;
        let mut ordered_roots = roots.to_vec();
        ordered_roots.sort_by_key(|&idx| !contains_active[idx]);

        // Stack items: (arena_idx, indent, just_branched, show_connector, is_last, gutters, is_virtual_root_child)
        type StackItem = (usize, usize, bool, bool, bool, Vec<GutterInfo>, bool);
        let mut stack: Vec<StackItem> = Vec::new();
        for (i, &root) in ordered_roots.iter().enumerate().rev() {
            let is_last = i == ordered_roots.len() - 1;
            stack.push((
                root,
                usize::from(multiple_roots),
                multiple_roots,
                multiple_roots,
                is_last,
                Vec::new(),
                multiple_roots,
            ));
        }

        while let Some((
            arena_idx,
            indent,
            just_branched,
            show_connector,
            is_last,
            gutters,
            is_virtual_root_child,
        )) = stack.pop()
        {
            // Extract tool calls from assistant messages for later lookup.
            if let SessionEntry::Message { message, .. } = &self.arena[arena_idx].entry
                && message.get("role").and_then(Value::as_str) == Some("assistant")
                && let Some(content) = message.get("content").and_then(Value::as_array)
            {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("toolCall")
                        && let (Some(id), Some(name)) = (
                            block.get("id").and_then(Value::as_str),
                            block.get("name").and_then(Value::as_str),
                        )
                    {
                        self.tool_call_map.insert(
                            id.to_owned(),
                            ToolCallInfo {
                                name: name.to_owned(),
                                arguments: block
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(Value::Object(serde_json::Map::new())),
                            },
                        );
                    }
                }
            }

            self.flat_nodes.push(FlatNode {
                arena_idx,
                indent,
                show_connector,
                is_last,
                gutters: gutters.clone(),
                is_virtual_root_child,
            });

            let children = self.arena[arena_idx].children.clone();
            let multiple_children = children.len() > 1;

            // Order children so the branch containing the active leaf comes first.
            let mut ordered_children: Vec<usize> = Vec::with_capacity(children.len());
            let mut rest: Vec<usize> = Vec::new();
            for child in children {
                if contains_active[child] {
                    ordered_children.push(child);
                } else {
                    rest.push(child);
                }
            }
            ordered_children.extend(rest);

            // Calculate child indent.
            let child_indent = if multiple_children {
                // Parent branches: children get +1
                indent + 1
            } else if just_branched && indent > 0 {
                // First generation after a branch: +1 for visual grouping
                indent + 1
            } else {
                // Single-child chain: stay flat
                indent
            };

            // Build gutters for children.
            // If this node showed a connector, add a gutter entry for descendants.
            // Only add gutter if connector is actually displayed (not suppressed for virtual root children).
            let connector_displayed = show_connector && !is_virtual_root_child;
            // When connector is displayed, add a gutter entry at the connector's position.
            // Connector is at position (displayIndent - 1), so gutter should be there too.
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let child_gutters: Vec<GutterInfo> = if connector_displayed {
                let mut g = gutters;
                g.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
                g
            } else {
                gutters
            };

            // Add children in reverse order.
            for (i, &child) in ordered_children.iter().enumerate().rev() {
                let child_is_last = i == ordered_children.len() - 1;
                stack.push((
                    child,
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }
    }

    fn apply_filter(&mut self) {
        // Update lastSelectedId only when we have a valid selection (non-empty list).
        // This preserves the selection when switching through empty filter results.
        if !self.filtered_nodes.is_empty() {
            self.last_selected_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&f| self.node_id(f).to_owned())
                .or_else(|| self.last_selected_id.clone());
        }

        let lower_query = self.search_query.to_lowercase();
        let search_tokens: Vec<&str> = lower_query.split_whitespace().collect();

        let mut filtered: Vec<usize> = Vec::new();
        for (flat_idx, flat_node) in self.flat_nodes.iter().enumerate() {
            let node = &self.arena[flat_node.arena_idx];
            let entry = &node.entry;
            let is_current_leaf = self.current_leaf_id.as_deref() == Some(node.id());

            // Skip assistant messages with only tool calls (no text) unless error/aborted.
            // Always show current leaf so active position is visible.
            if let SessionEntry::Message { message, .. } = entry
                && message.get("role").and_then(Value::as_str) == Some("assistant")
                && !is_current_leaf
            {
                let has_text = has_text_content(message.get("content"));
                let is_error_or_aborted = message
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        !reason.is_empty() && reason != "stop" && reason != "toolUse"
                    });
                // Only hide if no text AND not an error/aborted message.
                if !has_text && !is_error_or_aborted {
                    continue;
                }
            }

            // Entry types hidden in default view (settings/bookkeeping).
            let is_settings_entry = matches!(
                entry,
                SessionEntry::Label { .. }
                    | SessionEntry::Custom { .. }
                    | SessionEntry::ModelChange { .. }
                    | SessionEntry::ThinkingLevelChange { .. }
                    | SessionEntry::SessionInfo { .. }
            );

            // Apply filter mode.
            let passes_filter = match self.filter_mode {
                FilterMode::UserOnly => {
                    // Just user messages
                    matches!(entry, SessionEntry::Message { message, .. }
                        if message.get("role").and_then(Value::as_str) == Some("user"))
                }
                FilterMode::NoTools => {
                    // Default minus tool results
                    !is_settings_entry
                        && !matches!(entry, SessionEntry::Message { message, .. }
                            if message.get("role").and_then(Value::as_str) == Some("toolResult"))
                }
                FilterMode::LabeledOnly => {
                    // Just labeled entries
                    node.label.is_some()
                }
                FilterMode::All => true,
                FilterMode::Default => !is_settings_entry,
            };

            if !passes_filter {
                continue;
            }

            // Apply search filter.
            if !search_tokens.is_empty() {
                let node_text = self.get_searchable_text(flat_node.arena_idx).to_lowercase();
                if !search_tokens.iter().all(|token| node_text.contains(token)) {
                    continue;
                }
            }

            filtered.push(flat_idx);
        }
        self.filtered_nodes = filtered;

        // Filter out descendants of folded nodes.
        if !self.folded_nodes.is_empty() {
            let mut skip_set: HashSet<String> = HashSet::new();
            for flat_node in &self.flat_nodes {
                let node = &self.arena[flat_node.arena_idx];
                let id = node.id();
                if let Some(parent_id) = node.parent_id()
                    && (self.folded_nodes.contains(parent_id) || skip_set.contains(parent_id))
                {
                    skip_set.insert(id.to_owned());
                }
            }
            let filtered = std::mem::take(&mut self.filtered_nodes);
            self.filtered_nodes = filtered
                .into_iter()
                .filter(|&f| !skip_set.contains(self.node_id(f)))
                .collect();
        }

        // Recalculate visual structure (indent, connectors, gutters) based on visible tree.
        self.recalculate_visual_structure();

        // Try to preserve cursor on the same node, or find nearest visible ancestor.
        if let Some(last_id) = self.last_selected_id.clone() {
            self.selected_index = self.find_nearest_visible_index(Some(&last_id));
        } else if self.selected_index >= self.filtered_nodes.len() {
            // Clamp index if out of bounds
            self.selected_index = self.filtered_nodes.len().saturating_sub(1);
        }

        // Update lastSelectedId to the actual selection (may have changed due to parent walk).
        if !self.filtered_nodes.is_empty() {
            self.last_selected_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&f| self.node_id(f).to_owned())
                .or_else(|| self.last_selected_id.clone());
        }
    }

    /// Recompute indentation/connectors for the filtered view.
    ///
    /// Filtering can hide intermediate entries; descendants attach to the nearest visible ancestor.
    /// Keep indentation semantics aligned with `flatten_tree` so single-child chains don't drift right.
    fn recalculate_visual_structure(&mut self) {
        if self.filtered_nodes.is_empty() {
            return;
        }

        let arena_by_id: HashMap<String, usize> = self
            .arena
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.id().to_owned(), idx))
            .collect();
        let visible_ids: HashSet<&str> = self
            .filtered_nodes
            .iter()
            .map(|&f| self.node_id(f))
            .collect();

        // Find nearest visible ancestor for a node.
        let find_visible_ancestor = |node_id: &str| -> Option<String> {
            let mut current_id = arena_by_id
                .get(node_id)
                .and_then(|&idx| self.arena[idx].parent_id());
            while let Some(id) = current_id {
                if visible_ids.contains(id) {
                    return Some(id.to_owned());
                }
                current_id = arena_by_id
                    .get(id)
                    .and_then(|&idx| self.arena[idx].parent_id());
            }
            None
        };

        // Build visible tree structure:
        // - visibleParent: nodeId → nearest visible ancestor (or None for roots)
        // - visibleChildren: parentId → list of visible children (in filteredNodes order)
        let mut visible_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut visible_children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        visible_children.insert(None, Vec::new()); // root-level nodes

        for &f in &self.filtered_nodes {
            let node_id = self.node_id(f).to_owned();
            let ancestor_id = find_visible_ancestor(&node_id);
            visible_parent.insert(node_id.clone(), ancestor_id.clone());
            visible_children
                .entry(ancestor_id)
                .or_default()
                .push(node_id);
        }

        // Update multipleRoots based on visible roots.
        let visible_root_ids = visible_children.get(&None).cloned().unwrap_or_default();
        self.multiple_roots = visible_root_ids.len() > 1;

        // Map for quick lookup: nodeId → flat_nodes index.
        let filtered_node_map: HashMap<String, usize> = self
            .filtered_nodes
            .iter()
            .map(|&f| (self.node_id(f).to_owned(), f))
            .collect();

        // DFS over the visible tree using flatten_tree() indentation semantics.
        // Stack items: (node_id, indent, just_branched, show_connector, is_last, gutters, is_virtual_root_child)
        type StackItem = (String, usize, bool, bool, bool, Vec<GutterInfo>, bool);
        let mut stack: Vec<StackItem> = Vec::new();

        // Add visible roots in reverse order (to process in forward order via stack).
        for (i, root_id) in visible_root_ids.iter().enumerate().rev() {
            let is_last = i == visible_root_ids.len() - 1;
            stack.push((
                root_id.clone(),
                usize::from(self.multiple_roots),
                self.multiple_roots,
                self.multiple_roots,
                is_last,
                Vec::new(),
                self.multiple_roots,
            ));
        }

        while let Some((
            node_id,
            indent,
            just_branched,
            show_connector,
            is_last,
            gutters,
            is_virtual_root_child,
        )) = stack.pop()
        {
            let Some(&flat_idx) = filtered_node_map.get(&node_id) else {
                continue;
            };

            // Update this node's visual properties.
            {
                let flat_node = &mut self.flat_nodes[flat_idx];
                flat_node.indent = indent;
                flat_node.show_connector = show_connector;
                flat_node.is_last = is_last;
                flat_node.gutters = gutters.clone();
                flat_node.is_virtual_root_child = is_virtual_root_child;
            }

            // Get visible children of this node.
            let children = visible_children
                .get(&Some(node_id.clone()))
                .cloned()
                .unwrap_or_default();
            let multiple_children = children.len() > 1;

            // Child indent follows flatten_tree(): branch points (and first generation after a branch) shift +1.
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };

            // Child gutters follow flatten_tree() connector/gutter rules.
            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let child_gutters: Vec<GutterInfo> = if connector_displayed {
                let mut g = gutters;
                g.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
                g
            } else {
                gutters
            };

            // Add children in reverse order (to process in forward order via stack).
            for (i, child) in children.iter().enumerate().rev() {
                let child_is_last = i == children.len() - 1;
                stack.push((
                    child.clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        // Store visible tree maps for ancestor/descendant lookups in navigation.
        self.visible_parent_map = visible_parent;
        self.visible_children_map = visible_children;
    }

    /// Get searchable text content from a node.
    fn get_searchable_text(&self, arena_idx: usize) -> String {
        let node = &self.arena[arena_idx];
        let entry = &node.entry;
        let mut parts: Vec<String> = Vec::new();

        if let Some(label) = &node.label {
            parts.push(label.clone());
        }

        match entry {
            SessionEntry::Message { message, .. } => {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                parts.push(role.to_owned());
                if let Some(content) = message.get("content")
                    && !content.is_null()
                {
                    parts.push(extract_content(Some(content)));
                }
                if role == "bashExecution"
                    && let Some(command) = message.get("command").and_then(Value::as_str)
                {
                    parts.push(command.to_owned());
                }
            }
            SessionEntry::CustomMessage {
                custom_type,
                content,
                ..
            } => {
                parts.push(custom_type.clone());
                if let Some(text) = content.as_str() {
                    parts.push(text.to_owned());
                } else {
                    parts.push(extract_content(Some(content)));
                }
            }
            SessionEntry::Compaction { .. } => parts.push("compaction".to_owned()),
            SessionEntry::BranchSummary { summary, .. } => {
                parts.push("branch summary".to_owned());
                parts.push(summary.clone());
            }
            SessionEntry::SessionInfo { name, .. } => {
                parts.push("title".to_owned());
                if let Some(name) = name {
                    parts.push(name.clone());
                }
            }
            SessionEntry::ModelChange { model_id, .. } => {
                parts.push("model".to_owned());
                parts.push(model_id.clone());
            }
            SessionEntry::ThinkingLevelChange { thinking_level, .. } => {
                parts.push("thinking".to_owned());
                parts.push(thinking_level.clone());
            }
            SessionEntry::Custom { custom_type, .. } => {
                parts.push("custom".to_owned());
                parts.push(custom_type.clone());
            }
            SessionEntry::Label { label, .. } => {
                parts.push("label".to_owned());
                parts.push(label.clone().unwrap_or_default());
            }
        }

        parts.join(" ")
    }

    #[must_use]
    pub fn get_search_query(&self) -> &str {
        &self.search_query
    }

    #[must_use]
    pub fn filter_mode(&self) -> FilterMode {
        self.filter_mode
    }

    /// Entry of the currently selected node.
    #[must_use]
    pub fn get_selected_entry(&self) -> Option<&SessionEntry> {
        self.filtered_nodes
            .get(self.selected_index)
            .map(|&f| &self.arena[self.flat_nodes[f].arena_idx].entry)
    }

    pub fn copy_selected(&mut self) {
        let text = self
            .filtered_nodes
            .get(self.selected_index)
            .and_then(|&f| self.get_entry_copy_text(self.flat_nodes[f].arena_idx));
        if let Some(cb) = &mut self.on_copy {
            cb(text);
        }
    }

    pub fn update_node_label(
        &mut self,
        entry_id: &str,
        label: Option<&str>,
        label_timestamp: Option<&str>,
    ) {
        for node in &mut self.arena {
            if node.id() == entry_id {
                node.label = label.map(str::to_owned);
                node.label_timestamp = if label.is_some() {
                    Some(label_timestamp.map(str::to_owned).unwrap_or_else(|| {
                        jiff::Timestamp::now()
                            .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
                            .to_string()
                    }))
                } else {
                    None
                };
                break;
            }
        }
    }

    fn get_status_labels(&self) -> String {
        let mut labels = String::new();
        match self.filter_mode {
            FilterMode::NoTools => labels.push_str(" [no-tools]"),
            FilterMode::UserOnly => labels.push_str(" [user]"),
            FilterMode::LabeledOnly => labels.push_str(" [labeled]"),
            FilterMode::All => labels.push_str(" [all]"),
            FilterMode::Default => {}
        }
        if self.show_label_timestamps {
            labels.push_str(" [+label time]");
        }
        labels
    }

    fn render_lines(&mut self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.filtered_nodes.is_empty() {
            lines.push(truncate_ellipsis(
                &theme().fg(ThemeColor::Muted, "  No entries found"),
                width,
            ));
            lines.push(truncate_ellipsis(
                &theme().fg(
                    ThemeColor::Muted,
                    &format!("  (0/0){}", self.get_status_labels()),
                ),
                width,
            ));
            return lines;
        }

        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible_lines / 2)
            .min(
                self.filtered_nodes
                    .len()
                    .saturating_sub(self.max_visible_lines),
            );
        let end_index = (start_index + self.max_visible_lines).min(self.filtered_nodes.len());

        let mut rendered_rows: Vec<HorizontalViewportRow> = Vec::new();
        for i in start_index..end_index {
            let flat_node = &self.flat_nodes[self.filtered_nodes[i]];
            let node = &self.arena[flat_node.arena_idx];
            let entry_id = node.id().to_owned();
            let is_selected = i == self.selected_index;

            // Build line: cursor + prefix + path marker + label + content
            let cursor = if is_selected {
                theme().fg(ThemeColor::Accent, "› ")
            } else {
                "  ".to_owned()
            };

            // If multiple roots, shift display (roots at 0, not 1)
            let display_indent = if self.multiple_roots {
                flat_node.indent.saturating_sub(1)
            } else {
                flat_node.indent
            };

            // Build prefix with gutters at their correct positions.
            // Each gutter has a position (displayIndent where its connector was shown).
            let has_connector = flat_node.show_connector && !flat_node.is_virtual_root_child;
            let connector_position: Option<usize> = if has_connector {
                Some(display_indent.saturating_sub(1))
            } else {
                None
            };

            // Build prefix char by char, placing gutters and connector at their positions.
            let total_chars = display_indent * 3;
            let mut prefix = String::new();
            let is_folded = self.folded_nodes.contains(&entry_id);
            for c in 0..total_chars {
                let level = c / 3;
                let pos_in_level = c % 3;

                // Check if there's a gutter at this level
                if let Some(gutter) = flat_node.gutters.iter().find(|g| g.position == level) {
                    if pos_in_level == 0 {
                        prefix.push(if gutter.show { '│' } else { ' ' });
                    } else {
                        prefix.push(' ');
                    }
                } else if has_connector && Some(level) == connector_position {
                    // Connector at this level, with fold indicator
                    if pos_in_level == 0 {
                        prefix.push(if flat_node.is_last { '└' } else { '├' });
                    } else if pos_in_level == 1 {
                        let foldable = self.is_foldable(&entry_id);
                        prefix.push(if is_folded {
                            '⊞'
                        } else if foldable {
                            '⊟'
                        } else {
                            '─'
                        });
                    } else {
                        prefix.push(' ');
                    }
                } else {
                    prefix.push(' ');
                }
            }

            // Fold marker for nodes without connectors (roots)
            let shows_fold_in_connector = has_connector;
            let fold_marker = if is_folded && !shows_fold_in_connector {
                theme().fg(ThemeColor::Accent, "⊞ ")
            } else {
                String::new()
            };

            // Active path marker - shown right before the entry text
            let is_on_active_path = self.active_path_ids.contains(&entry_id);
            let path_marker = if is_on_active_path {
                theme().fg(ThemeColor::Accent, "• ")
            } else {
                String::new()
            };

            let label = node
                .label
                .as_ref()
                .map(|l| theme().fg(ThemeColor::Warning, &format!("[{l}] ")))
                .unwrap_or_default();
            let label_timestamp = if self.show_label_timestamps
                && node.label.is_some()
                && let Some(ts) = &node.label_timestamp
            {
                theme().fg(
                    ThemeColor::Muted,
                    &format!("{} ", format_label_timestamp(ts)),
                )
            } else {
                String::new()
            };
            let content = self.get_entry_display_text(flat_node.arena_idx, is_selected);
            let prefix_part = format!(
                "{}{fold_marker}{path_marker}",
                theme().fg(ThemeColor::Dim, &prefix)
            );
            let anchor_col = visible_width(&prefix_part);
            let mut gutter = cursor;
            let mut body = format!("{prefix_part}{label}{label_timestamp}{content}");
            if is_selected {
                gutter = theme().bg(ThemeBg::SelectedBg, &gutter);
                body = theme().bg(ThemeBg::SelectedBg, &body);
            }
            let body_width = visible_width(&body);
            rendered_rows.push(HorizontalViewportRow {
                gutter,
                body,
                anchor_col,
                body_width,
                is_selected,
            });
        }

        lines.extend(render_horizontal_viewport(&rendered_rows, width));
        lines.push(truncate_ellipsis(
            &theme().fg(
                ThemeColor::Muted,
                &format!(
                    "  ({}/{}){}",
                    self.selected_index + 1,
                    self.filtered_nodes.len(),
                    self.get_status_labels()
                ),
            ),
            width,
        ));

        lines
    }

    fn get_entry_display_text(&self, arena_idx: usize, is_selected: bool) -> String {
        let entry = &self.arena[arena_idx].entry;

        let result = match entry {
            SessionEntry::Message { message, .. } => {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                if role == "user" {
                    let content = normalize(&extract_content(message.get("content")));
                    format!("{}{content}", theme().fg(ThemeColor::Accent, "user: "))
                } else if role == "assistant" {
                    let text_content = normalize(&extract_content(message.get("content")));
                    let prefix = theme().fg(ThemeColor::Success, "assistant: ");
                    if !text_content.is_empty() {
                        format!("{prefix}{text_content}")
                    } else if message.get("stopReason").and_then(Value::as_str) == Some("aborted") {
                        format!("{prefix}{}", theme().fg(ThemeColor::Muted, "(aborted)"))
                    } else if let Some(error_message) = message
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        let err_msg = normalize(error_message);
                        let err_msg = slice_utf16_prefix(&err_msg, 80);
                        format!("{prefix}{}", theme().fg(ThemeColor::Error, err_msg))
                    } else {
                        format!("{prefix}{}", theme().fg(ThemeColor::Muted, "(no content)"))
                    }
                } else if role == "toolResult" {
                    let tool_call = message
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .and_then(|id| self.tool_call_map.get(id));
                    if let Some(tool_call) = tool_call {
                        theme().fg(
                            ThemeColor::Muted,
                            &format_tool_call(&tool_call.name, &tool_call.arguments),
                        )
                    } else {
                        let tool_name = message
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        theme().fg(ThemeColor::Muted, &format!("[{tool_name}]"))
                    }
                } else if role == "bashExecution" {
                    let command = message.get("command").and_then(Value::as_str).unwrap_or("");
                    theme().fg(ThemeColor::Dim, &format!("[bash]: {}", normalize(command)))
                } else {
                    theme().fg(ThemeColor::Dim, &format!("[{role}]"))
                }
            }
            SessionEntry::CustomMessage {
                custom_type,
                content,
                ..
            } => {
                let content_text = if let Some(text) = content.as_str() {
                    text.to_owned()
                } else {
                    extract_full_content(Some(content))
                };
                format!(
                    "{}{}",
                    theme().fg(
                        ThemeColor::CustomMessageLabel,
                        &format!("[{custom_type}]: ")
                    ),
                    normalize(&content_text)
                )
            }
            SessionEntry::Compaction { tokens_before, .. } => {
                let tokens = (*tokens_before as f64 / 1000.0).round() as i64;
                theme().fg(
                    ThemeColor::BorderAccent,
                    &format!("[compaction: {tokens}k tokens]"),
                )
            }
            SessionEntry::BranchSummary { summary, .. } => format!(
                "{}{}",
                theme().fg(ThemeColor::Warning, "[branch summary]: "),
                normalize(summary)
            ),
            SessionEntry::ModelChange { model_id, .. } => {
                theme().fg(ThemeColor::Dim, &format!("[model: {model_id}]"))
            }
            SessionEntry::ThinkingLevelChange { thinking_level, .. } => {
                theme().fg(ThemeColor::Dim, &format!("[thinking: {thinking_level}]"))
            }
            SessionEntry::Custom { custom_type, .. } => {
                theme().fg(ThemeColor::Dim, &format!("[custom: {custom_type}]"))
            }
            SessionEntry::Label { label, .. } => theme().fg(
                ThemeColor::Dim,
                &format!("[label: {}]", label.as_deref().unwrap_or("(cleared)")),
            ),
            SessionEntry::SessionInfo { name, .. } => match name {
                Some(name) => [
                    theme().fg(ThemeColor::Dim, "[title: "),
                    theme().fg(ThemeColor::Dim, name),
                    theme().fg(ThemeColor::Dim, "]"),
                ]
                .join(""),
                None => [
                    theme().fg(ThemeColor::Dim, "[title: "),
                    theme().italic(&theme().fg(ThemeColor::Dim, "empty")),
                    theme().fg(ThemeColor::Dim, "]"),
                ]
                .join(""),
            },
        };

        if is_selected {
            theme().bold(&result)
        } else {
            result
        }
    }

    fn get_entry_copy_text(&self, arena_idx: usize) -> Option<String> {
        let entry = &self.arena[arena_idx].entry;
        let text: Option<String> = match entry {
            SessionEntry::Message { message, .. } => {
                if message.get("role").and_then(Value::as_str) == Some("bashExecution") {
                    message
                        .get("command")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                } else if let Some(content) = message.get("content") {
                    let mut text = extract_full_content(Some(content));
                    if text.is_empty()
                        && message.get("role").and_then(Value::as_str) == Some("assistant")
                    {
                        text = message
                            .get("errorMessage")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                    }
                    Some(text)
                } else {
                    None
                }
            }
            SessionEntry::CustomMessage { content, .. } => {
                if let Some(text) = content.as_str() {
                    Some(text.to_owned())
                } else {
                    Some(extract_full_content(Some(content)))
                }
            }
            SessionEntry::Compaction { summary, .. }
            | SessionEntry::BranchSummary { summary, .. } => Some(summary.clone()),
            _ => None,
        };

        text.filter(|t| !t.trim().is_empty())
    }

    /// Whether a node can be folded. A node is foldable if it has visible children
    /// and is either a root (no visible parent) or a segment start (visible parent
    /// has multiple visible children).
    fn is_foldable(&self, entry_id: &str) -> bool {
        let Some(children) = self.visible_children_map.get(&Some(entry_id.to_owned())) else {
            return false;
        };
        if children.is_empty() {
            return false;
        }
        let Some(Some(parent_id)) = self.visible_parent_map.get(entry_id) else {
            return true;
        };
        self.visible_children_map
            .get(&Some(parent_id.clone()))
            .is_some_and(|siblings| siblings.len() > 1)
    }

    /// Find the index of the next branch segment start in the given direction.
    /// A segment start is the first child of a branch point.
    ///
    /// "up" walks the visible parent chain; "down" walks visible children
    /// (always following the first child).
    fn find_branch_segment_start(&self, direction_up: bool) -> usize {
        let Some(selected_id) = self
            .filtered_nodes
            .get(self.selected_index)
            .map(|&f| self.node_id(f).to_owned())
        else {
            return self.selected_index;
        };

        let index_by_entry_id: HashMap<&str, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(i, &f)| (self.node_id(f), i))
            .collect();

        let mut current_id = selected_id;
        if !direction_up {
            loop {
                let children = self
                    .visible_children_map
                    .get(&Some(current_id.clone()))
                    .cloned()
                    .unwrap_or_default();
                if children.is_empty() {
                    return index_by_entry_id[current_id.as_str()];
                }
                if children.len() > 1 {
                    return index_by_entry_id[children[0].as_str()];
                }
                current_id = children[0].clone();
            }
        }

        // direction == "up"
        loop {
            let parent_id = self.visible_parent_map.get(&current_id).cloned().flatten();
            let Some(parent_id) = parent_id else {
                return index_by_entry_id[current_id.as_str()];
            };
            let children = self
                .visible_children_map
                .get(&Some(parent_id.clone()))
                .cloned()
                .unwrap_or_default();
            if children.len() > 1 {
                let segment_start = index_by_entry_id[current_id.as_str()];
                if segment_start < self.selected_index {
                    return segment_start;
                }
            }
            current_id = parent_id;
        }
    }
}

fn normalize(s: &str) -> String {
    s.replace(['\n', '\t'], " ").trim().to_owned()
}

fn extract_content(content: Option<&Value>) -> String {
    let full = extract_full_content(content);
    slice_utf16_prefix(&full, 200).to_owned()
}

fn extract_full_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut result = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            result.push_str(text);
        }
    }
    result
}

fn has_text_content(content: Option<&Value>) -> bool {
    let Some(content) = content else {
        return false;
    };
    if let Some(text) = content.as_str() {
        return !text.trim().is_empty();
    }
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                return true;
            }
        }
    }
    false
}

fn format_label_timestamp(timestamp: &str) -> String {
    let Ok(ts) = timestamp.parse::<jiff::Timestamp>() else {
        return timestamp.to_owned();
    };
    let date = ts.to_zoned(jiff::tz::TimeZone::system());
    let now = jiff::Zoned::now();
    let time = format!("{:02}:{:02}", date.hour(), date.minute());

    if date.year() == now.year() && date.month() == now.month() && date.day() == now.day() {
        return time;
    }

    let month = date.month();
    let day = date.day();
    if date.year() == now.year() {
        return format!("{month}/{day} {time}");
    }

    let year = date.year().rem_euclid(100);
    format!("{year:02}/{month}/{day} {time}")
}

fn shorten_path(p: &str) -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        return format!("~{}", &p[home.len()..]);
    }
    p.to_owned()
}

fn arg_str(args: &Value, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn format_tool_call(name: &str, args: &Value) -> String {
    match name {
        "read" => {
            let path_arg = {
                let p = arg_str(args, "path");
                if p.is_empty() {
                    arg_str(args, "file_path")
                } else {
                    p
                }
            };
            let path = shorten_path(&path_arg);
            let offset = args.get("offset").and_then(Value::as_i64);
            let limit = args.get("limit").and_then(Value::as_i64);
            let mut display = path;
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1);
                let end = limit.map(|l| start + l - 1);
                display.push(':');
                display.push_str(&start.to_string());
                if let Some(end) = end {
                    display.push('-');
                    display.push_str(&end.to_string());
                }
            }
            format!("[read: {display}]")
        }
        "write" => {
            let p = arg_str(args, "path");
            let p = if p.is_empty() {
                arg_str(args, "file_path")
            } else {
                p
            };
            format!("[write: {}]", shorten_path(&p))
        }
        "edit" => {
            let p = arg_str(args, "path");
            let p = if p.is_empty() {
                arg_str(args, "file_path")
            } else {
                p
            };
            format!("[edit: {}]", shorten_path(&p))
        }
        "bash" => {
            let raw_cmd = arg_str(args, "command");
            let trimmed = raw_cmd.replace(['\n', '\t'], " ").trim().to_owned();
            let cmd = slice_utf16_prefix(&trimmed, 50);
            format!(
                "[bash: {cmd}{}]",
                if utf16_len(&raw_cmd) > 50 { "..." } else { "" }
            )
        }
        "grep" => {
            let pattern = arg_str(args, "pattern");
            let p = arg_str(args, "path");
            let path = shorten_path(if p.is_empty() { "." } else { &p });
            format!("[grep: /{pattern}/ in {path}]")
        }
        "find" => {
            let pattern = arg_str(args, "pattern");
            let p = arg_str(args, "path");
            let path = shorten_path(if p.is_empty() { "." } else { &p });
            format!("[find: {pattern} in {path}]")
        }
        "ls" => {
            let p = arg_str(args, "path");
            let path = shorten_path(if p.is_empty() { "." } else { &p });
            format!("[ls: {path}]")
        }
        _ => {
            // Custom tool - show name and truncated JSON args
            let args_json = serde_json::to_string(args).unwrap_or_default();
            let args_str = slice_utf16_prefix(&args_json, 40);
            format!(
                "[{name}: {args_str}{}]",
                if utf16_len(&args_json) > 40 {
                    "..."
                } else {
                    ""
                }
            )
        }
    }
}

impl Component for TreeList {
    fn render(&mut self, width: u16) -> &[Line] {
        let lines = self.render_lines(width as usize);
        self.cached = lines.iter().map(|l| Line::from_ansi(l)).collect();
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, key_data: &str) {
        struct Matches {
            up: bool,
            down: bool,
            fold_or_up: bool,
            unfold_or_down: bool,
            cursor_left: bool,
            cursor_right: bool,
            page_up: bool,
            page_down: bool,
            confirm: bool,
            copy: bool,
            cancel: bool,
            filter_default: bool,
            filter_no_tools: bool,
            filter_user_only: bool,
            filter_labeled_only: bool,
            filter_all: bool,
            filter_cycle_backward: bool,
            filter_cycle_forward: bool,
            delete_char_backward: bool,
            edit_label: bool,
            toggle_label_timestamp: bool,
        }
        let m = {
            let kb = get_keybindings();
            Matches {
                up: kb.matches(key_data, "tui.select.up"),
                down: kb.matches(key_data, "tui.select.down"),
                fold_or_up: kb.matches(key_data, "app.tree.foldOrUp"),
                unfold_or_down: kb.matches(key_data, "app.tree.unfoldOrDown"),
                cursor_left: kb.matches(key_data, "tui.editor.cursorLeft"),
                cursor_right: kb.matches(key_data, "tui.editor.cursorRight"),
                page_up: kb.matches(key_data, "tui.select.pageUp"),
                page_down: kb.matches(key_data, "tui.select.pageDown"),
                confirm: kb.matches(key_data, "tui.select.confirm"),
                copy: kb.matches(key_data, "app.message.copy"),
                cancel: kb.matches(key_data, "tui.select.cancel"),
                filter_default: kb.matches(key_data, "app.tree.filter.default"),
                filter_no_tools: kb.matches(key_data, "app.tree.filter.noTools"),
                filter_user_only: kb.matches(key_data, "app.tree.filter.userOnly"),
                filter_labeled_only: kb.matches(key_data, "app.tree.filter.labeledOnly"),
                filter_all: kb.matches(key_data, "app.tree.filter.all"),
                filter_cycle_backward: kb.matches(key_data, "app.tree.filter.cycleBackward"),
                filter_cycle_forward: kb.matches(key_data, "app.tree.filter.cycleForward"),
                delete_char_backward: kb.matches(key_data, "tui.editor.deleteCharBackward"),
                edit_label: kb.matches(key_data, "app.tree.editLabel"),
                toggle_label_timestamp: kb.matches(key_data, "app.tree.toggleLabelTimestamp"),
            }
        };

        if m.up {
            self.selected_index = if self.selected_index == 0 {
                self.filtered_nodes.len().saturating_sub(1)
            } else {
                self.selected_index - 1
            };
        } else if m.down {
            self.selected_index = if self.selected_index + 1 >= self.filtered_nodes.len() {
                0
            } else {
                self.selected_index + 1
            };
        } else if m.fold_or_up {
            let current_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&f| self.node_id(f).to_owned());
            if let Some(id) = current_id.filter(|id| {
                !id.is_empty() && self.is_foldable(id) && !self.folded_nodes.contains(id)
            }) {
                self.folded_nodes.insert(id);
                self.apply_filter();
            } else {
                self.selected_index = self.find_branch_segment_start(true);
            }
        } else if m.unfold_or_down {
            let current_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&f| self.node_id(f).to_owned());
            if let Some(id) = current_id.filter(|id| self.folded_nodes.contains(id)) {
                self.folded_nodes.remove(&id);
                self.apply_filter();
            } else {
                self.selected_index = self.find_branch_segment_start(false);
            }
        } else if m.cursor_left || m.page_up {
            // Page up
            self.selected_index = self.selected_index.saturating_sub(self.max_visible_lines);
        } else if m.cursor_right || m.page_down {
            // Page down
            self.selected_index = (self.selected_index + self.max_visible_lines)
                .min(self.filtered_nodes.len().saturating_sub(1));
        } else if m.confirm {
            let selected_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&f| self.node_id(f).to_owned());
            if let (Some(id), Some(cb)) = (selected_id, &mut self.on_select) {
                cb(&id);
            }
        } else if m.copy {
            self.copy_selected();
        } else if m.cancel {
            if !self.search_query.is_empty() {
                self.search_query.clear();
                self.folded_nodes.clear();
                self.apply_filter();
            } else if let Some(cb) = &mut self.on_cancel {
                cb();
            }
        } else if m.filter_default {
            // Direct filter: default
            self.filter_mode = FilterMode::Default;
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.filter_no_tools {
            // Toggle filter: no-tools ↔ default
            self.filter_mode = if self.filter_mode == FilterMode::NoTools {
                FilterMode::Default
            } else {
                FilterMode::NoTools
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.filter_user_only {
            // Toggle filter: user-only ↔ default
            self.filter_mode = if self.filter_mode == FilterMode::UserOnly {
                FilterMode::Default
            } else {
                FilterMode::UserOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.filter_labeled_only {
            // Toggle filter: labeled-only ↔ default
            self.filter_mode = if self.filter_mode == FilterMode::LabeledOnly {
                FilterMode::Default
            } else {
                FilterMode::LabeledOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.filter_all {
            // Toggle filter: all ↔ default
            self.filter_mode = if self.filter_mode == FilterMode::All {
                FilterMode::Default
            } else {
                FilterMode::All
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.filter_cycle_backward {
            // Cycle filter backwards
            let modes = FilterMode::CYCLE;
            let current_index = modes
                .iter()
                .position(|&f| f == self.filter_mode)
                .unwrap_or(0);
            self.filter_mode = modes[(current_index + modes.len() - 1) % modes.len()];
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.filter_cycle_forward {
            // Cycle filter forwards: default → no-tools → user-only → labeled-only → all → default
            let modes = FilterMode::CYCLE;
            let current_index = modes
                .iter()
                .position(|&f| f == self.filter_mode)
                .unwrap_or(0);
            self.filter_mode = modes[(current_index + 1) % modes.len()];
            self.folded_nodes.clear();
            self.apply_filter();
        } else if m.delete_char_backward {
            if !self.search_query.is_empty() {
                self.search_query.pop();
                self.folded_nodes.clear();
                self.apply_filter();
            }
        } else if m.edit_label {
            let selected = self.filtered_nodes.get(self.selected_index).map(|&f| {
                let node = &self.arena[self.flat_nodes[f].arena_idx];
                (node.id().to_owned(), node.label.clone())
            });
            if let (Some((id, label)), Some(cb)) = (selected, &mut self.on_label_edit) {
                cb(&id, label.as_deref());
            }
        } else if m.toggle_label_timestamp {
            self.show_label_timestamps = !self.show_label_timestamps;
        } else {
            let has_control_chars = key_data.chars().any(|ch| {
                let code = ch as u32;
                code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
            });
            if !has_control_chars && !key_data.is_empty() {
                self.search_query.push_str(key_data);
                self.folded_nodes.clear();
                self.apply_filter();
            }
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

/// Search-query line (oracle `SearchLine`).
fn search_line(query: &str, width: usize) -> String {
    if query.is_empty() {
        truncate_ellipsis(
            &format!("  {}", theme().fg(ThemeColor::Muted, "Type to search:")),
            width,
        )
    } else {
        truncate_ellipsis(
            &format!(
                "  {} {}",
                theme().fg(ThemeColor::Muted, "Type to search:"),
                theme().fg(ThemeColor::Accent, query)
            ),
            width,
        )
    }
}

struct TreeHelpItem {
    keys: &'static [&'static str],
    label: &'static str,
    label_first: bool,
}

const TREE_HELP_ITEMS: [TreeHelpItem; 8] = [
    TreeHelpItem {
        keys: &["tui.select.up", "tui.select.down"],
        label: "move",
        label_first: false,
    },
    TreeHelpItem {
        keys: &["tui.editor.cursorLeft", "tui.editor.cursorRight"],
        label: "page",
        label_first: false,
    },
    TreeHelpItem {
        keys: &["app.tree.foldOrUp", "app.tree.unfoldOrDown"],
        label: "branch",
        label_first: false,
    },
    TreeHelpItem {
        keys: &["app.message.copy"],
        label: "copy",
        label_first: false,
    },
    TreeHelpItem {
        keys: &["app.tree.editLabel"],
        label: "label",
        label_first: false,
    },
    TreeHelpItem {
        keys: &["app.tree.toggleLabelTimestamp"],
        label: "label time",
        label_first: false,
    },
    TreeHelpItem {
        keys: &[
            "app.tree.filter.default",
            "app.tree.filter.noTools",
            "app.tree.filter.userOnly",
            "app.tree.filter.labeledOnly",
            "app.tree.filter.all",
        ],
        label: "filters",
        label_first: true,
    },
    TreeHelpItem {
        keys: &[
            "app.tree.filter.cycleForward",
            "app.tree.filter.cycleBackward",
        ],
        label: "cycle",
        label_first: true,
    },
];

static PAGE_UP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bpageUp\b").unwrap());
static PAGE_DOWN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bpageDown\b").unwrap());
static UP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bup\b").unwrap());
static DOWN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bdown\b").unwrap());
static LEFT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bleft\b").unwrap());
static RIGHT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bright\b").unwrap());

fn format_help_keys(keybindings: &[&str]) -> String {
    let mut keys: Vec<String> = Vec::new();
    {
        let kb = get_keybindings();
        for keybinding in keybindings {
            if let Some(key) = kb.get_keys(keybinding).first() {
                keys.push(key.clone());
            }
        }
    }
    if keys.is_empty() {
        return String::new();
    }

    let text = format_key_text(&compact_raw_keys(&keys), false);
    let text = PAGE_UP_RE.replace_all(&text, "pgup");
    let text = PAGE_DOWN_RE.replace_all(&text, "pgdn");
    let text = UP_RE.replace_all(&text, "↑");
    let text = DOWN_RE.replace_all(&text, "↓");
    let text = LEFT_RE.replace_all(&text, "←");
    let text = RIGHT_RE.replace_all(&text, "→");
    text.into_owned()
}

fn compact_raw_keys(keys: &[String]) -> String {
    if keys.len() == 1 {
        return keys[0].clone();
    }

    let parts: Vec<(String, String)> = keys
        .iter()
        .map(|key| match key.rfind('+') {
            None => (String::new(), key.clone()),
            Some(separator_index) => (
                key[..=separator_index].to_owned(),
                key[separator_index + 1..].to_owned(),
            ),
        })
        .collect();
    let prefix = &parts[0].0;
    if !prefix.is_empty() && parts.iter().all(|(p, _)| p == prefix) {
        format!(
            "{prefix}{}",
            parts
                .iter()
                .map(|(_, suffix)| suffix.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )
    } else {
        keys.join("/")
    }
}

/// Tree help rows with chunk-aware wrapping (oracle `TreeHelp`).
fn tree_help_lines(width: usize) -> Vec<String> {
    let items: Vec<String> = TREE_HELP_ITEMS
        .iter()
        .map(|item| {
            let text = format_help_keys(item.keys);
            if text.is_empty() {
                return item.label.to_owned();
            }
            if item.label_first {
                format!("{} {text}", item.label)
            } else {
                format!("{text} {}", item.label)
            }
        })
        .collect();

    let available_width = width.max(1);
    let indent = "  ";
    let separator = " · ";
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();

    for item in &items {
        let candidate = if !current_line.is_empty() {
            format!("{current_line}{separator}{item}")
        } else if visible_width(&format!("{indent}{item}")) <= available_width {
            format!("{indent}{item}")
        } else {
            item.clone()
        };
        if current_line.is_empty() || visible_width(&candidate) <= available_width {
            current_line = candidate;
            continue;
        }

        lines.extend(wrap_text_with_ansi(
            current_line.trim_end(),
            available_width,
        ));
        current_line = if visible_width(&format!("{indent}{item}")) <= available_width {
            format!("{indent}{item}")
        } else {
            item.clone()
        };
    }

    if !current_line.is_empty() {
        lines.extend(wrap_text_with_ansi(
            current_line.trim_end(),
            available_width,
        ));
    }

    lines
        .into_iter()
        .map(|line| theme().fg(ThemeColor::Muted, &line))
        .collect()
}

/// Result of a label-input interaction, drained by the parent.
enum LabelAction {
    Submit(String, Option<String>),
    Cancel,
}

/// Label input component shown when editing a label (oracle `LabelInput`).
struct LabelInput {
    input: Input,
    entry_id: String,
    pending: Option<LabelAction>,
    focused: bool,
    cached: Vec<Line>,
}

impl LabelInput {
    fn new(entry_id: &str, current_label: Option<&str>) -> Self {
        let mut input = Input::new();
        if let Some(label) = current_label {
            input.set_value(label);
        }
        Self {
            input,
            entry_id: entry_id.to_owned(),
            pending: None,
            focused: false,
            cached: Vec::new(),
        }
    }
}

impl Component for LabelInput {
    fn render(&mut self, width: u16) -> &[Line] {
        let w = width as usize;
        let indent = "  ";
        let available_width = width.saturating_sub(indent.len() as u16);
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from_ansi(&truncate_ellipsis(
            &format!(
                "{indent}{}",
                theme().fg(ThemeColor::Muted, "Label (empty to remove):")
            ),
            w,
        )));
        for line in self.input.render(available_width) {
            lines.push(Line::from_ansi(&truncate_ellipsis(
                &format!("{indent}{}", line.to_ansi()),
                w,
            )));
        }
        lines.push(Line::from_ansi(&truncate_ellipsis(
            &format!(
                "{indent}{}  {}",
                key_hint("tui.select.confirm", "save"),
                key_hint("tui.select.cancel", "cancel")
            ),
            w,
        )));
        self.cached = lines;
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, key_data: &str) {
        let (confirm, cancel) = {
            let kb = get_keybindings();
            (
                kb.matches(key_data, "tui.select.confirm"),
                kb.matches(key_data, "tui.select.cancel"),
            )
        };
        if confirm {
            let value = self.input.get_value().trim().to_owned();
            self.pending = Some(LabelAction::Submit(
                self.entry_id.clone(),
                (!value.is_empty()).then_some(value),
            ));
        } else if cancel {
            self.pending = Some(LabelAction::Cancel);
        } else {
            self.input.handle_input(key_data);
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

impl Focusable for LabelInput {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

/// Component that renders a session tree selector for navigation
/// (oracle `TreeSelectorComponent`).
pub struct TreeSelectorComponent {
    tree_list: TreeList,
    label_input: Option<LabelInput>,
    on_label_change: Option<LabelCallback>,
    pub on_copy: Option<Box<dyn FnMut(Option<String>)>>,

    top_border: DynamicBorder,
    mid_border: DynamicBorder,
    bottom_border: DynamicBorder,
    title: Text,

    /// `(entry_id, current_label)` requested by the tree list.
    label_edit_request: LabelEditRequest,
    /// Copy text forwarded from the tree list.
    copy_request: Rc<RefCell<Option<Option<String>>>>,
    /// Oracle `setTimeout(onCancel, 100)` for an empty tree.
    empty_tree_cancel: Option<(Instant, SharedCancel)>,

    focused: bool,
    cached: Vec<Line>,
}

impl TreeSelectorComponent {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        tree: Vec<SessionTreeNode>,
        current_leaf_id: Option<&str>,
        terminal_height: u16,
        on_select: IdCallback,
        on_cancel: Box<dyn FnMut()>,
        on_label_change: Option<LabelCallback>,
        initial_selected_id: Option<&str>,
        initial_filter_mode: Option<FilterMode>,
    ) -> Self {
        let max_visible_lines = (usize::from(terminal_height) / 2).max(5);
        let tree_is_empty = tree.is_empty();

        let mut tree_list = TreeList::new(
            tree,
            current_leaf_id,
            max_visible_lines,
            initial_selected_id,
            initial_filter_mode,
        );
        tree_list.on_select = Some(on_select);

        let cancel_shared: SharedCancel = Rc::new(RefCell::new(on_cancel));
        {
            let cancel = Rc::clone(&cancel_shared);
            tree_list.on_cancel = Some(Box::new(move || (cancel.borrow_mut())()));
        }

        let label_edit_request: LabelEditRequest = Rc::new(RefCell::new(None));
        {
            let slot = Rc::clone(&label_edit_request);
            tree_list.on_label_edit = Some(Box::new(move |entry_id, current_label| {
                *slot.borrow_mut() = Some((entry_id.to_owned(), current_label.map(str::to_owned)));
            }));
        }

        let copy_request: Rc<RefCell<Option<Option<String>>>> = Rc::new(RefCell::new(None));
        {
            let slot = Rc::clone(&copy_request);
            tree_list.on_copy = Some(Box::new(move |text| {
                *slot.borrow_mut() = Some(text);
            }));
        }

        Self {
            tree_list,
            label_input: None,
            on_label_change,
            on_copy: None,
            top_border: DynamicBorder::default(),
            mid_border: DynamicBorder::default(),
            bottom_border: DynamicBorder::default(),
            title: Text::new(theme().bold("  Session Tree"), 1, 0, None),
            label_edit_request,
            copy_request,
            empty_tree_cancel: tree_is_empty
                .then(|| (Instant::now() + Duration::from_millis(100), cancel_shared)),
            focused: false,
            cached: Vec::new(),
        }
    }

    pub fn get_tree_list(&mut self) -> &mut TreeList {
        &mut self.tree_list
    }

    fn show_label_input(&mut self, entry_id: &str, current_label: Option<&str>) {
        let mut label_input = LabelInput::new(entry_id, current_label);
        // Propagate current focused state to the new labelInput.
        label_input.set_focused(self.focused);
        self.label_input = Some(label_input);
    }

    fn hide_label_input(&mut self) {
        self.label_input = None;
    }

    /// Fire the deferred empty-tree cancel once its deadline has passed.
    fn fire_empty_tree_cancel(&mut self) {
        if let Some((deadline, _)) = &self.empty_tree_cancel
            && Instant::now() >= *deadline
            && let Some((_, cancel)) = self.empty_tree_cancel.take()
        {
            (cancel.borrow_mut())();
        }
    }

    fn drain_tree_list_requests(&mut self) {
        let label_edit = self.label_edit_request.borrow_mut().take();
        if let Some((entry_id, current_label)) = label_edit {
            self.show_label_input(&entry_id, current_label.as_deref());
        }
        let copy = self.copy_request.borrow_mut().take();
        if let Some(text) = copy
            && let Some(cb) = &mut self.on_copy
        {
            cb(text);
        }
    }
}

impl Component for TreeSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.fire_empty_tree_cancel();
        self.cached.clear();
        // Spacer(1)
        self.cached.push(Line::empty());
        self.cached.extend_from_slice(self.top_border.render(width));
        self.cached.extend_from_slice(self.title.render(width));
        for line in tree_help_lines(width as usize) {
            self.cached.push(Line::from_ansi(&line));
        }
        self.cached.push(Line::from_ansi(&search_line(
            self.tree_list.get_search_query(),
            width as usize,
        )));
        self.cached.extend_from_slice(self.mid_border.render(width));
        // Spacer(1)
        self.cached.push(Line::empty());
        match &mut self.label_input {
            Some(label_input) => {
                self.cached.extend_from_slice(label_input.render(width));
            }
            None => {
                self.cached.extend_from_slice(self.tree_list.render(width));
            }
        }
        // Spacer(1)
        self.cached.push(Line::empty());
        self.cached
            .extend_from_slice(self.bottom_border.render(width));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.mid_border.invalidate();
        self.bottom_border.invalidate();
        self.title.invalidate();
        self.tree_list.invalidate();
    }

    fn handle_input(&mut self, key_data: &str) {
        self.fire_empty_tree_cancel();
        if let Some(label_input) = &mut self.label_input {
            label_input.handle_input(key_data);
            if let Some(action) = label_input.pending.take() {
                match action {
                    LabelAction::Submit(entry_id, label) => {
                        self.tree_list
                            .update_node_label(&entry_id, label.as_deref(), None);
                        if let Some(cb) = &mut self.on_label_change {
                            cb(&entry_id, label.as_deref());
                        }
                        self.hide_label_input();
                    }
                    LabelAction::Cancel => self.hide_label_input(),
                }
            }
        } else {
            self.tree_list.handle_input(key_data);
            self.drain_tree_list_requests();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for TreeSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        // Propagate to labelInput when it's active.
        if let Some(label_input) = &mut self.label_input {
            label_input.set_focused(focused);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_util::NullOr;
    use serde_json::json;

    fn message_node(
        id: &str,
        parent: Option<&str>,
        message: Value,
        children: Vec<SessionTreeNode>,
    ) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry::Message {
                id: Some(id.to_owned()),
                parent_id: NullOr::from_option(parent.map(str::to_owned)),
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                message,
            },
            children,
            label: None,
            label_timestamp: None,
        }
    }

    fn label_node(id: &str, parent: Option<&str>) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry::Label {
                id: Some(id.to_owned()),
                parent_id: NullOr::from_option(parent.map(str::to_owned)),
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                target_id: "r".to_owned(),
                label: Some("bookmark".to_owned()),
            },
            children: Vec::new(),
            label: None,
            label_timestamp: None,
        }
    }

    /// r(user) -> a1(assistant) -> u2(user) -> {a3a(assistant), a3b(assistant)}
    /// plus l1(label entry) under u2.
    fn test_tree() -> Vec<SessionTreeNode> {
        let a3a = message_node(
            "a3a",
            Some("u2"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "branch one"}]}),
            Vec::new(),
        );
        let a3b = message_node(
            "a3b",
            Some("u2"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "branch two"}]}),
            Vec::new(),
        );
        let l1 = label_node("l1", Some("u2"));
        let u2 = message_node(
            "u2",
            Some("a1"),
            json!({"role": "user", "content": "second question"}),
            vec![a3a, a3b, l1],
        );
        let a1 = message_node(
            "a1",
            Some("r"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]}),
            vec![u2],
        );
        vec![message_node(
            "r",
            None,
            json!({"role": "user", "content": "hello world"}),
            vec![a1],
        )]
    }

    fn ids(list: &TreeList) -> Vec<&str> {
        list.filtered_nodes
            .iter()
            .map(|&f| list.node_id(f))
            .collect()
    }

    #[test]
    fn default_filter_hides_settings_entries_and_selects_leaf() {
        let list = TreeList::new(test_tree(), Some("a3a"), 10, None, None);
        // Label entry hidden; active branch (a3a) ordered before a3b.
        assert_eq!(ids(&list), vec!["r", "a1", "u2", "a3a", "a3b"]);
        assert_eq!(
            list.get_selected_entry().and_then(SessionEntry::id),
            Some("a3a")
        );
    }

    #[test]
    fn user_only_filter_keeps_user_messages() {
        let list = TreeList::new(
            test_tree(),
            Some("a3a"),
            10,
            None,
            Some(FilterMode::UserOnly),
        );
        assert_eq!(ids(&list), vec!["r", "u2"]);
    }

    #[test]
    fn all_filter_shows_label_entries() {
        let list = TreeList::new(test_tree(), Some("a3a"), 10, None, Some(FilterMode::All));
        assert_eq!(list.filtered_nodes.len(), 6);
    }

    #[test]
    fn typing_searches_and_escape_clears() {
        let mut list = TreeList::new(test_tree(), Some("a3a"), 10, None, None);
        for ch in "branch".chars() {
            list.handle_input(&ch.to_string());
        }
        assert_eq!(list.get_search_query(), "branch");
        assert_eq!(ids(&list), vec!["a3a", "a3b"]);

        // Escape clears the search instead of cancelling.
        let cancelled = Rc::new(RefCell::new(false));
        let flag = Rc::clone(&cancelled);
        list.on_cancel = Some(Box::new(move || *flag.borrow_mut() = true));
        list.handle_input("\x1b");
        assert!(!*cancelled.borrow());
        assert_eq!(list.get_search_query(), "");
        assert_eq!(list.filtered_nodes.len(), 5);

        // Second escape (no search active) cancels.
        list.handle_input("\x1b");
        assert!(*cancelled.borrow());
    }

    #[test]
    fn arrows_wrap_and_confirm_selects() {
        let mut list = TreeList::new(test_tree(), Some("a3b"), 10, None, None);
        assert_eq!(
            list.get_selected_entry().and_then(SessionEntry::id),
            Some("a3b")
        );
        // Active branch sorts first: order is r, a1, u2, a3b, a3a.
        list.handle_input("\x1b[B");
        assert_eq!(
            list.get_selected_entry().and_then(SessionEntry::id),
            Some("a3a")
        );
        // a3a is the last visible row — down wraps to the top.
        list.handle_input("\x1b[B");
        assert_eq!(
            list.get_selected_entry().and_then(SessionEntry::id),
            Some("r")
        );
        // Up wraps back to the bottom.
        list.handle_input("\x1b[A");
        assert_eq!(
            list.get_selected_entry().and_then(SessionEntry::id),
            Some("a3a")
        );

        let selected = Rc::new(RefCell::new(None::<String>));
        let sink = Rc::clone(&selected);
        list.on_select = Some(Box::new(move |id| *sink.borrow_mut() = Some(id.to_owned())));
        list.handle_input("\r");
        assert_eq!(selected.borrow().as_deref(), Some("a3a"));
    }

    #[test]
    fn copy_extracts_full_text() {
        let mut list = TreeList::new(test_tree(), Some("a3a"), 10, Some("r"), None);
        let copied = Rc::new(RefCell::new(None::<Option<String>>));
        let sink = Rc::clone(&copied);
        list.on_copy = Some(Box::new(move |text| *sink.borrow_mut() = Some(text)));
        list.copy_selected();
        assert_eq!(
            copied.borrow().clone(),
            Some(Some("hello world".to_owned()))
        );
    }

    #[test]
    fn update_node_label_sets_and_clears() {
        let mut list = TreeList::new(test_tree(), Some("a3a"), 10, None, None);
        list.update_node_label("u2", Some("important"), Some("2026-01-02T03:04:05.000Z"));
        let node = list.arena.iter().find(|n| n.id() == "u2").unwrap();
        assert_eq!(node.label.as_deref(), Some("important"));
        assert_eq!(
            node.label_timestamp.as_deref(),
            Some("2026-01-02T03:04:05.000Z")
        );
        list.update_node_label("u2", None, None);
        let node = list.arena.iter().find(|n| n.id() == "u2").unwrap();
        assert_eq!(node.label, None);
        assert_eq!(node.label_timestamp, None);
    }

    #[test]
    fn tool_call_formatting() {
        assert_eq!(
            format_tool_call(
                "read",
                &json!({"path": "/x/y.rs", "offset": 5, "limit": 10})
            ),
            "[read: /x/y.rs:5-14]"
        );
        assert_eq!(
            format_tool_call("bash", &json!({"command": "echo hi"})),
            "[bash: echo hi]"
        );
        assert_eq!(
            format_tool_call("grep", &json!({"pattern": "foo"})),
            "[grep: /foo/ in .]"
        );
        assert_eq!(format_tool_call("ls", &json!({})), "[ls: .]");
    }

    #[test]
    fn slice_by_column_preserves_ansi_and_strict_wide_chars() {
        // Plain slice.
        assert_eq!(slice_by_column("abcdef", 2, 3, true), "cde");
        // ANSI codes before the range are carried into the slice.
        let styled = "\x1b[31mabcdef\x1b[0m";
        let sliced = slice_by_column(styled, 2, 3, true);
        assert!(sliced.contains("cde"));
        assert!(sliced.starts_with("\x1b[31m"));
        // Strict mode drops a wide char that would straddle the boundary.
        assert_eq!(slice_by_column("a你b", 0, 2, true), "a");
    }

    #[test]
    fn compact_raw_keys_merges_shared_prefixes() {
        assert_eq!(
            compact_raw_keys(&["ctrl+left".to_owned(), "ctrl+right".to_owned()]),
            "ctrl+left/right"
        );
        assert_eq!(
            compact_raw_keys(&["ctrl+d".to_owned(), "alt+t".to_owned()]),
            "ctrl+d/alt+t"
        );
        assert_eq!(compact_raw_keys(&["up".to_owned()]), "up");
    }

    #[test]
    fn render_shows_status_line() {
        let mut list = TreeList::new(test_tree(), Some("a3a"), 10, None, None);
        let lines: Vec<String> = list.render(80).iter().map(Line::plain_text).collect();
        assert!(
            lines.last().is_some_and(|l| l.contains("(4/5)")),
            "status line should show selection position: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("user: hello world")));
    }
}
